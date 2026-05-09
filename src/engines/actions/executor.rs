// src/engines/actions/executor.rs
//
// Refactor: system-target dispatch now goes through the Capability framework.
// Apps and system actions (Sleep/Restart/Shutdown/CloseAll) keep their
// existing handlers because they don't fit the on/off-or-numeric-value pattern.
//
// What changed:
//   - Executor struct gained a `capabilities: Capabilities` field
//   - execute() routes binary targets (WiFi/Bluetooth/DarkMode/DND) and
//     analog targets (Volume/Brightness/KeyboardBrightness) through the
//     capability registries
//   - Removed: wifi(), bluetooth_settings(), volume_*, screen_brightness_*,
//     keyboard_brightness_*, adjust_brightness, current/set_brightness_percent,
//     wifi_interface() and friends, BRIGHTNESS_BIN
//   - Kept: open_app, close_app, close_all, shutdown, restart, sleep,
//     sanitize_app_name, run_with_timeout, run_applescript, ExecError variants,
//     destructive-action confirmation gating

use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::process::Command;
use tokio::time::timeout;

use crate::modes::ModeError;
use crate::engines::actions::intent::{Action, Confidence, Intent, Target};
use crate::engines::capabilities::{CapError, Capabilities};
use crate::registry::{AppRegistry, ResolutionResult};

/// Default deadline for any single external process. Longer than every
/// healthy call, short enough to recover from a stuck one.
const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

/// Longer timeout for `close_all`, which iterates over every running app.
const CLOSE_ALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Default analog adjustment step when no explicit amount is given.
const DEFAULT_STEP: i32 = 10;

#[derive(Debug, Error)]
pub enum ExecError {
    #[error("don't know how to {action:?} on {target:?}")]
    Unsupported { action: Action, target: Target },

    #[error("command requires confirmation: {0}")]
    NeedsConfirmation(String),

    #[error("invalid app name: {0}")]
    InvalidAppName(String),

    #[error("system call failed: {0}")]
    System(String),

    #[error("operation timed out after {0:?}")]
    Timeout(Duration),

    #[error("ambiguous app name: {0}")]
    AmbiguousAppName(String),

    /// Surfaces capability-layer failures (missing dependency, permission
    /// denied, etc.) to the user with their stable kind tags.
    #[error("capability error: {0}")]
    Capability(#[from] CapError),
    
    #[error("mode error: {0}")]
    Mode(#[from] ModeError),
}

impl ExecError {
    pub fn kind_str(&self) -> &'static str {
        match self {
            ExecError::Timeout(_)             => "Timeout",
            ExecError::NeedsConfirmation(_)   => "NeedsConfirmation",
            ExecError::InvalidAppName(_)      => "InvalidAppName",
            ExecError::AmbiguousAppName(_)    => "AmbiguousAppName",
            ExecError::Unsupported { .. }     => "Unsupported",
            ExecError::System(_)              => "System",
            
            // Forward the capability's own kind tag so clients can branch
            // on MissingDependency / PermissionDenied / etc. without us
            // having to enumerate them all here.
            ExecError::Capability(c)          => c.kind_str(),
            ExecError::Mode(_)                => "Mode",
        }
    }
}

pub type ExecResult = Result<String, ExecError>;

// ============================================================
// Executor
// ============================================================

pub struct Executor {
    pub require_confirmation: bool,
    pub apps: Arc<AppRegistry>,
    pub capabilities: Capabilities,
    pub handler_registry: Arc<crate::modes::HandlerRegistry>,
}


impl Executor {
    pub fn new(apps: Arc<AppRegistry>, capabilities: Capabilities,handler_registry:Arc<crate::modes::HandlerRegistry>) -> Self {
        Self {
            require_confirmation: true,
            apps,
            capabilities,
            handler_registry,
        }
    }

