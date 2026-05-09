// src/engines/capabilities/runtime.rs
//
// Shared helpers used across capability backends:
//   - run_command: execute a CLI with timeout + stdout/stderr capture
//   - run_applescript: execute AppleScript via osascript
//   - which_exists: cheap PATH lookup for probe()
//
// All command execution uses tokio::process with kill_on_drop=true so a
// timeout actually kills the underlying process instead of leaving zombies.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use super::error::{CapError, CapResult};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Run an external CLI and return its stdout on success.
///
/// Behavior:
///   - On non-zero exit: returns CapError::External with stderr text
///   - On timeout: returns CapError::Timeout (process is killed)
///   - On spawn failure (e.g. binary not found): CapError::missing(...)
pub async fn run_command(program: &str, args: &[&str]) -> CapResult<String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    let child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CapError::missing(program.to_string())
        } else {
            CapError::external(format!("failed to spawn {}: {}", program, e))
        }
    })?;

    let output = match timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(CapError::external(format!("io error: {}", e))),
        Err(_) => return Err(CapError::Timeout(COMMAND_TIMEOUT)),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let code = output.status.code()
            .map(|c| format!("exit {}", c))
            .unwrap_or_else(|| "killed by signal".to_string());

        // Heuristic: if stderr mentions "not authorized" or "operation not
        // permitted", surface as PermissionDenied so the user knows what
        // setting to flip.
        let lower = stderr.to_lowercase();
        if lower.contains("not authorized")
            || lower.contains("operation not permitted")
            || lower.contains("automation")
        {
            return Err(CapError::permission(stderr));
        }

        return Err(CapError::external(format!(
            "{} {} ({}): {}",
            program,
            args.join(" "),
            code,
            stderr
        )));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run an AppleScript snippet via osascript.
///
/// The script is passed via `-e` so we don't need a temp file. For multi-line
/// scripts you can call this with a single string containing newlines —
/// osascript handles it.
pub async fn run_applescript(script: &str) -> CapResult<String> {
    run_command("osascript", &["-e", script]).await
}

/// Returns true if the named program exists on PATH and is executable.
/// Used in probe() to decide if a CLI-based capability is available.
pub async fn which_exists(program: &str) -> bool {
    // Try `which` first — it's everywhere on macOS.
    match run_command("which", &[program]).await {
        Ok(stdout) => !stdout.trim().is_empty(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_command_echo() {
        // Most macs have echo. If not, this test is the least of our problems.
        let out = run_command("echo", &["hello"]).await.unwrap();
        assert_eq!(out.trim(), "hello");
    }

    #[tokio::test]
    async fn run_command_missing_binary() {
        let r = run_command("definitely-not-a-real-binary-xyz", &[]).await;
        assert!(matches!(r, Err(CapError::MissingDependency { .. })));
    }

    #[tokio::test]
    async fn run_command_nonzero_exit() {
        // `false` exits 1 with no output.
        let r = run_command("false", &[]).await;
        assert!(matches!(r, Err(CapError::External(_))));
    }

    #[tokio::test]
    async fn which_finds_existing() {
        // sh is universal on macOS / Linux.
        assert!(which_exists("sh").await);
    }

    #[tokio::test]
    async fn which_misses_nonexistent() {
        assert!(!which_exists("xyzzy-blarg-12345").await);
    }
}