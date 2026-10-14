//! `project.json` — the persisted state of one VOD being clipped.
//!
//! Every time is on the **VOD timeline** (seconds from the start of the VOD's
//! EXTINF timeline). A clip maps VOD time to its own file time via
//! [`Clip::to_clip_time`].

use serde::{Deserialize, Serialize};

use crate::analysis::detect::{Analysis, Candidate, Confidence, Params};
use crate::hls::audio_extract::{AudioResult, SegIndex};
use crate::hls::video_fetch::VideoClip;
use crate::source::SourceInfo;
use crate::stems::Stems;

/// `@2`: segment-based video (video in `video/`, stems as [`StemSpan`]s);
/// `@1` projects (clips) are migrated on load, see `project::migrate`.
pub const SCHEMA: &str = "cliper/project@2";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Project {
    pub schema: String,
    pub id: String,
    /// what the user typed (URL / id)
    pub input: String,
    pub created_at: String,
    pub updated_at: String,
    /// bumped on every save; pass it back as `base_revision` to detect
    /// concurrent changes (optimistic concurrency)
    #[serde(default)]
    pub revision: u64,
    pub source: Option<SourceInfo>,
    pub audio: Option<AudioState>,
    pub detection: Option<Detection>,
    pub settings: Settings,
    pub songs: Vec<Song>,
    /// deprecated (schema @1): always empty since the segment store; kept so
    /// older clients still parse the project. Video: `video.*` methods.
    #[serde(default)]
    pub clips: Vec<Clip>,
    /// separated audio, each a contiguous VOD range (sorted, non-overlapping)
    #[serde(default)]
    pub stems: Vec<StemSpan>,
    /// talk found in the vocal stem (VOD time), the source of mute suggestions
    #[serde(default)]
    pub talk: Vec<(f64, f64)>,
    /// VOD ranges already searched for talk: suggestions are only ever added
    /// for newly separated ranges, so a deleted suggestion never comes back
    #[serde(default)]
    pub talk_scanned: Vec<(f64, f64)>,
    /// 📌 keep: cleanup refuses this project unless forced
    #[serde(default)]
    pub pinned: bool,
    /// is the VOD (replay) still online? (`vod.check`)
    #[serde(default)]
    pub vod: Option<VodStatus>,
    /// last failed job of this project (cleared when a job of that kind succeeds)
    #[serde(default)]
    pub last_error: Option<JobError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum VodState {
    /// the replay can be played (video can be fetched)
    Alive,
    /// deleted / private / expired: nothing more can be fetched
    Gone,
    /// couldn't check (offline, login needed, …)
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VodStatus {
    pub status: VodState,
    pub checked_at: String,
    /// why (`VOD_NOT_FOUND`, `AUTH_REQUIRED`, `NETWORK`, …) when not alive
    #[serde(default)]
    pub code: Option<String>,
}

/// Separated audio of one contiguous VOD range: `orig.wav` (the video's own
/// audio, 48 kHz stereo float) and `vocals.wav` (same length), plus their
/// 100/s peaks, in `dir`. Sample k is VOD time `start + k / sample_rate`.
/// A span's folder never changes once written: growing a span writes a new
/// one (new id) and drops the old.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct StemSpan {
    pub id: String,
    /// VOD time of sample 0 (on the sample grid)
    pub start: f64,
    pub end: f64,
    pub frames: u64,
    pub sample_rate: u32,
    /// project-relative folder: `orig.wav`, `vocals.wav`, `orig.peaks`, `vocals.peaks`
    pub dir: String,
    pub model: String,
    pub device: crate::ml::runtime::Device,
    pub created_at: String,
}

impl StemSpan {
    pub fn file(&self, name: &str) -> String {
        format!("{}/{name}", self.dir)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct JobError {
    pub kind: String,
    pub code: String,
    pub message: String,
    pub at: String,
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
    /// the audio file was deleted by cleanup (level `audio`); fetch it again to
    /// listen / align. The index is kept (it places the video segments).
    #[serde(default)]
    pub dropped: bool,
    /// local recordings: the audio track copied (None = remote / older projects)
    #[serde(default)]
    pub track: Option<u32>,
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
            dropped: false,
            track: None,
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
    /// music-like stretches the detector rejected (possible missed songs)
    #[serde(default)]
    pub candidates: Vec<Candidate>,
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
    /// video rendition height: the best one at or below it (None = highest).
    /// Default 1080: chzzk's original, and SOOP's 1440p doubles the size.
    pub video_height: Option<u32>,
    /// local recordings: audio track used for analysis, separation and export
    /// (0 = first; pick an OBS mix track when recording multitrack)
    pub audio_track: u32,
    /// suggest vocal mutes over detected talk after separation
    pub suggest_mutes: bool,
    /// default fade length for new mutes (s)
    pub mute_fade: f64,
    /// default strength for new mutes: None = remove the voice completely,
    /// Some(-12.0) = only turn it down by 12 dB
    pub mute_gain_db: Option<f64>,
    /// fade at the very start / end of each exported song (s)
    pub song_fade_in: f64,
    pub song_fade_out: f64,
    /// export: "h264_nvenc" | "libx264" | "copy"
    pub video_codec: String,
    /// export quality (CQ for nvenc, CRF for x264); unused when `video_bitrate_k` is set
    pub video_quality: u32,
    /// export height, e.g. 1080 / 720 / 480 / 360 (aspect kept, never
    /// upscaled); None = the fetched video's size. Ignored with "copy".
    pub export_height: Option<u32>,
    /// export video bitrate in kbps (e.g. 4000); None = constant quality.
    /// Ignored with "copy".
    pub video_bitrate_k: Option<u32>,
    /// mp4 AAC bitrate
    pub audio_bitrate_k: u32,
    /// loudness normalisation of exported songs (None = keep the original level)
    pub loudness: Option<Loudness>,
    pub output_dir: Option<String>,
    /// output file name (without extension). Tokens: {date} (broadcast date
    /// YYYY-MM-DD), {yymmdd}, {platform}, {vod_id}, {channel}, {vod_title},
    /// {idx} (2-digit song number), {title}, {artist}, {start} (h-mm-ss in the VOD).
    /// Unsafe characters are replaced; an existing file gets `_v2`, `_v3`, ….
    pub filename_template: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            margin_before: 30.0,
            margin_after: 30.0,
            video_height: Some(1080),
            audio_track: 0,
            suggest_mutes: true,
            mute_fade: 0.3,
            mute_gain_db: None,
            song_fade_in: 0.3,
            song_fade_out: 1.5,
            video_codec: "h264_nvenc".into(),
            video_quality: 23,
            export_height: None,
            video_bitrate_k: None,
            audio_bitrate_k: 320,
            loudness: None,
            output_dir: None,
            filename_template: "{vod_id}_{idx}_{title}".into(),
        }
    }
}

/// Target loudness for export (EBU R128 / ITU-R BS.1770 via ffmpeg `loudnorm`, linear mode).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Loudness {
    /// integrated loudness target, LUFS (YouTube ≈ −14)
    pub target_lufs: f64,
    /// true-peak ceiling, dBTP (e.g. −1)
    pub true_peak: f64,
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
    /// original artist
    #[serde(default)]
    pub artist: String,
    pub start: f64,
    pub end: f64,
    pub origin: Origin,
    /// excluded songs are kept (so detection reruns don't resurrect them) but not exported
    #[serde(default = "yes")]
    pub enabled: bool,
    /// the user has reviewed this song's boundaries
    #[serde(default)]
    pub reviewed: bool,
    /// free-form note (e.g. "retake", "medley", "2:13 talk audible")
    #[serde(default)]
    pub note: String,
    /// detector confidence (auto songs; None for manual songs)
    #[serde(default)]
    pub confidence: Option<Confidence>,
    pub clip_id: Option<String>,
    #[serde(default)]
    pub mutes: Vec<Mute>,
    /// apply this song's confirmed mutes on export (false = keep the original
    /// audio, mutes are kept for later)
    #[serde(default = "yes")]
    pub mute_enabled: bool,
    /// fade at the song's start / end on export (None = settings.song_fade_in/out)
    #[serde(default)]
    pub fade_in: Option<f64>,
    #[serde(default)]
    pub fade_out: Option<f64>,
    pub export: Option<ExportInfo>,
}