    pub async fn execute(&self, intent: Intent) -> ExecResult {
        // Confirmation gating for destructive low-confidence intents.
        // The IPC layer can re-send with confidence forced to High after
        // the user confirms via the UI.
        if self.require_confirmation
            && intent.confidence == Confidence::Low
            && is_destructive(&intent.action)
        {
            return Err(ExecError::NeedsConfirmation(format!(
                "Did you mean to {}? Original: {:?}",
                intent, intent.raw
            )));
        }

        let step = intent.amount.unwrap_or(DEFAULT_STEP);

        match (intent.action, &intent.target) {
            // ----- App actions -------------------------------------------
            (Action::Open, Target::App(name)) | (Action::Close, Target::App(name)) => {
                let canonical = match self.apps.resolve(name) {
                    ResolutionResult::Confident { canonical, .. } => canonical,
                    ResolutionResult::Ambiguous { candidates } => {
                        let names: Vec<String> = candidates.into_iter().map(|(n, _)| n).collect();
                        return Err(ExecError::AmbiguousAppName(format!(
                            "ambiguous app name '{}', candidates: {}",
                            name,
                            names.join(", ")
                        )));
                    }
                    ResolutionResult::NotFound => {
                        return Err(ExecError::InvalidAppName(format!(
                            "no app found with name '{}'",
                            name
                        )));
                    }
                };

                if intent.action == Action::Open {
                    self.open_app(&canonical).await
                } else {
                    self.close_app(&canonical).await
                }
            }
            (Action::CloseAll, _) => self.close_all().await,

            // ----- System-level (no target) ------------------------------
            (Action::Shutdown, _) => self.shutdown().await,
            (Action::Restart, _)  => self.restart().await,
            (Action::Sleep, _)    => self.sleep().await,

            // ----- Binary capabilities -----------------------------------
            (Action::TurnOn, target) if is_binary_target(target) => {
                self.binary_turn_on(target).await
            }
            (Action::TurnOff, target) if is_binary_target(target) => {
                self.binary_turn_off(target).await
            }
            (Action::Toggle, target) if is_binary_target(target) => {
                self.binary_toggle(target).await
            }

            // ----- Analog capabilities -----------------------------------
            (Action::Set, target) if is_analog_target(target) => {
                self.analog_set(target, step).await
            }
            (Action::Increase, target) if is_analog_target(target) => {
                self.analog_adjust(target, step).await
            }
            (Action::Decrease, target) if is_analog_target(target) => {
                self.analog_adjust(target, -step).await
            }

            // Volume mute toggle is special: TurnOff/Toggle on Volume means
            // mute, not "set to 0". We handle that via the analog backend's
            // set(0) for now; future work could add a dedicated mute op.
            (Action::Toggle, Target::Volume) | (Action::TurnOff, Target::Volume) => {
                self.analog_set(&Target::Volume, 0).await
            }
            // ----- Mode commands ----------------------------------------
            (Action::ModeSave, Target::Mode(name)) => {
                let result = crate::modes::save_mode(
                    name,
                    &self.handler_registry,
                    &self.capabilities,    // ← new
                )
                .await
                .map_err(|e| ExecError::System(e.to_string()))?;
                Ok(serde_json::to_string(&result).unwrap())
            }
            
            (Action::SummaryDebug, Target::Mode(name)) => {
                let text = crate::modes::summary::build_summary_text(name)
                    .await
                    .map_err(|e| ExecError::System(e.to_string()))?;
                Ok(serde_json::to_string(&serde_json::json!({ "summary_input": text })).unwrap())
            }

            (Action::ModeSwitch, Target::Mode(name)) => {
                let result = crate::modes::switch_to_mode(
                    name,
                    self.handler_registry.clone(),
                    &self.capabilities,    // ← new
                    |_| async { true },
                )
                .await
                .map_err(|e| ExecError::System(e.to_string()))?;
                Ok(serde_json::to_string(&result).unwrap())
            }

            (Action::ModeList, _) => {
                let names = crate::modes::list_modes()
                    .await
                    .map_err(|e| ExecError::System(e.to_string()))?;
                Ok(serde_json::to_string(&serde_json::json!({ "modes": names })).unwrap())
            }

            (Action::ModeDelete, Target::Mode(name)) => {
                crate::modes::delete_mode(name)
                    .await
                    .map_err(|e| ExecError::System(e.to_string()))?;
                Ok(serde_json::to_string(&serde_json::json!({ "deleted": name })).unwrap())
            }
            (Action::ModeExit, Target::Mode(_name)) => {
                let result = crate::modes::exit_mode(
                    &self.capabilities,
                    self.handler_registry.clone(),
                )
                .await
                .map_err(|e| ExecError::System(e.to_string()))?;
                Ok(serde_json::to_string(&result).unwrap())
            }

            // ----- Catch-all --------------------------------------------
            (action, target) => Err(ExecError::Unsupported {
                action,
                target: target.clone(),
            }),
        }
    }

    // ============================================================
    // Capability dispatch helpers
    // ============================================================

    async fn binary_turn_on(&self, target: &Target) -> ExecResult {
        let cap = self.resolve_binary(target).await?;
        cap.turn_on().await?;
        Ok(format!("{:?} turned on via {}", target, cap.id()))
    }

    async fn binary_turn_off(&self, target: &Target) -> ExecResult {
        let cap = self.resolve_binary(target).await?;
        cap.turn_off().await?;
        Ok(format!("{:?} turned off via {}", target, cap.id()))
    }

    async fn binary_toggle(&self, target: &Target) -> ExecResult {
        let cap = self.resolve_binary(target).await?;
        cap.toggle().await?;
        Ok(format!("{:?} toggled via {}", target, cap.id()))
    }

    async fn analog_set(&self, target: &Target, value: i32) -> ExecResult {
        let cap = self.resolve_analog(target).await?;
        cap.set(value).await?;
        Ok(format!("{:?} set to {} via {}", target, value, cap.id()))
    }

    async fn analog_adjust(&self, target: &Target, delta: i32) -> ExecResult {
        let cap = self.resolve_analog(target).await?;
        cap.adjust(delta).await?;
        let direction = if delta >= 0 { "increased" } else { "decreased" };
        Ok(format!(
            "{:?} {} by {} via {}",
            target,
            direction,
            delta.abs(),
            cap.id()
        ))
    }

