// src/daemon.rs
//
// CHANGES vs current version:
//   - Engines now holds a Capabilities handle.
//   - Engines::init() builds the capability registry, registers built-ins,
//     and probes at startup. Probe results are logged so the user can see
//     at a glance what's available and what needs to be installed.
//   - Background tasks for registry refresh stay as-is.

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{error, info, warn};

use crate::engines::actions::executor::Executor;
use crate::engines::capabilities::Capabilities;
use crate::ipc::socket::{start_listener, ListenerConfig};
use crate::registry::AppRegistry;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("IPC listener failed: {0}")]
    Ipc(#[source] Box<dyn std::error::Error + Send + Sync>),

    #[error("engine initialization failed: {0}")]
    EngineInit(String),
}

/// Shared engine state. Cloneable; everything inside is Arc.
#[derive(Clone)]
pub struct Engines {
    pub executor: Arc<Executor>,
    pub apps: Arc<AppRegistry>,
    pub capabilities: Capabilities,
    pub handler_registry: Arc<crate::modes::HandlerRegistry>,
}

impl Engines {
    async fn init() -> Result<Self, DaemonError> {
        // App registry first — synchronous scan, then async probe.
        let apps = AppRegistry::scan_now()
            .map_err(|e| DaemonError::EngineInit(format!("AppRegistry: {e}")))?;
        let apps = Arc::new(apps);
        info!(apps_found = apps.all().len(), "App registry initialized");

        // Capability registry. Register all built-ins, probe each in parallel.
        let capabilities = Capabilities::new();
        let handler_registry = Arc::new(crate::modes::HandlerRegistry::with_default_handlers());


        capabilities.register_builtins().await;
        capabilities.probe_all().await;
        capabilities.report().await.log_summary();

        // Executor takes both registries — apps for fuzzy matching,
        // capabilities for system-target dispatch.
        let executor = Executor::new(apps.clone(), capabilities.clone(), handler_registry.clone());

        Ok(Self {
            executor: Arc::new(executor),
            apps,
            capabilities,
            handler_registry,
        })
    }
}

pub async fn run(
    socket_path: &str,
    shutdown_tx: broadcast::Sender<()>,
) -> Result<(), DaemonError> {
    info!("Initializing macagent engines...");
    let engines = Engines::init().await?;
    info!("Engines initialized.");
    let _ = crate::modes::snapshot::check_orphaned().await;
    
    let listener_cfg = ListenerConfig::default();

    info!(
        socket = %socket_path,
        max_conns = listener_cfg.max_concurrent_connections,
        "Starting IPC socket listener..."
    );

    let listener_shutdown = shutdown_tx.subscribe();
    let listener_handle = tokio::spawn({
        let socket_path = socket_path.to_string();
        let engines = engines.clone();
        async move {
            start_listener(&socket_path, engines, listener_cfg, listener_shutdown).await
        }
    });

    let mut shutdown_rx = shutdown_tx.subscribe();
    tokio::select! {
        res = listener_handle => {
            match res {
                Ok(Ok(())) => {
                    info!("IPC listener exited cleanly.");
                }
                Ok(Err(e)) => {
                    error!(error = %e, "IPC listener returned error");
                    let _ = shutdown_tx.send(());
                    return Err(DaemonError::Ipc(e));
                }
                Err(join_err) => {
                    error!(error = %join_err, "IPC listener task panicked");
                    let _ = shutdown_tx.send(());
                    return Err(DaemonError::Ipc(join_err.into()));
                }
            }
        }
        _ = shutdown_rx.recv() => {
            info!("Shutdown signal received. Draining engines...");
            match tokio::time::timeout(SHUTDOWN_GRACE, async {}).await {
                Ok(()) => info!("Drain completed."),
                Err(_) => warn!(
                    grace_secs = SHUTDOWN_GRACE.as_secs(),
                    "Drain exceeded grace period; exiting anyway"
                ),
            }
        }
    }

    Ok(())
}