impl Song {
    /// VOD range of video wanted for this song (song ± margins).
    pub fn with_margins(&self, s: &Settings) -> (f64, f64) {
        ((self.start - s.margin_before).max(0.0), self.end + s.margin_after)
    }
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
    /// approved by the user. Only confirmed mutes are applied on export;
    /// suggestions start unconfirmed, user-made mutes are confirmed.
    #[serde(default)]
    pub confirmed: bool,
    /// None = remove the voice completely; Some(db) = attenuate by `db` (negative)
    #[serde(default)]
    pub gain_db: Option<f64>,
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
    /// songs assigned to this clip (maintained by clip planning)
    #[serde(default)]
    pub song_ids: Vec<String>,
    /// planned for specific songs (`clips.fetch {exact}`) or being fetched:
    /// kept by re-planning even before its video exists
    #[serde(default)]
    pub pinned: bool,
    /// how `offset` was obtained (None until the video is fetched)
    #[serde(default)]
    pub alignment: Option<Alignment>,
}

/// Clip ↔ analysis-audio alignment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Alignment {
    /// "measured" (cross-correlation confirmed) | "predicted" (from the segment
    /// index only — the audio didn't match; check this clip's timing)
    pub method: String,
    /// normalised correlation of the best window (0..1)
    pub corr: f32,
    /// offset predicted from the segment index (s)
    pub predicted: f64,
    /// false = could not be confirmed; the UI should flag the clip
    pub ok: bool,
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
            revision: 0,
            source: None,
            audio: None,
            detection: None,
            settings: Settings::default(),
            songs: Vec::new(),
            clips: Vec::new(),
            stems: Vec::new(),
            talk: Vec::new(),
            talk_scanned: Vec::new(),
            pinned: false,
            vod: None,
            last_error: None,
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

    /// Does this song carry user work that detection must not throw away?
    /// (manual, reviewed, titled, noted, muted, disabled, exported)
    pub fn is_touched(&self, s: &Song) -> bool {
        s.origin == Origin::Manual
            || s.reviewed
            || !s.enabled
            || !s.title.is_empty()
            || !s.artist.is_empty()
            || !s.note.is_empty()
            || !s.mutes.is_empty()
            || !s.mute_enabled
            || s.fade_in.is_some()
            || s.fade_out.is_some()
            || s.export.is_some()
    }

    /// Replace untouched auto songs with fresh detector output, keeping manual
    /// songs and every song the user touched (see [`Project::is_touched`]).
    pub fn apply_detection(&mut self, a: &Analysis) {
        let touched: Vec<bool> = self.songs.iter().map(|s| self.is_touched(s)).collect();
        let keep: Vec<Song> = self.songs.drain(..).zip(touched).filter(|(_, t)| *t).map(|(s, _)| s).collect();
        let overlaps = |a: (f64, f64), b: (f64, f64)| a.0 < b.1 && b.0 < a.1;
        let mut songs = keep;
        for (k, &seg) in a.segments.iter().enumerate() {
            if songs.iter().any(|s| overlaps((s.start, s.end), seg)) {
                continue;
            }
            songs.push(Song {
                id: new_id("s"),
                title: String::new(),
                artist: String::new(),
                start: seg.0,
                end: seg.1,
                origin: Origin::Auto,
                enabled: true,
                reviewed: false,
                note: String::new(),
                confidence: a.confidence.get(k).cloned(),
                clip_id: None,
                mutes: Vec::new(),
                mute_enabled: true,
                fade_in: None,
                fade_out: None,
                export: None,
            });
        }
        songs.sort_by(|a, b| a.start.total_cmp(&b.start));
        self.songs = songs;
    }
}
