use std::time::Duration;
use futures::future::join_all;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::modes::error::ModeError;

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Launch all apps in parallel by bundle ID. Returns the bundle IDs that
/// launched successfully. Failures are logged but do not abort the operation.
pub async fn launch_apps(bundle_ids: &[String]) -> Result<Vec<String>, ModeError> {
    let futures = bundle_ids.iter().map(|bid| {
        let bid = bid.clone();
        async move {
            let result = launch_one(&bid).await;
            (bid, result)
        }
    });

    let results = join_all(futures).await;
    let mut launched = Vec::new();

    for (bid, result) in results {
        match result {
            Ok(()) => {
                debug!(bundle_id = %bid, "launched");
                launched.push(bid);
            }
            Err(e) => {
                warn!(bundle_id = %bid, error = %e, "launch failed");
            }
        }
    }

    Ok(launched)
}

async fn launch_one(bundle_id: &str) -> Result<(), ModeError> {
    let output = timeout(
        LAUNCH_TIMEOUT,
        Command::new("open")
            .arg("-b")
            .arg(bundle_id)
            .output(),
    )
    .await
    .map_err(|_| ModeError::Restore(format!("launch of {bundle_id} timed out")))?
    .map_err(|e| ModeError::Restore(format!("open spawn failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ModeError::Restore(format!("open failed for {bundle_id}: {stderr}")));
    }

    Ok(())
}