// src/engines/capabilities/dispatch.rs
//
// dispatch_safe: wraps every capability call to enforce two safety invariants:
//
//   1. No capability can hang the daemon. tokio::time::timeout terminates
//      stuck operations after a fixed budget.
//
//   2. No capability can panic the daemon. catch_unwind converts panics
//      into structured CapError::Panic, logged with context for the dev.
//
// This is the boundary between the registry and individual capability
// implementations. Every call goes through it. Cost is negligible (one
// extra task spawn + one timeout future composition).

use std::future::Future;
use std::time::Duration;

use futures::FutureExt;
use tracing::{error, warn};

use super::error::{CapError, CapResult};

/// Default per-call timeout. Capabilities can override per-action by using
/// `dispatch_safe_with` directly. Most operations should complete in well
/// under 1 second; 5s is generous.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// Wrap a capability call with timeout + panic handling.
///
/// `cap_id` is the capability id (e.g. "core::bluetooth::blueutil") used for
/// logging when something goes wrong. It's structured so we can grep logs.
pub async fn dispatch_safe<F, Fut, T>(
    cap_id: &str,
    f: F,
) -> CapResult<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = CapResult<T>>,
{
    dispatch_safe_with(cap_id, DEFAULT_TIMEOUT, f).await
}

/// Same as dispatch_safe but with an explicit timeout.
pub async fn dispatch_safe_with<F, Fut, T>(
    cap_id: &str,
    timeout: Duration,
    f: F,
) -> CapResult<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = CapResult<T>>,
{
    // Build the future inside AssertUnwindSafe so it can be caught.
    // catch_unwind on a future catches panics during its poll() — what we
    // want for guarding against bad capability code.
    let fut = std::panic::AssertUnwindSafe(f()).catch_unwind();

    let result = tokio::time::timeout(timeout, fut).await;

    match result {
        // Future completed without panic
        Ok(Ok(inner)) => inner,

        // Future panicked
        Ok(Err(panic)) => {
            let msg = panic_message(&panic);
            error!(cap = cap_id, panic = %msg, "capability panicked");
            Err(CapError::Panic(format!("{}: {}", cap_id, msg)))
        }

        // Timed out
        Err(_) => {
            warn!(cap = cap_id, timeout_secs = timeout.as_secs(), "capability timed out");
            Err(CapError::Timeout(timeout))
        }
    }
}

/// Best-effort extraction of a useful message from a panic payload.
/// Panics can carry &'static str, String, or arbitrary types.
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = panic.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn happy_path_passes_through() {
        let r: CapResult<i32> = dispatch_safe("test", || async { Ok(42) }).await;
        assert_eq!(r.unwrap(), 42);
    }

    #[tokio::test]
    async fn error_passes_through() {
        let r: CapResult<i32> = dispatch_safe("test", || async {
            Err(CapError::external("boom"))
        }).await;
        assert!(matches!(r, Err(CapError::External(_))));
    }

    #[tokio::test]
    async fn timeout_fires() {
        let r: CapResult<()> = dispatch_safe_with(
            "test",
            Duration::from_millis(50),
            || async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(())
            },
        ).await;
        assert!(matches!(r, Err(CapError::Timeout(_))));
    }

    #[tokio::test]
    async fn panic_caught() {
        let r: CapResult<()> = dispatch_safe("test", || async {
            panic!("intentional test panic");
        }).await;
        match r {
            Err(CapError::Panic(msg)) => {
                assert!(msg.contains("test"));
                assert!(msg.contains("intentional test panic"));
            }
            other => panic!("expected Panic, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn panic_with_string_message() {
        let r: CapResult<()> = dispatch_safe("test", || async {
            panic!("{}", String::from("dynamic message"));
        }).await;
        match r {
            Err(CapError::Panic(msg)) => assert!(msg.contains("dynamic message")),
            other => panic!("expected Panic, got {:?}", other),
        }
    }
}