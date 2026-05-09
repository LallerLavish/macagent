//! Terminal.app state handler.
//!
//! Capture: enumerates open Terminal windows, finds each window's tty
//! device, looks up the leaf process (the user's shell or active foreground
//! command) on that tty via `ps`, then reads its CWD via `lsof`.
//!
//! Restore: opens N windows in Terminal.app, each cd'd to the saved CWD.
//!
//! Subtleties:
//!   - Terminal.app's AppleScript `tty` property returns "/dev/ttys001"
//!     style paths. We strip the "/dev/" prefix for `ps -t`.
//!   - `ps -t` returns multiple processes per tty (login shell, current
//!     foreground, etc). We want the deepest child — the one closest to
//!     what the user is "in." Use the highest PID as a heuristic.
//!   - `lsof -a -d cwd -p <pid>` is reliable but slow (~50-200ms per call).
//!     We run captures in parallel.
//!   - On restore, the first window already exists (Terminal opens one on
//!     launch). For window 2+, `do script "cd ..."` without an `in` clause
//!     opens a new window.

use async_trait::async_trait;
use futures::future::join_all;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use super::{AppStateHandler, HandlerError, Role};

const BUNDLE_ID: &str = "com.apple.Terminal";
const NAME: &str = "Terminal.app";
const APPLESCRIPT_TIMEOUT: Duration = Duration::from_secs(5);
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize, Deserialize)]
struct TerminalState {
    windows: Vec<TerminalWindow>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TerminalWindow {
    cwd: String,
}

pub struct TerminalAppHandler;

impl TerminalAppHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AppStateHandler for TerminalAppHandler {
    fn bundle_id(&self) -> &str {
        BUNDLE_ID
    }

    fn name(&self) -> &str {
        NAME
    }

    fn role(&self) -> Role {
        Role::Terminal
    }

    async fn capture(&self) -> Result<Option<Value>, HandlerError> {
        let ttys = enumerate_ttys().await?;
        if ttys.is_empty() {
            // Terminal is running but has no windows. Save no state.
            return Ok(None);
        }

        // Resolve each tty's CWD in parallel.
        let futures = ttys.into_iter().map(|tty| async move {
            match resolve_cwd_for_tty(&tty).await {
                Ok(Some(cwd)) => Some(TerminalWindow { cwd }),
                Ok(None) => {
                    warn!(tty = %tty, "no leaf process found for tty");
                    None
                }
                Err(e) => {
                    warn!(tty = %tty, error = %e, "cwd lookup failed");
                    None
                }
            }
        });
        let windows: Vec<TerminalWindow> = join_all(futures)
            .await
            .into_iter()
            .flatten()
            .collect();

        if windows.is_empty() {
            return Ok(None);
        }

        let state = TerminalState { windows };
        Ok(Some(serde_json::to_value(state)?))
    }

