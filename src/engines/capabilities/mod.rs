// src/engines/capabilities/mod.rs
//
// Capability framework: the core of system-control dispatch.
//
// Two trait families:
//
//   BinaryCapability — for on/off/toggle state (Wi-Fi, Bluetooth, dark mode, DND).
//   AnalogCapability — for numeric values (volume, brightness).
//
// Two corresponding registries that hold backends and resolve them by Target.
// Each registry maintains one "active" capability per target (the highest-
// priority one whose probe returned available).
//
// Adding a new capability:
//   1. Implement BinaryCapability or AnalogCapability for a struct.
//   2. Register it once at startup via Registry::register.
//   3. Done. The executor dispatches to it automatically.

mod actions;
mod dispatch;
mod error;
mod runtime;

// Public submodules — built-in capabilities live here.
pub mod wifi;
pub mod bluetooth;
pub mod dark_mode;
pub mod dnd;
pub mod volume;
pub mod brightness;

pub use actions::{AnalogAction, BinaryAction, TriggerAction};
pub use dispatch::{dispatch_safe, dispatch_safe_with, DEFAULT_TIMEOUT};
pub use error::{CapError, CapResult};
pub use runtime::{run_applescript, run_command, which_exists};

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::engines::actions::intent::Target;

// ---- Binary capability trait --------------------------------------------

/// A capability whose primary state is on/off (Wi-Fi, Bluetooth, dark mode).
#[async_trait]
pub trait BinaryCapability: Send + Sync {
    /// Stable identifier — "core::wifi::networksetup", "user::my_thing", etc.
    /// Used for logs and for resolving conflicts when multiple backends
    /// register for the same target at the same priority.
    fn id(&self) -> &str;

    /// Which Target enum variant this capability acts on.
    fn target(&self) -> Target;

    /// Higher = preferred. Defaults to 50 ("normal"). Use 100+ for native
    /// FFI, 50 for stable CLIs, 10 for last-resort fallbacks.
    fn priority(&self) -> i32 { 50 }

    /// Human-readable hint shown when this capability's dependency is
    /// missing. E.g. "install with: brew install blueutil".
    fn install_hint(&self) -> Option<&str> { None }

    /// Probe: is this capability actually usable on this system?
    /// Called once at startup. Failures are non-fatal.
    /// Implementations should be cheap (which-check, version probe, etc.)
    /// and must complete within DEFAULT_TIMEOUT.
    async fn is_available(&self) -> bool;

    /// Switch on. Should be idempotent (no-op if already on).
    async fn turn_on(&self) -> CapResult<()>;

    /// Switch off. Should be idempotent.
    async fn turn_off(&self) -> CapResult<()>;

    /// Read current state.
    async fn is_on(&self) -> CapResult<bool>;

    /// Flip state. Default impl: query then turn_on/turn_off. Backends
    /// with atomic toggle support can override for slightly faster ops.
    async fn toggle(&self) -> CapResult<()> {
        if self.is_on().await? {
            self.turn_off().await
        } else {
            self.turn_on().await
        }
    }
}

// ---- Analog capability trait --------------------------------------------

/// A capability with a numeric value over a range (volume, brightness).
#[async_trait]
pub trait AnalogCapability: Send + Sync {
    fn id(&self) -> &str;
    fn target(&self) -> Target;
    fn priority(&self) -> i32 { 50 }
    fn install_hint(&self) -> Option<&str> { None }
    async fn is_available(&self) -> bool;

    /// Valid range for set/adjust. Values outside are clamped by the
    /// dispatcher before reaching the capability. Default is 0..=100.
    fn range(&self) -> (i32, i32) { (0, 100) }

    /// Default step size for "increase/decrease" with no explicit amount.
    /// Most capabilities default to 10% steps; brightness might use less.
    fn default_step(&self) -> i32 { 10 }

    /// Set absolute value. Must accept anything within range().
    async fn set(&self, value: i32) -> CapResult<()>;

    /// Adjust by delta. Default impl: read current, clamp, set.
    /// Backends with native deltas (AppleScript key codes for brightness)
    /// can override for cleaner semantics.
    async fn adjust(&self, delta: i32) -> CapResult<()> {
        let cur = self.current().await?;
        let (min, max) = self.range();
        let new = (cur + delta).clamp(min, max);
        self.set(new).await
    }

