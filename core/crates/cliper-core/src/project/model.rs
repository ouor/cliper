//! `project.json` — the persisted state of one VOD being clipped.
//!
//! Every time is on the **VOD timeline** (seconds from the start of the VOD's
//! EXTINF timeline). A clip maps VOD time to its own file time via
//! [`Clip::to_clip_time`].

use serde::{Deserialize, Serialize};

use crate::analysis::detect::Params;
use crate::hls::audio_extract::{AudioResult, SegIndex};
use crate::hls::video_fetch::VideoClip;
use crate::source::SourceInfo;
use crate::stems::Stems;

pub const SCHEMA: &str = "cliper/project@1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Project {
    pub schema: String,
    pub id: String,
    /// what the user typed (URL / id)
    pub input: String,
    pub created_at: String,
    pub updated_at: String,
    pub source: Option<SourceInfo>,
    pub audio: Option<AudioState>,
    pub detection: Option<Detection>,
    pub settings: Settings,
    pub songs: Vec<Song>,
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AudioState {
    /// file name inside the project dir (ADTS)
    pub file: String,
    pub variant: usize,
    pub duration: f64,
    pub index: SegIndex,
    pub fetched_bytes: u64,
    pub full_source_bytes: u64,
    pub fallback_segments: u32,
    pub elapsed_sec: f64,
}

impl AudioState {
    pub fn from_result(file: &str, variant: usize, r: &AudioResult) -> Self {
        Self {
            file: file.to_string(),
            variant,
            duration: r.index.duration(),
            index: r.index.clone(),
            fetched_bytes: r.fetched_bytes,
            full_source_bytes: r.full_source_bytes,
            fallback_segments: r.fallback_segments,
            elapsed_sec: r.elapsed_sec,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Detection {
    /// CLPROBS1 file inside the project dir
    pub probs_file: String,
    pub params: Params,
    /// raw detector output (before user edits)
    pub segments: Vec<(f64, f64)>,
    pub device: crate::ml::runtime::Device,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct Settings {
    /// seconds of video fetched before each song
    pub margin_before: f64,
    /// seconds of video fetched after each song
    pub margin_after: f64,
    /// video rendition height (None = highest/original)
    pub video_height: Option<u32>,
    /// default fade length for new mutes (s)
    pub mute_fade: f64,
    /// export: "h264_nvenc" | "libx264" | "copy"
    pub video_codec: String,
    /// export quality (CQ for nvenc, CRF for x264)
    pub video_quality: u32,
    pub audio_bitrate_k: u32,
    pub output_dir: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            margin_before: 30.0,
            margin_after: 30.0,
            video_height: None,
            mute_fade: 0.3,
            video_codec: "h264_nvenc".into(),
            video_quality: 23,
            audio_bitrate_k: 320,
            output_dir: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// produced by the detector / suggester
    Auto,
    /// created or edited by the user
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Song {
    pub id: String,
    pub title: String,
    pub start: f64,
    pub end: f64,
    pub origin: Origin,
    /// excluded songs are kept (so detection reruns don't resurrect them) but not exported
    #[serde(default = "yes")]
    pub enabled: bool,
    pub clip_id: Option<String>,
    #[serde(default)]
    pub mutes: Vec<Mute>,
    pub export: Option<ExportInfo>,
}

fn yes() -> bool {
    true
}

/// Vocal-mute region: inside `[start, end]` the vocal stem is removed, ramping
/// with raised-cosine fades of `fade_in` / `fade_out` *inside* the region.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Mute {
    pub id: String,
    pub start: f64,
    pub end: f64,
    pub fade_in: f64,
    pub fade_out: f64,
    pub origin: Origin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ExportInfo {
    pub path: String,
    pub exported_at: String,
    pub duration: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Clip {
    pub id: String,
    /// requested VOD range (songs ± margins, merged)
    pub start: f64,
    pub end: f64,
    pub video: Option<VideoClip>,
    /// measured alignment: clip time = VOD time − video.vod_start − offset
    #[serde(default)]
    pub offset: f64,
    pub stems: Option<Stems>,
    /// talk regions found in the vocal stem (VOD time), mute suggestions
    #[serde(default)]
    pub talk: Vec<(f64, f64)>,
}

impl Clip {
    pub fn dir_name(&self) -> String {
        format!("clips/{}", self.id)
    }
    /// VOD time → time inside the clip's files (None until the video exists).
    pub fn to_clip_time(&self, vod: f64) -> Option<f64> {
        self.video.as_ref().map(|v| vod - v.vod_start - self.offset)
    }
    pub fn to_vod_time(&self, clip_t: f64) -> Option<f64> {
        self.video.as_ref().map(|v| clip_t + v.vod_start + self.offset)
    }
    /// VOD range actually covered by the downloaded video.
    pub fn covered(&self) -> Option<(f64, f64)> {
        self.video.as_ref().map(|v| (v.vod_start + self.offset, v.vod_start + self.offset + v.duration))
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub fn new_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let t = chrono::Utc::now().timestamp_micros() as u64;
    format!("{prefix}{:x}{:02x}", t, N.fetch_add(1, Ordering::Relaxed) & 0xff)
}

impl Project {
    pub fn new(id: &str, input: &str) -> Self {
        let now = now();
        Self {
            schema: SCHEMA.into(),
            id: id.to_string(),
            input: input.to_string(),
            created_at: now.clone(),
            updated_at: now,
            source: None,
            audio: None,
            detection: None,
            settings: Settings::default(),
            songs: Vec::new(),
            clips: Vec::new(),
        }
    }

    pub fn song(&self, id: &str) -> Option<&Song> {
        self.songs.iter().find(|s| s.id == id)
    }
    pub fn song_mut(&mut self, id: &str) -> Option<&mut Song> {
        self.songs.iter_mut().find(|s| s.id == id)
    }
    pub fn clip(&self, id: &str) -> Option<&Clip> {
        self.clips.iter().find(|c| c.id == id)
    }
    pub fn clip_mut(&mut self, id: &str) -> Option<&mut Clip> {
        self.clips.iter_mut().find(|c| c.id == id)
    }

    /// Replace auto songs with fresh detector output, keeping manual songs and
    /// any auto song the user touched (titles/mutes/disabled survive reruns).
    pub fn apply_detection(&mut self, segs: &[(f64, f64)]) {
        let keep: Vec<Song> = self
            .songs
            .drain(..)
            .filter(|s| s.origin == Origin::Manual || !s.enabled || !s.title.is_empty() || !s.mutes.is_empty() || s.clip_id.is_some())
            .collect();
        let overlaps = |a: (f64, f64), b: (f64, f64)| a.0 < b.1 && b.0 < a.1;
        let mut songs = keep;
        for &seg in segs {
            if songs.iter().any(|s| overlaps((s.start, s.end), seg)) {
                continue;
            }
            songs.push(Song {
                id: new_id("s"),
                title: String::new(),
                start: seg.0,
                end: seg.1,
                origin: Origin::Auto,
                enabled: true,
                clip_id: None,
                mutes: Vec::new(),
                export: None,
            });
        }
        songs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
        self.songs = songs;
    }
}
