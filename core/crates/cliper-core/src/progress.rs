//! Progress reporting + cancellation shared by long-running operations.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Serialize;
pub use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
    /// network bytes so far (if relevant)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub bytes: Option<u64>,
    /// English text for logs / the CLI. UIs should use `note` instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub message: Option<String>,
    /// machine-readable event (UIs build their own sentence from it)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub note: Option<Note>,
    /// what the progress is about (clip id in clips.fetch, song id in export.run)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub item: Option<String>,
    /// 0-based position of `item` in the job's `items` (see job.started)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub item_index: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub items_total: Option<u64>,
    /// estimated seconds left in the current stage (filled in by the engine)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub eta_sec: Option<f64>,
    /// audio.fetch: overview.peaks now covers [0, peaks_until) seconds
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub peaks_until: Option<f64>,
}

/// Machine-readable progress note. Codes:
/// `copy_audio_track`, `cut_recording`, `find_talk`, `fallback_segments {count}`,
/// `variant_truncated {resolution}`, `remux`, `render_audio`, `measure_loudness`,
/// `write_stems`, `write_reference_video`, `decode`.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Note {
    pub code: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    #[cfg_attr(feature = "ts", ts(optional, type = "Record<string, string | number>"))]
    pub args: BTreeMap<String, serde_json::Value>,
}

impl Progress {
    pub fn new(stage: &str, done: u64, total: u64) -> Self {
        Self {
            stage: stage.to_string(),
            done,
            total,
            bytes: None,
            message: None,
            note: None,
            item: None,
            item_index: None,
            items_total: None,
            eta_sec: None,
            peaks_until: None,
        }
    }
    /// A note: `code` for UIs, `message` for logs.
    pub fn note(stage: &str, code: &str, message: impl Into<String>) -> Self {
        Self {
            message: Some(message.into()),
            note: Some(Note { code: code.into(), args: BTreeMap::new() }),
            ..Self::new(stage, 0, 0)
        }
    }
    pub fn arg(mut self, k: &str, v: impl Into<serde_json::Value>) -> Self {
        if let Some(n) = self.note.as_mut() {
            n.args.insert(k.into(), v.into());
        }
        self
    }
    pub fn is_note(&self) -> bool {
        self.note.is_some() || self.message.is_some()
    }
}

pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

pub fn noop() -> ProgressFn {
    Arc::new(|_| {})
}

#[derive(Debug, thiserror::Error)]
#[error("cancelled")]
pub struct Cancelled;