    async fn resolve_binary(
        &self,
        target: &Target,
    ) -> Result<Arc<dyn crate::engines::capabilities::BinaryCapability>, ExecError> {
        let registry = self.capabilities.binary.read().await;
        registry.resolve(target).ok_or_else(|| {
            ExecError::Capability(CapError::Unavailable(format!(
                "no working capability for {:?} — install one of the suggested tools",
                target
            )))
        })
    }

    async fn resolve_analog(
        &self,
        target: &Target,
    ) -> Result<Arc<dyn crate::engines::capabilities::AnalogCapability>, ExecError> {
        let registry = self.capabilities.analog.read().await;
        registry.resolve(target).ok_or_else(|| {
            ExecError::Capability(CapError::Unavailable(format!(
                "no working capability for {:?} — install one of the suggested tools",
                target
            )))
        })
    }

    // ============================================================
    // App handlers (unchanged)
    // ============================================================

    async fn open_app(&self, name: &str) -> ExecResult {
        let name = sanitize_app_name(name)?;
        run_with_timeout(
            Command::new("open").args(["-a", &name]),
            PROCESS_TIMEOUT,
        )
        .await?;
        Ok(format!("Opened {name}"))
    }

    async fn close_app(&self, name: &str) -> ExecResult {
        let name = sanitize_app_name(name)?;
        let script = format!(
            r#"with timeout of 3 seconds
                tell application "{}" to quit
            end timeout"#,
            name
        );
        run_applescript(&script, PROCESS_TIMEOUT).await?;
        Ok(format!("Closed {name}"))
    }

    async fn close_all(&self) -> ExecResult {
        let script = r#"
            tell application "System Events"
                set activeApps to name of application processes whose background only is false
            end tell
            repeat with appName in activeApps
                if appName is not "Finder" and appName is not "macagent" then
                    try
                        with timeout of 2 seconds
                            tell application (appName as text) to quit
                        end timeout
                    end try
                end if
            end repeat
        "#;
        run_applescript(script, CLOSE_ALL_TIMEOUT).await?;
        Ok("Closed all foreground applications".into())
    }

    // ============================================================
    // System actions (unchanged)
    // ============================================================

    async fn shutdown(&self) -> ExecResult {
        run_applescript(
            r#"do shell script "shutdown -h now" with administrator privileges"#,
            PROCESS_TIMEOUT,
        )
        .await?;
        Ok("Shutting down".into())
    }

    async fn restart(&self) -> ExecResult {
        run_applescript(
            r#"do shell script "shutdown -r now" with administrator privileges"#,
            PROCESS_TIMEOUT,
        )
        .await?;
        Ok("Restarting".into())
    }

    async fn sleep(&self) -> ExecResult {
        run_applescript(
            r#"tell application "Finder" to sleep"#,
            PROCESS_TIMEOUT,
        )
        .await?;
        Ok("Going to sleep".into())
    }
}

// ============================================================
// Helpers (unchanged from your hardened version)
// ============================================================

fn is_destructive(action: &Action) -> bool {
    matches!(
        action,
        Action::Shutdown | Action::Restart | Action::CloseAll | Action::Close
    )
}

fn is_binary_target(target: &Target) -> bool {
    matches!(
        target,
        Target::WiFi | Target::Bluetooth | Target::DarkMode | Target::DoNotDisturb
    )
}

fn is_analog_target(target: &Target) -> bool {
    matches!(
        target,
        Target::ScreenBrightness | Target::KeyboardBrightness | Target::Volume
    )
}

fn sanitize_app_name(name: &str) -> Result<String, ExecError> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return Err(ExecError::InvalidAppName(name.to_string()));
    }
    if trimmed.contains('"') || trimmed.contains('\\') || trimmed.contains('\n') {
        return Err(ExecError::InvalidAppName(name.to_string()));
    }
    Ok(trimmed.to_string())
}

/// All external commands go through here so they share one timeout +
/// kill-on-timeout policy. Non-blocking — uses tokio's Command and timeout
/// so the daemon's runtime is never starved.
async fn run_with_timeout(
    cmd: &mut Command,
    deadline: Duration,
) -> Result<std::process::Output, ExecError> {
    cmd.kill_on_drop(true);
    let fut = cmd.output();
    match timeout(deadline, fut).await {
        Err(_) => Err(ExecError::Timeout(deadline)),
        Ok(Err(e)) => Err(ExecError::System(e.to_string())),
        Ok(Ok(out)) => {
            if !out.status.success() {
                return Err(ExecError::System(
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ));
            }
            Ok(out)
        }
    }
}

async fn run_applescript(script: &str, deadline: Duration) -> Result<(), ExecError> {
    run_with_timeout(
        Command::new("osascript").arg("-e").arg(script),
        deadline,
    )
    .await?;
    Ok(())
}