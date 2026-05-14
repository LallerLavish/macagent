//! LLM inference via Python subprocess (mlx_lm).
//!
//! Spawns the bundled `summary_inference.py` script with the prepared
//! prompt on stdin. Script loads model + adapter, generates, exits.
//! Memory is reclaimed when subprocess exits.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::debug;

const INFERENCE_TIMEOUT: Duration = Duration::from_secs(120);  // generous; LLM can be slow

/// Default Python interpreter and inference script paths.
/// Override via env vars MACAGENT_PYTHON and MACAGENT_INFERENCE_SCRIPT.
const DEFAULT_PYTHON: &str = "/Users/lavishlaller/Lavish/qwen/venv/bin/python3";
const DEFAULT_SCRIPT: &str = "/Users/lavishlaller/Lavish/qwen/summary_inference.py";

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("inference timed out after {0:?}")]
    Timeout(Duration),

    #[error("subprocess failed: {0}")]
    Subprocess(String),

    #[error("inference returned non-zero: code={code}, stderr={stderr}")]
    NonZero { code: i32, stderr: String },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub async fn generate_summary(prompt: &str) -> Result<String, LlmError> {
    let python = std::env::var("MACAGENT_PYTHON").unwrap_or_else(|_| DEFAULT_PYTHON.to_string());
    let script = std::env::var("MACAGENT_INFERENCE_SCRIPT")
        .unwrap_or_else(|_| DEFAULT_SCRIPT.to_string());

    if !PathBuf::from(&python).exists() {
        return Err(LlmError::Subprocess(format!("python not found: {python}")));
    }
    if !PathBuf::from(&script).exists() {
        return Err(LlmError::Subprocess(format!("script not found: {script}")));
    }

    debug!(prompt_len = prompt.len(), "spawning inference subprocess");

    let mut child = Command::new(&python)
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // Write prompt to stdin
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(prompt.as_bytes()).await?;
        stdin.shutdown().await?;
    }

    // Wait with timeout
    let output = match timeout(INFERENCE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(LlmError::Io(e)),
        Err(_) => return Err(LlmError::Timeout(INFERENCE_TIMEOUT)),
    };

    if !output.status.success() {
        return Err(LlmError::NonZero {
            code: output.status.code().unwrap_or(-1),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let summary = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!(summary_len = summary.len(), "inference complete");
    Ok(summary)
}