    /// Read current value.
    async fn current(&self) -> CapResult<i32>;
}

// ---- Registries ---------------------------------------------------------

/// Per-target chosen-backend registry for binary capabilities.
pub struct BinaryRegistry {
    /// All registered capabilities, even unavailable ones (for diagnostics).
    all: Vec<Arc<dyn BinaryCapability>>,
    /// The highest-priority available capability per target.
    active: HashMap<Target, Arc<dyn BinaryCapability>>,
}

impl Default for BinaryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BinaryRegistry {
    pub fn new() -> Self {
        Self {
            all: Vec::new(),
            active: HashMap::new(),
        }
    }

    /// Register a capability. Multiple capabilities can share a target —
    /// probe time picks the highest-priority available one.
    pub fn register(&mut self, cap: Arc<dyn BinaryCapability>) {
        self.all.push(cap);
    }

    /// Probe every registered capability and populate `active`.
    /// Idempotent — safe to call multiple times.
    pub async fn probe_all(&mut self) {
        self.active.clear();

        // Group by target, sort by priority (descending) for tie-breaking.
        let mut by_target: HashMap<Target, Vec<Arc<dyn BinaryCapability>>> = HashMap::new();
        for cap in &self.all {
            by_target.entry(cap.target()).or_default().push(cap.clone());
        }

        for (target, mut caps) in by_target {
            caps.sort_by_key(|c| -c.priority()); // highest first
            for cap in caps {
                let avail = dispatch_safe(cap.id(), || async { Ok(cap.is_available().await) })
                    .await
                    .unwrap_or(false);
                if avail {
                    info!(
                        target = ?target,
                        cap = cap.id(),
                        priority = cap.priority(),
                        "binary capability active"
                    );
                    self.active.insert(target, cap);
                    break;
                } else {
                    info!(target = ?target, cap = cap.id(), "binary capability unavailable");
                }
            }
        }
    }

    pub fn resolve(&self, target: &Target) -> Option<Arc<dyn BinaryCapability>> {
        self.active.get(target).cloned()
    }

    /// For startup logging — list everything registered.
    pub fn report(&self) -> Vec<RegistryEntry> {
        self.all
            .iter()
            .map(|c| RegistryEntry {
                id: c.id().to_string(),
                target: format!("{:?}", c.target()),
                priority: c.priority(),
                install_hint: c.install_hint().map(String::from),
                active: self.active.get(&c.target())
                    .map(|active| active.id() == c.id())
                    .unwrap_or(false),
            })
            .collect()
    }
}

/// Per-target chosen-backend registry for analog capabilities.
pub struct AnalogRegistry {
    all: Vec<Arc<dyn AnalogCapability>>,
    active: HashMap<Target, Arc<dyn AnalogCapability>>,
}

impl Default for AnalogRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AnalogRegistry {
    pub fn new() -> Self {
        Self {
            all: Vec::new(),
            active: HashMap::new(),
        }
    }

    pub fn register(&mut self, cap: Arc<dyn AnalogCapability>) {
        self.all.push(cap);
    }

    pub async fn probe_all(&mut self) {
        self.active.clear();
        let mut by_target: HashMap<Target, Vec<Arc<dyn AnalogCapability>>> = HashMap::new();
        for cap in &self.all {
            by_target.entry(cap.target()).or_default().push(cap.clone());
        }

        for (target, mut caps) in by_target {
            caps.sort_by_key(|c| -c.priority());
            for cap in caps {
                let avail = dispatch_safe(cap.id(), || async { Ok(cap.is_available().await) })
                    .await
                    .unwrap_or(false);
                if avail {
                    info!(
                        target = ?target,
                        cap = cap.id(),
                        priority = cap.priority(),
                        "analog capability active"
                    );
                    self.active.insert(target, cap);
                    break;
                } else {
                    info!(target = ?target, cap = cap.id(), "analog capability unavailable");
                }
            }
        }
    }

    pub fn resolve(&self, target: &Target) -> Option<Arc<dyn AnalogCapability>> {
        self.active.get(target).cloned()
    }

    pub fn report(&self) -> Vec<RegistryEntry> {
        self.all
            .iter()
            .map(|c| RegistryEntry {
                id: c.id().to_string(),
                target: format!("{:?}", c.target()),
                priority: c.priority(),
                install_hint: c.install_hint().map(String::from),
                active: self.active.get(&c.target())
                    .map(|active| active.id() == c.id())
                    .unwrap_or(false),
            })
            .collect()
    }
}

