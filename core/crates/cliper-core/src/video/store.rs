//! On-disk segment store of one project.
//!
//! ```text
//! <project>/video/
//!   index.json                   VideoIndex (requests, failures, alignment, legacy clips, probes, views)
//!   v<variant>/init.mp4          fMP4 init segment of that rendition
//!   v<variant>/<index:06>.m4s    one downloaded HLS segment (fMP4) …
//!   v<variant>/<index:06>.ts     … or MPEG-TS, exactly the CDN's bytes
//!   views/<key>.mp4              playable remux of a contiguous range (cache, rebuilt on demand)
//! ```
//!
//! The files are the truth for "what we have": a segment exists when its file
//! exists (written to `.part` and renamed, so a file is always complete). A
//! user deleting files in Explorer just turns that range back into "none".
//! `index.json` keeps what the files can't say: pending requests (resumed after
//! a restart), failures, the video↔analysis alignment per rendition, clips
//! adopted from projects made before the segment store (legacy pieces) and
//! the view cache.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ranges::Range;
use crate::source::Container;

pub const DIR: &str = "video";

/// Fetch priority (lower = sooner): ① what the user is looking at, ② requests
/// the user made (in order), ③ automatic top-ups, ④ overnight batches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Now,
    #[default]
    User,
    Auto,
    Batch,
}

/// A request for the video of some VOD ranges (S-E2). Segments already on
/// disk or being fetched for another request are never fetched again.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FetchRequest {
    pub id: String,
    /// VOD ranges asked for (rounded out to whole segments when fetched)
    pub ranges: Vec<Range>,
    pub priority: Priority,
    /// free-form tag from the client: "song", "here", "range", "topup", "export", "batch", …
    pub reason: String,
    /// songs this request is for (progress per song; `process` separates them)
    #[serde(default)]
    pub song_ids: Vec<String>,
    /// preferred rendition height for segments that don't continue an
    /// existing range (None = settings.video_height at request time)
    #[serde(default)]
    pub height: Option<u32>,
    /// separate + suggest mutes for `song_ids` (or `ranges`) once fetched
    #[serde(default)]
    pub process: bool,
    pub created_at: String,
    /// order among requests of the same priority
    pub seq: u64,
}

/// A segment that could not be fetched (after the retries).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SegFailure {
    /// engine error code: NETWORK | CDN_TRUNCATED | VOD_GONE | AUTH_REQUIRED | FORMAT | IO
    pub code: String,
    pub message: String,
    pub attempts: u32,
    pub at: String,
}

/// A clip downloaded by an engine before the segment store (one remuxed MP4
/// holding segments `[first, last)` of `variant`). Adopted as-is: its file
/// time t is VOD time `t + origin`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyPiece {
    /// old clip id
    pub id: String,
    /// project-relative path of the MP4
    pub file: String,
    pub variant: usize,
    pub first: usize,
    pub last: usize,
    /// VOD time of file t = 0 (old `vod_start + offset`)
    pub origin: f64,
    pub resolution: String,
}

/// Video ↔ analysis alignment of one rendition (or of the local recording):
/// `vod = media − m`, where media time is the rendition's own timestamps
/// (tfdt / PTS seconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Align {
    pub m: f64,
    /// "measured" (cross-correlated with the analysis audio) | "predicted" (frame index only)
    pub method: String,
    pub corr: f32,
    /// the frame-index prediction, for comparison
    pub predicted: f64,
    pub ok: bool,
    /// segment the measurement started at
    pub at_segment: usize,
}

/// Media timestamps of a segment's first packets (ffprobe, cached).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Probe {
    /// min over streams (= t 0 of a remux that starts here)
    pub format_start: f64,
    /// first audio sample (= sample 0 of a decode that starts here)
    pub audio_start: f64,
}

/// A cached playable remux.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewEntry {
    pub key: String,
    /// project-relative path
    pub file: String,
    pub variant: usize,
    pub first: usize,
    pub last: usize,
    /// VOD time of file t = 0
    pub origin: f64,
    pub bytes: u64,
    pub last_used: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct VideoIndex {
    pub version: u32,
    pub requests: Vec<FetchRequest>,
    pub next_seq: u64,
    /// segment index → why it failed
    pub failed: BTreeMap<usize, SegFailure>,
    pub legacy: Vec<LegacyPiece>,
    /// "v<variant>" | "local" → alignment
    pub align: BTreeMap<String, Align>,
    /// "v<variant>:<segment>" → probe
    pub probes: BTreeMap<String, Probe>,
    pub views: Vec<ViewEntry>,
}

impl VideoIndex {
    pub fn path(project_dir: &Path) -> PathBuf {
        project_dir.join(DIR).join("index.json")
    }
    pub fn load(project_dir: &Path) -> Self {
        std::fs::read(Self::path(project_dir)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| Self { version: 1, ..Default::default() })
    }
    pub fn save(&self, project_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(project_dir.join(DIR))?;
        crate::project::store::write_atomic(&Self::path(project_dir), &serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)
    }
}

