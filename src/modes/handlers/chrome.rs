//! Google Chrome state handler.
//!
//! Capture: enumerates open Chrome windows and their tab URLs via
//! AppleScript. Filters out browser-internal URLs.
//!
//! Restore: launches Chrome (if not running), opens saved URLs in the
//! correct window/tab structure. The first saved window reuses Chrome's
//! default launch window; subsequent saved windows get new windows.
//!
//! AppleScript notes:
//!   - Chrome's dictionary uses "Google Chrome" as application name.
//!   - URLs are escaped with double quotes and backslashes only.
//!   - `make new tab at end of tabs of window N` appends in correct order.
//!   - `make new window with properties {URL: ...}` creates a window with
//!     a single tab at that URL.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use super::{AppStateHandler, HandlerError, Role};

const BUNDLE_ID: &str = "com.google.Chrome";
const NAME: &str = "Google Chrome";
const APP_NAME_AS: &str = "Google Chrome";  // What AppleScript uses

const APPLESCRIPT_TIMEOUT: Duration = Duration::from_secs(10);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(300);
const READINESS_MAX_ATTEMPTS: u32 = 17;  // ~5s
const INTER_TAB_DELAY: Duration = Duration::from_millis(80);

/// Window separator emitted by capture script.
const WINDOW_SEP: &str = "---";

#[derive(Debug, Serialize, Deserialize)]
struct ChromeState {
    windows: Vec<ChromeWindow>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ChromeWindow {
    tabs: Vec<String>,
}

pub struct ChromeHandler;

impl ChromeHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AppStateHandler for ChromeHandler {
    fn bundle_id(&self) -> &str {
        BUNDLE_ID
    }

    fn name(&self) -> &str {
        NAME
    }

    fn role(&self) -> Role {
        Role::Browser
    }

    async fn capture(&self) -> Result<Option<Value>, HandlerError> {
        let raw = run_applescript(ENUMERATE_TABS_SCRIPT).await?;
        let windows = parse_capture_output(&raw);

        if windows.is_empty() {
            debug!("Chrome has no windows or all tabs filtered out");
            return Ok(None);
        }

        debug!(
            window_count = windows.len(),
            tab_count = windows.iter().map(|w| w.tabs.len()).sum::<usize>(),
            "captured Chrome state"
        );

        let state = ChromeState { windows };
        Ok(Some(serde_json::to_value(state)?))
    }

    async fn restore(&self, state: &Value) -> Result<(), HandlerError> {
        let parsed: ChromeState = serde_json::from_value(state.clone())
            .map_err(|e| HandlerError::InvalidState(e.to_string()))?;

        if parsed.windows.is_empty() {
            return Ok(());
        }

        wait_for_chrome_ready().await?;

        for (window_idx, window) in parsed.windows.iter().enumerate() {
            if window.tabs.is_empty() {
                continue;
            }
            if let Err(e) = restore_window(window_idx, &window.tabs).await {
                warn!(
                    window_idx = window_idx,
                    error = %e,
                    "failed to restore Chrome window, continuing"
                );
            }
        }

        Ok(())
    }
}

// ---- AppleScript: capture all tab URLs grouped by window ----------------

const ENUMERATE_TABS_SCRIPT: &str = r#"
tell application "Google Chrome"
    set output to ""
    set firstWindow to true
    repeat with w in windows
        if not firstWindow then
            set output to output & "---" & linefeed
        end if
        set firstWindow to false
        repeat with t in tabs of w
            try
                set output to output & (URL of t) & linefeed
            end try
        end repeat
    end repeat
    return output
end tell
"#;

fn parse_capture_output(raw: &str) -> Vec<ChromeWindow> {
    let mut windows: Vec<ChromeWindow> = Vec::new();
    let mut current = ChromeWindow { tabs: Vec::new() };

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == WINDOW_SEP {
            if !current.tabs.is_empty() {
                windows.push(std::mem::replace(&mut current, ChromeWindow { tabs: Vec::new() }));
            }
            continue;
        }
        if !is_filtered(line) {
            current.tabs.push(line.to_string());
        }
    }
    // Push the final window if it has tabs.
    if !current.tabs.is_empty() {
        windows.push(current);
    }

    windows
}

/// Returns true if a URL should be skipped (browser-internal, local file,
/// extension page, javascript:, data:, etc).
fn is_filtered(url: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "chrome://",
        "chrome-extension://",
        "chrome-search://",
        "chrome-untrusted://",
        "javascript:",
        "data:",
        "file://",
        "about:",
        "edge://",
        "view-source:",
    ];
    let lower = url.to_lowercase();
    if PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return true;
    }
    // Special exact matches the prefix list might miss
    matches!(
        lower.as_str(),
        "about:blank" | "about:newtab" | "about:home" | ""
    )
}

// ---- Restore -----------------------------------------------------------

async fn wait_for_chrome_ready() -> Result<(), HandlerError> {
    for attempt in 0..READINESS_MAX_ATTEMPTS {
        let probe = run_applescript(
            r#"tell application "Google Chrome" to return count of windows"#,
        )
        .await;
        if probe.is_ok() {
            debug!(attempt = attempt + 1, "Chrome ready");
            return Ok(());
        }
        sleep(READINESS_POLL_INTERVAL).await;
    }
    Err(HandlerError::AppNotReady(NAME.to_string()))
}

