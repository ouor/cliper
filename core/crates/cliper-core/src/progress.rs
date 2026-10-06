//! Progress reporting + cancellation shared by long-running operations.

use std::sync::Arc;

use serde::Serialize;
pub use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize)]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
    /// network bytes so far (if relevant)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl Progress {
    pub fn new(stage: &str, done: u64, total: u64) -> Self {
        Self { stage: stage.to_string(), done, total, bytes: None, message: None }
    }
    pub fn note(stage: &str, message: impl Into<String>) -> Self {
        Self { stage: stage.to_string(), done: 0, total: 0, bytes: None, message: Some(message.into()) }
    }
}

pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

pub fn noop() -> ProgressFn {
    Arc::new(|_| {})
}

#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;