pub fn variant_dir(project_dir: &Path, variant: usize) -> PathBuf {
    project_dir.join(DIR).join(format!("v{variant}"))
}

pub fn ext(container: Container) -> &'static str {
    match container {
        Container::Ts => "ts",
        Container::Fmp4 => "m4s",
    }
}

pub fn segment_name(index: usize, container: Container) -> String {
    format!("{index:06}.{}", ext(container))
}

pub fn segment_path(project_dir: &Path, variant: usize, index: usize, container: Container) -> PathBuf {
    variant_dir(project_dir, variant).join(segment_name(index, container))
}

pub fn init_path(project_dir: &Path, variant: usize) -> PathBuf {
    variant_dir(project_dir, variant).join("init.mp4")
}

/// Where a segment's bytes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HaveKind {
    /// `v<variant>/<index>` file
    Raw(usize),
    /// inside legacy piece `n` of the index
    Legacy(usize),
}

impl HaveKind {
    pub fn variant(&self, idx: &VideoIndex) -> usize {
        match *self {
            HaveKind::Raw(v) => v,
            HaveKind::Legacy(n) => idx.legacy.get(n).map(|l| l.variant).unwrap_or(usize::MAX),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Have {
    pub kind: HaveKind,
    pub bytes: u64,
}

/// Scan the store: which segments are on disk and in what form. Raw segments
/// win over legacy pieces (they never overlap in practice). Legacy pieces
/// whose file is gone are skipped. `.part` files are ignored.
pub fn scan(project_dir: &Path, idx: &VideoIndex) -> BTreeMap<usize, Have> {
    let mut out = BTreeMap::new();
    for (n, l) in idx.legacy.iter().enumerate() {
        let f = project_dir.join(&l.file);
        let Ok(m) = std::fs::metadata(&f) else { continue };
        let per = m.len() / (l.last.saturating_sub(l.first).max(1)) as u64;
        for i in l.first..l.last {
            out.insert(i, Have { kind: HaveKind::Legacy(n), bytes: per });
        }
    }
    let Ok(rd) = std::fs::read_dir(project_dir.join(DIR)) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some(v) = name.strip_prefix('v').and_then(|x| x.parse::<usize>().ok()) else { continue };
        let Ok(files) = std::fs::read_dir(e.path()) else { continue };
        for f in files.flatten() {
            let fname = f.file_name().to_string_lossy().into_owned();
            let Some((stem, ext)) = fname.split_once('.') else { continue };
            if !(ext == "m4s" || ext == "ts") {
                continue;
            }
            let Ok(i) = stem.parse::<usize>() else { continue };
            let bytes = f.metadata().map(|m| m.len()).unwrap_or(0);
            if bytes == 0 {
                continue;
            }
            out.insert(i, Have { kind: HaveKind::Raw(v), bytes });
        }
    }
    out
}

/// Total bytes under a directory (recursive).
pub fn dir_size(d: &Path) -> u64 {
    std::fs::read_dir(d)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            _ => e.metadata().map(|m| m.len()).unwrap_or(0),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_reads_files_and_legacy() {
        let d = std::env::temp_dir().join(format!("cliper-vstore-{}-{}", std::process::id(), crate::project::new_id("")));
        std::fs::create_dir_all(variant_dir(&d, 2)).unwrap();
        std::fs::write(segment_path(&d, 2, 7, Container::Fmp4), b"x").unwrap();
        std::fs::write(segment_path(&d, 2, 8, Container::Fmp4).with_extension("m4s.part"), b"x").unwrap();
        std::fs::write(segment_path(&d, 2, 9, Container::Ts), b"").unwrap();
        std::fs::create_dir_all(d.join("clips/c1")).unwrap();
        std::fs::write(d.join("clips/c1/source.mp4"), vec![0u8; 300]).unwrap();
        let idx = VideoIndex {
            legacy: vec![
                LegacyPiece { id: "c1".into(), file: "clips/c1/source.mp4".into(), variant: 3, first: 1, last: 4, origin: 0.0, resolution: String::new() },
                LegacyPiece { id: "gone".into(), file: "clips/gone/source.mp4".into(), variant: 3, first: 20, last: 22, origin: 0.0, resolution: String::new() },
            ],
            ..Default::default()
        };
        let h = scan(&d, &idx);
        assert_eq!(h.keys().copied().collect::<Vec<_>>(), vec![1, 2, 3, 7]);
        assert_eq!(h[&7].kind, HaveKind::Raw(2));
        assert_eq!(h[&2].kind, HaveKind::Legacy(0));
        assert_eq!(h[&2].bytes, 100);
        idx.save(&d).unwrap();
        assert_eq!(VideoIndex::load(&d).legacy.len(), 2);
        let _ = std::fs::remove_dir_all(&d);
    }
}