/// Diagnostic entry for capability listing / startup logging.
#[derive(Debug, Clone)]
pub struct RegistryEntry {
    pub id: String,
    pub target: String,
    pub priority: i32,
    pub install_hint: Option<String>,
    pub active: bool,
}

// ---- Combined capabilities handle ---------------------------------------

/// What the executor sees: a single struct holding both registries.
/// Cheap to clone (Arc internally).
pub struct Capabilities {
    pub binary: Arc<tokio::sync::RwLock<BinaryRegistry>>,
    pub analog: Arc<tokio::sync::RwLock<AnalogRegistry>>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for Capabilities {
    fn clone(&self) -> Self {
        Self {
            binary: Arc::clone(&self.binary),
            analog: Arc::clone(&self.analog),
        }
    }
}

impl Capabilities {
    pub fn new() -> Self {
        Self {
            binary: Arc::new(tokio::sync::RwLock::new(BinaryRegistry::new())),
            analog: Arc::new(tokio::sync::RwLock::new(AnalogRegistry::new())),
        }
    }

    /// Convenience: register all built-in capabilities. Caller still has to
    /// call probe_all() afterward.
    pub async fn register_builtins(&self) {
        let mut bin = self.binary.write().await;
        bin.register(Arc::new(wifi::WiFiViaNetworksetup));
        bin.register(Arc::new(bluetooth::BluetoothViaBlueutil));
        bin.register(Arc::new(bluetooth::BluetoothViaAppleScript));
        bin.register(Arc::new(bluetooth::BluetoothViaSettings));
        bin.register(Arc::new(dark_mode::DarkModeViaAppleScript));
        bin.register(Arc::new(dnd::DndViaShortcuts));
        drop(bin);

        let mut ana = self.analog.write().await;
        ana.register(Arc::new(brightness::ScreenBrightnessViaDisplayServices));
        ana.register(Arc::new(volume::VolumeViaCoreAudio));
        ana.register(Arc::new(brightness::ScreenBrightnessViaAppleScript));
        ana.register(Arc::new(brightness::ScreenBrightnessViaCli));
        ana.register(Arc::new(brightness::KeyboardBrightnessViaCli));
    }

    pub async fn probe_all(&self) {
        self.binary.write().await.probe_all().await;
        self.analog.write().await.probe_all().await;
    }

    pub async fn report(&self) -> CapabilityReport {
        let binary = self.binary.read().await.report();
        let analog = self.analog.read().await.report();
        CapabilityReport { binary, analog }
    }
}

#[derive(Debug, Clone)]
pub struct CapabilityReport {
    pub binary: Vec<RegistryEntry>,
    pub analog: Vec<RegistryEntry>,
}

impl CapabilityReport {
    /// Pretty-print for startup logs.
    pub fn log_summary(&self) {
        info!("=== Capabilities ===");
        for e in &self.binary {
            let marker = if e.active { "✓" } else { " " };
            info!("  {} [binary] {} → {} (priority {})", marker, e.target, e.id, e.priority);
        }
        for e in &self.analog {
            let marker = if e.active { "✓" } else { " " };
            info!("  {} [analog] {} → {} (priority {})", marker, e.target, e.id, e.priority);
        }
        // Note any targets with no active capability.
        let active_binary_targets: std::collections::HashSet<&str> = self.binary.iter()
            .filter(|e| e.active).map(|e| e.target.as_str()).collect();
        let active_analog_targets: std::collections::HashSet<&str> = self.analog.iter()
            .filter(|e| e.active).map(|e| e.target.as_str()).collect();
        let unmet: Vec<_> = self.binary.iter().chain(self.analog.iter())
            .filter(|e| !active_binary_targets.contains(e.target.as_str())
                     && !active_analog_targets.contains(e.target.as_str()))
            .collect();
        if !unmet.is_empty() {
            warn!("targets with no working backend (will return errors at runtime):");
            for e in unmet {
                if let Some(hint) = &e.install_hint {
                    warn!("    {} — {}", e.target, hint);
                } else {
                    warn!("    {}", e.target);
                }
            }
        }
    }
}