//! Exit work mode — restore pre-work state from the snapshot.

use std::collections::HashSet;
use std::sync::Arc;
use tracing::{info, warn};

use crate::engines::capabilities::Capabilities;

use super::capture::apps::running_user_apps;
use super::error::ModeError;
use super::handlers::HandlerRegistry;
use super::restore::{close, launch};
use super::snapshot::{self, PreWorkSnapshot};
use super::system_state::apply_system;

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExitResult {
    pub apps_closed: Vec<String>,
    pub apps_relaunched: Vec<String>,
    pub system_fields_reverted: Vec<String>,
}

pub async fn exit_mode(
    caps: &Capabilities,
    _registry: Arc<HandlerRegistry>,
) -> Result<ExitResult, ModeError> {
    if !snapshot::exists().await? {
        return Err(ModeError::NotFound("no active work mode".into()));
    }

    let snap: PreWorkSnapshot = snapshot::read().await?;
    info!(mode = %snap.mode_name, "exiting work mode");

    // 1. Compute app diffs
    //    work_opened = currently running but NOT in pre-work running
    //                  (these were opened by work mode → close them)
    //    work_closed = in pre-work running but NOT currently running
    //                  (these were closed by work mode → reopen them)
    //
    //    Per Q2=B: we DON'T close apps the user manually opened during
    //    work mode. To respect that, we'd need to know which apps work
    //    mode opened vs which the user opened mid-session. We don't track
    //    this. Approximation: assume any app currently running that wasn't
    //    pre-work is fair game to close. This conflicts with Q2=B.
    //
    //    Pragmatic compromise: only close apps that were in the work
    //    mode's `apps.launch` list. That way Discord (user-opened) stays;
    //    Chrome (work-mode-opened) goes.
    //
    //    But we don't have the work mode here — the snapshot only knows
    //    pre-state, not the mode definition. Solution: snapshot also
    //    stores the mode's launch list, so we know what work mode tried
    //    to open.

    let pre_set: HashSet<String> = snap.apps.running.iter().cloned().collect();
    let currently_running = running_user_apps().await?;

    // Apps work mode opened that we should close on exit.
    // M2.4 v1 simplification: close currently-running apps that weren't
    // in pre-state. User-opened-during-work apps will get caught here too.
    // This is a known wart we'll fix when we add launch-list tracking
    // to the snapshot in a later milestone.
    let to_close: Vec<_> = currently_running
        .iter()
        .filter(|app| !pre_set.contains(&app.bundle_id))
        .cloned()
        .collect();

    // Apps that were running pre-work but aren't running now → relaunch.
    let currently_running_set: HashSet<String> =
        currently_running.iter().map(|a| a.bundle_id.clone()).collect();
    let to_relaunch: Vec<String> = snap.apps.running
        .iter()
        .filter(|bid| !currently_running_set.contains(*bid))
        .cloned()
        .collect();

    // 2. Execute close + launch
    let closed = close::close_apps(&to_close).await?;
    let relaunched = launch::launch_apps(&to_relaunch).await?;

    // 3. Revert system state
    let reverted = apply_system(caps, &snap.system).await;


    if let Err(e) = crate::modes::summary::save_baseline_for_mode(&snap.mode_name).await {
        warn!(error = %e, "failed to save git baseline");
    }
    
    // 4. Discard snapshot
    snapshot::delete().await?;

    info!(
        closed = closed.len(),
        relaunched = relaunched.len(),
        system_reverted = reverted.len(),
        "work mode exit complete"
    );

    Ok(ExitResult {
        apps_closed: closed,
        apps_relaunched: relaunched,
        system_fields_reverted: reverted.into_iter().map(String::from).collect(),
    })
}