/// Restore one saved window's tabs.
///
/// For window 0: target Chrome's existing window 1 (reuse the launch window).
/// For window N>0: create a new window with the first tab, then append the rest.
async fn restore_window(window_idx: usize, tabs: &[String]) -> Result<(), HandlerError> {
    if tabs.is_empty() {
        return Ok(());
    }

    if window_idx == 0 {
        // First saved window: navigate window 1's first tab to first URL,
        // then append the rest as new tabs.
        navigate_first_tab_of_window_1(&tabs[0]).await?;
        for url in &tabs[1..] {
            sleep(INTER_TAB_DELAY).await;
            append_tab_to_window(1, url).await?;
        }
    } else {
        // Subsequent windows: make a new window with the first URL,
        // then append remaining tabs to it.
        let new_window_index = make_new_window(&tabs[0]).await?;
        for url in &tabs[1..] {
            sleep(INTER_TAB_DELAY).await;
            append_tab_to_window(new_window_index, url).await?;
        }
    }
    Ok(())
}

/// Navigate the first tab of window 1 to a URL.
/// Used to reuse Chrome's default launch window for the first saved window.
async fn navigate_first_tab_of_window_1(url: &str) -> Result<(), HandlerError> {
    let escaped = escape_applescript_string(url);
    let script = format!(
        r#"tell application "Google Chrome"
    if (count of windows) is 0 then
        make new window
    end if
    tell window 1
        if (count of tabs) is 0 then
            make new tab with properties {{URL:"{url}"}}
        else
            set URL of active tab to "{url}"
        end if
    end tell
end tell"#,
        url = escaped
    );
    run_applescript(&script).await?;
    Ok(())
}

/// Append a new tab to the given window index. Returns nothing — failures
/// surface as HandlerError.
async fn append_tab_to_window(window_idx: usize, url: &str) -> Result<(), HandlerError> {
    let escaped = escape_applescript_string(url);
    let script = format!(
        r#"tell application "Google Chrome"
    tell window {idx}
        make new tab at end of tabs with properties {{URL:"{url}"}}
    end tell
end tell"#,
        idx = window_idx,
        url = escaped
    );
    run_applescript(&script).await?;
    Ok(())
}

/// Create a new Chrome window with the given URL. Returns the new window's
/// 1-based index, which we use for subsequent tab appends.
async fn make_new_window(url: &str) -> Result<usize, HandlerError> {
    let escaped = escape_applescript_string(url);
    // Chrome's `make new window` returns a window reference; we ask for
    // count after creation and assume our new window is the frontmost (1).
    // This is the documented behavior.
    let script = format!(
        r#"tell application "Google Chrome"
    make new window
    tell window 1
        set URL of active tab to "{url}"
    end tell
    return 1
end tell"#,
        url = escaped
    );
    let out = run_applescript(&script).await?;
    let idx: usize = out.trim().parse().unwrap_or(1);
    Ok(idx)
}

// ---- Utilities ---------------------------------------------------------

fn escape_applescript_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

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

// ---- Tests ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_window_two_tabs() {
        let raw = "https://example.com\nhttps://github.com\n";
        let windows = parse_capture_output(raw);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].tabs.len(), 2);
        assert_eq!(windows[0].tabs[0], "https://example.com");
        assert_eq!(windows[0].tabs[1], "https://github.com");
    }

    #[test]
    fn parse_two_windows() {
        let raw = "https://a.com\nhttps://b.com\n---\nhttps://c.com\n";
        let windows = parse_capture_output(raw);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].tabs.len(), 2);
        assert_eq!(windows[1].tabs.len(), 1);
        assert_eq!(windows[1].tabs[0], "https://c.com");
    }

    #[test]
    fn filters_chrome_internal() {
        let raw = "chrome://newtab\nhttps://example.com\nchrome-extension://abc/popup.html\n";
        let windows = parse_capture_output(raw);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].tabs.len(), 1);
        assert_eq!(windows[0].tabs[0], "https://example.com");
    }

    #[test]
    fn drops_window_with_only_filtered_urls() {
        let raw = "chrome://newtab\nabout:blank\n---\nhttps://example.com\n";
        let windows = parse_capture_output(raw);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].tabs[0], "https://example.com");
    }

    #[test]
    fn empty_input_no_windows() {
        assert!(parse_capture_output("").is_empty());
    }

    #[test]
    fn only_separators_no_windows() {
        assert!(parse_capture_output("---\n---\n").is_empty());
    }

    #[test]
    fn is_filtered_basic() {
        assert!(is_filtered("chrome://settings"));
        assert!(is_filtered("about:blank"));
        assert!(is_filtered("file:///Users/foo/bar.html"));
        assert!(is_filtered("javascript:void(0)"));
        assert!(!is_filtered("https://example.com"));
        assert!(!is_filtered("http://localhost:3000"));
    }

    #[test]
    fn escape_applescript_string_basic() {
        assert_eq!(escape_applescript_string("hello"), "hello");
        assert_eq!(escape_applescript_string(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_applescript_string(r"a\b"), r"a\\b");
    }
}