    async fn restore(&self, state: &Value) -> Result<(), HandlerError> {
        let parsed: TerminalState = serde_json::from_value(state.clone())
            .map_err(|e| HandlerError::InvalidState(e.to_string()))?;

        if parsed.windows.is_empty() {
            return Ok(());
        }

        // Wait for Terminal to be ready after launch. Poll for up to ~3s.
        wait_for_terminal_ready().await?;

        // Build a single AppleScript that opens all needed windows.
        // Window 1 already exists (Terminal opens one on launch).
        // For window 2+, use `do script` without `in` to spawn new windows.
        let mut script = String::from("tell application \"Terminal\"\n  activate\n");

        for (i, window) in parsed.windows.iter().enumerate() {
            let escaped_cwd = escape_applescript_string(&window.cwd);
            if i == 0 {
                script.push_str(&format!(
                    "  do script \"cd {}\" in window 1\n",
                    escaped_cwd
                ));
            } else {
                script.push_str(&format!(
                    "  do script \"cd {}\"\n",
                    escaped_cwd
                ));
            }
        }
        script.push_str("end tell\n");

        run_applescript(&script).await?;
        Ok(())
    }
}

// ---- AppleScript: enumerate open windows' ttys -------------------------

const ENUMERATE_TTYS_SCRIPT: &str = r#"
tell application "Terminal"
    set output to ""
    repeat with w in windows
        try
            set t to tty of selected tab of w
            set output to output & t & linefeed
        end try
    end repeat
    return output
end tell
"#;

async fn enumerate_ttys() -> Result<Vec<String>, HandlerError> {
    let stdout = run_applescript(ENUMERATE_TTYS_SCRIPT).await?;
    let ttys: Vec<String> = stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    debug!(count = ttys.len(), "enumerated terminal ttys");
    Ok(ttys)
}

// ---- CWD resolution via ps + lsof --------------------------------------

/// Given a tty path like "/dev/ttys001", find the leaf process and its CWD.
/// Returns Ok(None) if no process found on this tty.
async fn resolve_cwd_for_tty(tty: &str) -> Result<Option<String>, HandlerError> {
    // Strip "/dev/" prefix for ps -t
    let tty_short = tty.strip_prefix("/dev/").unwrap_or(tty);

    let pid = match find_leaf_pid(tty_short).await? {
        Some(p) => p,
        None => return Ok(None),
    };

    let cwd = lookup_cwd(pid).await?;
    Ok(Some(cwd))
}

/// Find the highest-PID process attached to the given tty. Heuristic for
/// "deepest child" (foreground command or shell).
async fn find_leaf_pid(tty_short: &str) -> Result<Option<i32>, HandlerError> {
    let output = timeout(
        SUBPROCESS_TIMEOUT,
        Command::new("ps")
            .arg("-t").arg(tty_short)
            .arg("-o").arg("pid=")
            .output(),
    )
    .await
    .map_err(|_| HandlerError::Subprocess(format!("ps -t {} timed out", tty_short)))?
    .map_err(|e| HandlerError::Subprocess(format!("ps spawn failed: {e}")))?;

    if !output.status.success() {
        // Empty tty — not an error, just nothing on it.
        return Ok(None);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pids: Vec<i32> = stdout
        .lines()
        .filter_map(|l| l.trim().parse::<i32>().ok())
        .collect();

    Ok(pids.into_iter().max())
}

/// Get the CWD of a process via lsof.
async fn lookup_cwd(pid: i32) -> Result<String, HandlerError> {
    let output = timeout(
        SUBPROCESS_TIMEOUT,
        Command::new("lsof")
            .arg("-a")
            .arg("-d").arg("cwd")
            .arg("-p").arg(pid.to_string())
            .arg("-Fn")  // formatted output, "n" lines = name (path)
            .output(),
    )
    .await
    .map_err(|_| HandlerError::Subprocess(format!("lsof for pid {} timed out", pid)))?
    .map_err(|e| HandlerError::Subprocess(format!("lsof spawn failed: {e}")))?;

    if !output.status.success() {
        return Err(HandlerError::Subprocess(format!(
            "lsof failed for pid {}: {}",
            pid,
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    // -Fn output looks like:
    //   p12345
    //   n/Users/lavish/code/macagent
    // We want the line starting with 'n'.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(path) = line.strip_prefix('n') {
            return Ok(path.to_string());
        }
    }

    Err(HandlerError::Subprocess(format!(
        "no cwd line in lsof output for pid {}",
        pid
    )))
}

// ---- Restore helpers ---------------------------------------------------

/// Wait until Terminal.app is responsive to AppleScript. Polls every 250ms
/// for up to ~3s. Necessary because `open -b` returns before the app has
/// finished initializing.
async fn wait_for_terminal_ready() -> Result<(), HandlerError> {
    const POLL_INTERVAL: Duration = Duration::from_millis(250);
    const MAX_ATTEMPTS: u32 = 12;  // 12 * 250ms = 3s

    for attempt in 0..MAX_ATTEMPTS {
        let ready = run_applescript_silent(
            r#"tell application "Terminal" to return count of windows"#,
        )
        .await
        .is_ok();

        if ready {
            debug!(attempt = attempt + 1, "Terminal ready");
            return Ok(());
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    Err(HandlerError::AppNotReady(NAME.to_string()))
}

/// Escape a string for safe inclusion inside an AppleScript double-quoted
/// literal. Only " and \ need escaping inside ASL strings.
fn escape_applescript_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

// ---- AppleScript runners -----------------------------------------------

async fn run_applescript(script: &str) -> Result<String, HandlerError> {
    let output = timeout(
        APPLESCRIPT_TIMEOUT,
        Command::new("osascript").arg("-e").arg(script).output(),
    )
    .await
    .map_err(|_| HandlerError::AppleScript("osascript timed out".to_string()))?
    .map_err(|e| HandlerError::AppleScript(format!("osascript spawn failed: {e}")))?;

    if !output.status.success() {
        return Err(HandlerError::AppleScript(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Same as run_applescript but discards output and only returns success/failure.
/// Used for readiness probing where we don't care about the value.
async fn run_applescript_silent(script: &str) -> Result<(), HandlerError> {
    run_applescript(script).await.map(|_| ())
}