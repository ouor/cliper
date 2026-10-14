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
    /// each song's range ± margins when requested (same order as `song_ids`)
    #[serde(default)]
    pub song_ranges: Vec<Range>,
    /// preferred rendition height for segments that don't continue an
    /// existing range (None = settings.video_height at request time)
    #[serde(default)]
    pub height: Option<u32>,
    /// separate + suggest mutes for `song_ids` (or `ranges`) once fetched
    #[serde(default)]
    pub process: bool,
    /// runs even while downloads are paused (an export waiting for it, R-11)
    #[serde(default)]
    pub force: bool,
    pub created_at: String,
    /// order among requests of the same priority, across projects
    /// (microseconds since 1970 when made; 0 = "fetch this first")
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
    /// rendition key (resolved once from the index-era `variant`); `variant`
    /// is then the rendition folder number of that key
    #[serde(default)]
    pub key: Option<String>,
}

/// What a rendition folder `v<N>` holds (A-1): its key and description.
/// Folder numbers are assigned once per key and never reused, so a folder
/// (and its `init.mp4`, alignment, probes) never mixes two renditions even
/// if the platform reorders, adds or removes renditions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rendition {
    pub key: String,
    pub width: u32,
    pub height: u32,
    pub bandwidth: u64,
    #[serde(default)]
    pub codecs: Option<String>,
}

/// Stable identity of a rendition across re-resolves: size, bitrate, codecs.
pub fn rendition_key(v: &crate::hls::m3u8::Variant) -> String {
    match &v.codecs {
        Some(c) => format!("{}x{}@{}/{}", v.width, v.height, v.bandwidth, c),
        None => format!("{}x{}@{}", v.width, v.height, v.bandwidth),
    }
}

impl Rendition {
    pub fn of(v: &crate::hls::m3u8::Variant) -> Self {
        Self { key: rendition_key(v), width: v.width, height: v.height, bandwidth: v.bandwidth, codecs: v.codecs.clone() }
    }
}

/// Why a sidecar couldn't be used.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("video/index.json was written by a newer engine (version {0}); update Cliper")]
    Newer(u32),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Version of `video/index.json` this engine writes (2: rendition table).
pub const INDEX_VERSION: u32 = 2;

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
    /// rendition folder number → what it holds (A-1)
    pub renditions: BTreeMap<usize, Rendition>,
    /// ranges to separate once fetched (`video.fetch {process}`), kept until done
    pub pending_process: Vec<Range>,
    /// problems met while loading (a corrupt file set aside), for `VideoState.warnings`
    #[serde(skip)]
    pub warnings: Vec<String>,
}

impl VideoIndex {
    pub fn path(project_dir: &Path) -> PathBuf {
        project_dir.join(DIR).join("index.json")
    }

    /// Load the index. A missing file is an empty index. A file that doesn't
    /// parse is never silently replaced (A-3): it is set aside as
    /// `index.json.corrupt-<time>`, the last good copy (`index.json.bak`) is
    /// used if there is one, and a warning is recorded. A newer version is refused.
    pub fn load(project_dir: &Path) -> Result<Self, IndexError> {
        let path = Self::path(project_dir);
        let parse = |p: &Path| -> Option<Result<Self, IndexError>> {
            let b = std::fs::read(p).ok()?;
            let v: serde_json::Value = serde_json::from_slice(&b).ok()?;
            let ver = v.get("version").and_then(|x| x.as_u64()).unwrap_or(1) as u32;
            if ver > INDEX_VERSION {
                return Some(Err(IndexError::Newer(ver)));
            }
            serde_json::from_value::<Self>(v).ok().map(Ok)
        };
        if !path.exists() {
            return Ok(Self { version: INDEX_VERSION, ..Default::default() });
        }
        if let Some(r) = parse(&path) {
            return r;
        }
        let bad = path.with_file_name(format!("index.json.corrupt-{}", chrono::Utc::now().format("%Y%m%d%H%M%S")));
        std::fs::rename(&path, &bad)?;
        let bak = path.with_file_name("index.json.bak");
        let (mut idx, how) = match parse(&bak) {
            Some(Ok(i)) => (i, "the last good copy was used"),
            Some(Err(e)) => return Err(e),
            None => (Self { version: INDEX_VERSION, ..Default::default() }, "no good copy: downloads in progress and adopted clips were lost"),
        };
        let w = format!("video/index.json could not be read; set aside as {}; {how}", bad.file_name().unwrap().to_string_lossy());
        tracing::error!("{}: {w}", project_dir.display());
        idx.warnings.push(w);
        Ok(idx)
    }

    /// Save (the previous file becomes `index.json.bak`).
    pub fn save(&self, project_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(project_dir.join(DIR))?;
        let path = Self::path(project_dir);
        if path.is_file() {
            let _ = std::fs::copy(&path, path.with_file_name("index.json.bak"));
        }
        let mut me = self.clone();
        me.version = INDEX_VERSION;
        crate::project::store::write_atomic(&path, &serde_json::to_vec_pretty(&me).map_err(std::io::Error::other)?)
    }

    /// Folder number for a rendition key (a new one if the key is unknown).
    pub fn folder_for(&mut self, r: &Rendition) -> usize {
        if let Some((&n, _)) = self.renditions.iter().find(|(_, x)| x.key == r.key) {
            return n;
        }
        let n = self.renditions.keys().next_back().map(|n| n + 1).unwrap_or(0);
        self.renditions.insert(n, r.clone());
        n
    }

    pub fn folder_of_key(&self, key: &str) -> Option<usize> {
        self.renditions.iter().find(|(_, x)| x.key == key).map(|(&n, _)| n)
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

/// Rendition folder numbers present on disk (`video/v<N>/`).
pub fn raw_folders(project_dir: &Path) -> Vec<usize> {
    let mut out: Vec<usize> = std::fs::read_dir(project_dir.join(DIR))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_string_lossy().strip_prefix('v').and_then(|n| n.parse().ok()))
        .collect();
    out.sort_unstable();
    out
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
                LegacyPiece { id: "c1".into(), file: "clips/c1/source.mp4".into(), variant: 3, first: 1, last: 4, origin: 0.0, resolution: String::new(), key: None },
                LegacyPiece { id: "gone".into(), file: "clips/gone/source.mp4".into(), variant: 3, first: 20, last: 22, origin: 0.0, resolution: String::new(), key: None },
            ],
            ..Default::default()
        };
        let h = scan(&d, &idx);
        assert_eq!(h.keys().copied().collect::<Vec<_>>(), vec![1, 2, 3, 7]);
        assert_eq!(h[&7].kind, HaveKind::Raw(2));
        assert_eq!(h[&2].kind, HaveKind::Legacy(0));
        assert_eq!(h[&2].bytes, 100);
        idx.save(&d).unwrap();
        assert_eq!(VideoIndex::load(&d).unwrap().legacy.len(), 2);
        // A-3: a newer index is refused, not reset
        std::fs::write(d.join(DIR).join("index.json"), br#"{"version": 99}"#).unwrap();
        assert!(matches!(VideoIndex::load(&d), Err(IndexError::Newer(99))));
        let _ = std::fs::remove_dir_all(&d);
    }
}
