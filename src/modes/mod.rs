pub mod capture;
pub mod definition;
pub mod error;
pub mod exit;
pub mod handlers;
pub mod restore;
pub mod snapshot;
pub mod storage;
pub mod system_state;
pub mod summary;

use chrono::Utc;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{info, warn};

pub use definition::{Mode, AppPlan, SystemPlan, ProjectRef, ActivitySnapshot};
pub use error::ModeError;
pub use exit::{exit_mode, ExitResult};
pub use handlers::HandlerRegistry;

use crate::engines::capabilities::Capabilities;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SwitchResult {
    pub apps_closed: Vec<String>,
    pub apps_launched: Vec<String>,
    pub apps_restored: Vec<String>,
    pub apps_skipped_dirty: Vec<String>,
    pub system_fields_applied: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SaveResult {
    pub name: String,
    pub apps_captured: usize,
    pub apps_with_state: usize,
    pub system_fields_captured: usize,
}

pub async fn save_mode(
    name: &str,
    registry: &HandlerRegistry,
    caps: &Capabilities,
) -> Result<SaveResult, ModeError> {
    definition::validate_name(name)?;

    if storage::exists(name).await? {
        warn!(mode = %name, "overwriting existing mode");
    }

    let apps = capture::apps::running_user_apps().await?;
    let count = apps.len();

    // Per-app state via handlers
    let mut state_map: HashMap<String, serde_json::Value> = HashMap::new();
    for app in &apps {
        let handler = registry.for_bundle_id(&app.bundle_id);
        match handler.capture().await {
            Ok(Some(state)) => { state_map.insert(app.bundle_id.clone(), state); }
            Ok(None) => {}
            Err(e) => {
                warn!(
                    bundle_id = %app.bundle_id,
                    handler = %handler.name(),
                    error = %e,
                    "state capture failed"
                );
            }
        }
    }

    let sys_state = system_state::capture_system(caps).await;
    let system = SystemPlan::from_state(&sys_state);
    let with_state_count = state_map.len();
    let sys_count = system.to_state().populated_fields().len();

    let mode = Mode {
        name: name.to_string(),
        created: Utc::now(),
        updated: Utc::now(),
        apps: AppPlan {
            launch: apps.into_iter().map(|a| a.bundle_id).collect(),
            close_others: true,
            state: state_map,
        },
        system,
        projects: Vec::new(),
        activity_snapshot: None,
    };

    storage::write(&mode).await?;
    info!(
        mode = %name,
        apps = count,
        with_state = with_state_count,
        system_fields = sys_count,
        "mode saved"
    );

    Ok(SaveResult {
        name: name.to_string(),
        apps_captured: count,
        apps_with_state: with_state_count,
        system_fields_captured: sys_count,
    })
}

pub async fn switch_to_mode<F, Fut>(
    name: &str,
    registry: Arc<HandlerRegistry>,
    caps: &Capabilities,
    confirm_close_dirty: F,
) -> Result<SwitchResult, ModeError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    // Q3=a: refuse if already in work mode
    if snapshot::exists().await? {
        return Err(ModeError::AlreadyInMode);
    }

    let mode = storage::read(name).await?;

    // Take snapshot BEFORE any changes — pre-work state.
    let pre_apps = capture::apps::running_user_apps().await?;
    let pre_system = system_state::capture_system(caps).await;
    let snap = snapshot::PreWorkSnapshot {
        captured_at: Utc::now(),
        mode_name: name.to_string(),
        apps: snapshot::SnapshotApps {
            running: pre_apps.iter().map(|a| a.bundle_id.clone()).collect(),
        },
        system: pre_system,
    };
    snapshot::write(&snap).await?;
    info!("pre-work snapshot taken");

    // ---- Now do the switch (mostly unchanged from M2.3) -----------------

    let target_set: HashSet<String> = mode.apps.launch.iter().cloned().collect();

    let mut to_close = Vec::new();
    if mode.apps.close_others {
        for app in &pre_apps {
            if !target_set.contains(&app.bundle_id) {
                to_close.push(app.clone());
            }
        }
    }

    let mut safe_to_close = Vec::new();
    let mut needs_prompt = Vec::new();
    for app in to_close {
        if restore::close::is_document_based(&app.bundle_id) {
            needs_prompt.push(app);
        } else {
            safe_to_close.push(app);
        }
    }

    let mut skipped = Vec::new();
    for app in needs_prompt {
        if confirm_close_dirty(app.name.clone()).await {
            safe_to_close.push(app);
        } else {
            skipped.push(app.name);
        }
    }

    let closed = restore::close::close_apps(&safe_to_close).await?;
    let launched = restore::launch::launch_apps(&mode.apps.launch).await?;

    // Restore per-app state
    let mut restored = Vec::new();
    for bundle_id in &launched {
        if let Some(state) = mode.apps.state.get(bundle_id) {
            let handler = registry.for_bundle_id(bundle_id);
            if let Err(e) = handler.restore(state).await {
                warn!(bundle_id = %bundle_id, error = %e, "state restore failed");
            } else {
                restored.push(bundle_id.clone());
            }
        }
    }

    // Apply system state from mode
    let target_system = mode.system.to_state();
    let system_applied = system_state::apply_system(caps, &target_system).await;
    let mode_name_for_summary = name.to_string();
    tokio::spawn(async move {
        match summary::generate_and_deliver(&mode_name_for_summary).await {
            Ok(path) => info!(?path, "summary delivered"),
            Err(e) => warn!(error = %e, "summary generation failed"),
        }
    });

    info!(
        mode = %name,
        closed = closed.len(),
        launched = launched.len(),
        restored = restored.len(),
        system_applied = system_applied.len(),
        "mode switch complete"
    );

    Ok(SwitchResult {
        apps_closed: closed,
        apps_launched: launched,
        apps_restored: restored,
        apps_skipped_dirty: skipped,
        system_fields_applied: system_applied.into_iter().map(String::from).collect(),
    })
}

pub async fn list_modes() -> Result<Vec<String>, ModeError> {
    storage::list().await
}

pub async fn delete_mode(name: &str) -> Result<(), ModeError> {
    storage::delete(name).await?;
    info!(mode = %name, "mode deleted");
    Ok(())
}