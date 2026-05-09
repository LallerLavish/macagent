// INTEGRATION GUIDE — Phase 1
//
// This is the diff against your existing code. Apply these changes after
// adding registry/mod.rs, registry/matcher.rs, and registry/apps.rs.

// ============================================================
// 1. Cargo.toml — add two dependencies
// ============================================================

// Add under [dependencies]:
//
//   plist = "1"               # parses Info.plist files
//
// (You already have tracing, serde, tokio, etc. The matcher uses no extra
// crates — pure std for Levenshtein.)

// ============================================================
// 2. src/main.rs — register the new module
// ============================================================

// Add this with the other `mod` declarations near the top:
//
//   mod registry;

// ============================================================
// 3. src/daemon.rs — add AppRegistry to Engines
// ============================================================

// At the top of daemon.rs, add:
//
//   use crate::registry::AppRegistry;
//
// In the Engines struct, add the field:
//
//   #[derive(Clone)]
//   pub struct Engines {
//       pub executor: Arc<Executor>,
//       pub apps: Arc<AppRegistry>,    // <-- NEW
//   }
//
// In Engines::init(), build the registry:
//
//   fn init() -> Result<Self, DaemonError> {
//       let executor = Executor::new();
//       let apps = AppRegistry::scan_now()
//           .map_err(|e| DaemonError::EngineInit(format!("AppRegistry: {e}")))?;
//
//       info!(
//           apps_found = apps.all().len(),
//           "App registry initialized"
//       );
//
//       Ok(Self {
//           executor: Arc::new(executor),
//           apps: Arc::new(apps),
//       })
//   }
//
// At the end of daemon::run, BEFORE starting the listener, spawn a
// background task that refreshes the registry every 30 seconds. This is
// the safety net while we wait for FSEvents in Phase 2.
//
//   let registry_for_refresh = engines.apps.clone();
//   let mut refresh_shutdown = shutdown_tx.subscribe();
//   tokio::spawn(async move {
//       let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
//       ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
//       loop {
//           tokio::select! {
//               _ = ticker.tick() => {
//                   // Only refresh if the registry hasn't been touched recently.
//                   // This is harmless either way but avoids a redundant scan
//                   // immediately after startup.
//                   if registry_for_refresh.age() >= std::time::Duration::from_secs(25) {
//                       registry_for_refresh.refresh();
//                   }
//               }
//               _ = refresh_shutdown.recv() => break,
//           }
//       }
//   });

// ============================================================
// 4. src/engines/actions/executor.rs — use the registry on app actions
// ============================================================

// The executor needs access to the registry to resolve fuzzy app names
// before handing off to AppleScript. There are two clean ways to pass it:
//
//   a) Add an `Arc<AppRegistry>` field to Executor at construction.
//   b) Pass it via execute() arguments.
//
// Option (a) is cleaner because the Executor's interface stays simple.
// But it means Engines::init() has to construct AppRegistry first, then
// pass it into Executor::new(). That's fine.
//
// Updated signature:
//
//   use crate::registry::{AppRegistry, ResolutionResult};
//   use std::sync::Arc;
//
//   pub struct Executor {
//       apps: Arc<AppRegistry>,
//   }
//
//   impl Executor {
//       pub fn new(apps: Arc<AppRegistry>) -> Self {
//           Self { apps }
//       }
//
//       pub async fn execute(&self, intent: Intent) -> Result<String, ExecError> {
//           match intent.action {
//               Action::OpenApp | Action::CloseApp | Action::QuitApp => {
//                   // Extract the raw target string from the intent. Adjust
//                   // based on your existing Target enum shape.
//                   let raw_target = match &intent.target {
//                       Target::App(name) => name.as_str(),
//                       _ => return Err(ExecError::InvalidAppName("not an app target".into())),
//                   };
//
//                   let canonical = match self.apps.resolve(raw_target) {
//                       ResolutionResult::Confident { canonical, .. } => canonical,
//                       ResolutionResult::Ambiguous { candidates } => {
//                           let names: Vec<String> = candidates
//                               .iter()
//                               .map(|(n, _)| n.clone())
//                               .collect();
//                           return Err(ExecError::InvalidAppName(format!(
//                               "ambiguous app name '{}', candidates: {}",
//                               raw_target,
//                               names.join(", ")
//                           )));
//                       }
//                       ResolutionResult::NotFound => {
//                           return Err(ExecError::InvalidAppName(format!(
//                               "no app found with name '{}'",
//                               raw_target
//                           )));
//                       }
//                   };
//
//                   // Now hand `canonical` to AppleScript — it's the real,
//                   // installed app name and AppleScript will accept it.
//                   self.run_app_action(intent.action, &canonical).await
//               }
//               // ... other action arms unchanged ...
//           }
//       }
//   }
//
// Don't forget to update Engines::init() to pass apps into Executor::new:
//
//   let apps = Arc::new(AppRegistry::scan_now()?);
//   let executor = Executor::new(apps.clone());
//   Ok(Self { executor: Arc::new(executor), apps })

// ============================================================
// 5. ExecError — add a new variant for ambiguity (optional)
// ============================================================

// If you want a distinct error_kind for ambiguous matches (vs. not-found
// vs. invalid), add to your ExecError enum:
//
//   #[error("ambiguous app name: {0}")]
//   AmbiguousAppName(String),
//
// And in kind_str():
//
//   ExecError::AmbiguousAppName(_) => "AmbiguousAppName",
//
// Otherwise, reusing InvalidAppName is fine — the message string carries
// the candidate list.

// ============================================================
// 6. Run the tests
// ============================================================

// From your project root:
//
//   cargo test --lib registry
//
// All matcher tests should pass. The apps test (scan_runs_without_panic)
// will pass on macOS, may not exist on Linux/Windows.
