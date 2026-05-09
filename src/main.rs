// src/main.rs
//
// Entry point. Stays small on purpose — all real logic is in daemon::run.
//
// Responsibilities here:
//   1. Initialize tracing.
//   2. Resolve the socket path (per-UID, under a 0700 parent dir).
//   3. Set umask to 0077 *before* the listener binds, so the bind/chmod
//      TOCTOU window is closed.
//   4. Install SIGTERM/SIGINT handlers that broadcast a single shutdown.
//   5. Hand off to daemon::run and report its exit status to launchd.

use std::path::PathBuf;
use std::process::ExitCode;

use tokio::signal::unix::{signal, SignalKind};
use tokio::sync::broadcast;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

mod daemon;
mod engines;
mod ipc;
mod platform;
mod registry;
mod pipeline;
mod modes;

fn main() -> ExitCode {
    // Build a multi-thread runtime explicitly so the worker count is
    // predictable and we can tune it from config later.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("macagent-worker")
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to build tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };

    runtime.block_on(async_main())
}

async fn async_main() -> ExitCode {
    init_tracing();

    let socket_path = match resolve_socket_path() {
        Ok(p) => p,
        Err(e) => {
            error!(error = %e, "failed to resolve socket path");
            return ExitCode::from(2);
        }
    };

    // Tighten umask before any file creation so the socket and its parent
    // dir are private from the moment they exist.
    let _umask_guard = platform::macos::umask::tighten(0o077);

    if let Err(e) = ensure_socket_parent_dir(&socket_path) {
        error!(error = %e, "failed to prepare socket parent dir");
        return ExitCode::from(3);
    }

    let (shutdown_tx, _) = broadcast::channel::<()>(8);

    // Signal handling: SIGTERM (launchd stop), SIGINT (Ctrl-C in dev),
    // SIGHUP (reserved for future config reload — currently ignored).
    spawn_signal_handlers(shutdown_tx.clone());

    info!(socket = %socket_path.display(), "macagent starting");

    let socket_str = match socket_path.to_str() {
        Some(s) => s,
        None => {
            error!("socket path is not valid UTF-8");
            return ExitCode::from(4);
        }
    };

    match daemon::run(socket_str, shutdown_tx).await {
        Ok(()) => {
            info!("macagent exited cleanly");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = %e, "macagent exited with error");
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("MACAGENT_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

fn resolve_socket_path() -> std::io::Result<PathBuf> {
    let uid = unsafe { libc::getuid() };
    Ok(PathBuf::from(format!("/tmp/macagent-{}/macagent.sock", uid)))
}

fn ensure_socket_parent_dir(socket_path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = socket_path
        .parent()
        .ok_or_else(|| std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "socket path has no parent",
        ))?;

    std::fs::create_dir_all(parent)?;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(parent, perms)?;
    Ok(())
}

fn spawn_signal_handlers(shutdown_tx: broadcast::Sender<()>) {
    tokio::spawn(async move {
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to install SIGTERM handler");
                return;
            }
        };
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to install SIGINT handler");
                return;
            }
        };

        tokio::select! {
            _ = sigterm.recv() => info!("SIGTERM received"),
            _ = sigint.recv()  => info!("SIGINT received"),
        }

        let _ = shutdown_tx.send(());
    });
}