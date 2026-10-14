//! Engine: project-level operations shared by the IPC server (and tests).
//!
//! Quick operations are plain async methods returning the updated [`Project`].
//! Long operations run as **jobs**: they return a job id at once and report
//! through [`Event`]s (progress → done | failed), cancellable via [`Engine::cancel`].
//! Every project mutation is load → modify → atomic save under a per-project
//! lock, followed by a `ProjectChanged` event carrying the new state.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use crate::analysis::detect::{self, Params};
use crate::audio::{ffmpeg, peaks, wav};
use crate::export::{ExportSpec, OutputFormat, StemsSpec, export_song, export_stems};
use crate::hls::audio_extract::{self, ExtractOptions};
use crate::hls::video_fetch;
use crate::ml::panns::{self, Probs, Tagger};
use crate::ml::runtime;
use crate::ml::separator::ModelSpec;
use crate::progress::{CancellationToken, Progress, ProgressFn};
use crate::project::report::{self, ReportFormat};
use crate::project::setlist::{self, SetlistMatch};
use crate::cookies::{CookieJar, CookieStatus};
use crate::project::{self, Detection, JobError, naming, Mute, Origin, Project, Settings, Song, Store};
use crate::ranges::{self, Range};
use crate::render::MuteSpan;
use crate::source::{ResolveError, Source};
use crate::storage::{DropLevel, StorageUsage};
use crate::video::store as vstore;

#[path = "engine_video.rs"]
mod video_impl;
#[path = "engine_stems.rs"]
mod stems_impl;

pub use stems_impl::EnsureResult;
pub use video_impl::{FetchArgs, FetchPlan, VideoView};
use video_impl::VideoMgr;

/// `lock()` that survives a panic in another holder (the data is still usable
/// for this engine's simple maps; a poisoned lock must not take the engine down).
pub(crate) trait LockExt<T> {
    fn lk(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lk(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "panic".into())
}

/// How long a failed resolve (network) is reused instead of trying again.
const RESOLVE_FAIL_TTL: Duration = Duration::from_secs(30);

/// Re-resolve playback URLs older than this (tokens expire after a few hours).
const SOURCE_TTL: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub projects_root: PathBuf,
    pub models_dir: PathBuf,
    pub prefer_cpu: bool,
    /// keep the PC awake while jobs run; None = the saved preference (default on)
    pub keep_awake: Option<bool>,
    /// app preferences file (`serve --config`); None = `<projects>/engine.json`
    pub prefs_path: Option<PathBuf>,
}

impl EngineConfig {
    pub fn prefs_file(&self) -> PathBuf {
        self.prefs_path.clone().unwrap_or_else(|| self.projects_root.join("engine.json"))
    }
}

/// Machine-readable error for clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ApiError {
    /// INVALID_PARAMS | NOT_FOUND | BUSY | CONFLICT | ADDON | UNRECOGNIZED_INPUT | AUTH_REQUIRED | VOD_NOT_FOUND |
    /// DUPLICATE_PROJECT | VIDEO_BUSY | ROOT_LOCKED | SCHEMA_NEWER |
    /// NETWORK | CDN_TRUNCATED | FORMAT | FFMPEG_MISSING | FFMPEG_FAILED | MODEL_MISSING |
    /// MODEL | CANCELLED | PRECONDITION | IO | INTERNAL | VIDEO_MISSING | VOD_GONE | NO_SPACE | PINNED
    pub code: String,
    pub message: String,
    /// details, e.g. `{ranges}` for VIDEO_MISSING, `{need, free}` for NO_SPACE
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub data: Option<Value>,
}

impl ApiError {
    pub fn new(code: &str, message: impl std::fmt::Display) -> Self {
        Self { code: code.into(), message: message.to_string(), data: None }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

macro_rules! api_from {
    ($t:ty, $f:expr) => {
        impl From<$t> for ApiError {
            fn from(e: $t) -> Self {
                $f(e)
            }
        }
    };
}

fn net_code(e: &crate::http::NetError) -> &'static str {
    match e {
        crate::http::NetError::Truncated { .. } => "CDN_TRUNCATED",
        _ => "NETWORK",
    }
}

api_from!(ResolveError, |e: ResolveError| {
    let code = match &e {
        ResolveError::Unrecognized(_) => "UNRECOGNIZED_INPUT",
        ResolveError::NotFound(_) => "VOD_NOT_FOUND",
        ResolveError::AuthRequired(_) => "AUTH_REQUIRED",
        ResolveError::Malformed(_) => "FORMAT",
        ResolveError::Net(n) => net_code(n),
    };
    ApiError::new(code, e)
});
api_from!(project::store::StoreError, |e: project::store::StoreError| match e {
    project::store::StoreError::NotFound(_) => ApiError::new("NOT_FOUND", e),
    project::store::StoreError::Newer(..) => ApiError::new("SCHEMA_NEWER", e),
    _ => ApiError::new("IO", e),
});
api_from!(std::io::Error, |e| ApiError::new("IO", e));
api_from!(ffmpeg::FfmpegError, |e: ffmpeg::FfmpegError| match e {
    ffmpeg::FfmpegError::Missing => ApiError::new("FFMPEG_MISSING", e),
    _ => ApiError::new("FFMPEG_FAILED", e),
});
api_from!(runtime::ModelError, |e: runtime::ModelError| match e {
    runtime::ModelError::Missing(_) => ApiError::new("MODEL_MISSING", e),
    _ => ApiError::new("MODEL", e),
});
api_from!(panns::PannsError, |e: panns::PannsError| match e {
    panns::PannsError::Model(m) => m.into(),
    panns::PannsError::Cancelled(_) => ApiError::new("CANCELLED", "cancelled"),
    panns::PannsError::Io(e) => e.into(),
});
api_from!(audio_extract::ExtractError, |e: audio_extract::ExtractError| match e {
    audio_extract::ExtractError::Net(n) => ApiError::new(net_code(&n), n),
    audio_extract::ExtractError::Cancelled(_) => ApiError::new("CANCELLED", "cancelled"),
    audio_extract::ExtractError::Io(e) => e.into(),
    audio_extract::ExtractError::Format(m) => ApiError::new("FORMAT", m),
});
api_from!(video_fetch::VideoError, |e: video_fetch::VideoError| match e {
    video_fetch::VideoError::Net(n) => ApiError::new(net_code(&n), n),
    video_fetch::VideoError::Cancelled(_) => ApiError::new("CANCELLED", "cancelled"),
    video_fetch::VideoError::Ffmpeg(f) => f.into(),
    video_fetch::VideoError::Io(e) => e.into(),
    video_fetch::VideoError::Format(m) => ApiError::new("FORMAT", m),
    video_fetch::VideoError::Internal(m) => ApiError::new("INTERNAL", m),
});
api_from!(crate::stems::StemError, |e: crate::stems::StemError| match e {
    crate::stems::StemError::Ffmpeg(f) => f.into(),
    crate::stems::StemError::Io(e) => e.into(),
    crate::stems::StemError::Separate(crate::ml::separator::SeparateError::Model(m)) => m.into(),
    crate::stems::StemError::Separate(crate::ml::separator::SeparateError::Cancelled(_)) => ApiError::new("CANCELLED", "cancelled"),
    crate::stems::StemError::Separate(crate::ml::separator::SeparateError::OutOfMemory) => ApiError::new("MODEL", "GPU out of memory"),
    crate::stems::StemError::Separate(crate::ml::separator::SeparateError::Io(e)) => e.into(),
});
api_from!(crate::export::ExportError, |e: crate::export::ExportError| match e {
    crate::export::ExportError::Ffmpeg(f) => f.into(),
    crate::export::ExportError::Io(e) => e.into(),
    crate::export::ExportError::Cancelled(_) => ApiError::new("CANCELLED", "cancelled"),
    crate::export::ExportError::Invalid(m) => ApiError::new("INVALID_PARAMS", m),
});
api_from!(tokio::task::JoinError, |e| ApiError::new("INTERNAL", e));
api_from!(crate::local::LocalError, |e: crate::local::LocalError| match e {
    crate::local::LocalError::Missing(_) => ApiError::new("NOT_FOUND", e),
    crate::local::LocalError::NoAudio(_) => ApiError::new("FORMAT", e),
    crate::local::LocalError::Ffmpeg(f) => f.into(),
    crate::local::LocalError::Io(e) => e.into(),
});

/// Notifications pushed to the client.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// `items`: what the job will process, in order (clip ids for clips.fetch,
    /// song ids for export.run; empty for single-step jobs)
    JobStarted { job_id: String, kind: String, project_id: String, items: Vec<String> },
    JobProgress { job_id: String, kind: String, project_id: String, progress: Progress },
    JobDone { job_id: String, kind: String, project_id: String, result: Value },
    JobFailed { job_id: String, kind: String, project_id: String, error: ApiError },
    ProjectChanged { project: Box<Project> },
    QueueChanged { queue: Vec<crate::queue::QueueEntry> },
    /// coverage / requests of a project's video changed (throttled, at most 4/s)
    VideoChanged { video: Box<crate::video::VideoState> },
    /// a `video.fetch` request ended: `status` "done" | "failed" | "cancelled";
    /// `failed` = ranges that couldn't be fetched, `code` = why (first failure)
    VideoRequestDone { project_id: String, request_id: String, status: String, failed: Vec<Range>, code: Option<String> },
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct JobInfo {
    pub job_id: String,
    pub kind: String,
    pub project_id: String,
    pub started_at: String,
}

struct JobEntry {
    info: JobInfo,
    cancel: CancellationToken,
}

/// Handle given to a running job.
#[derive(Clone)]
pub struct JobCtx {
    pub job_id: String,
    pub cancel: CancellationToken,
    pub progress: ProgressFn,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SystemInfo {
    pub version: String,
    pub projects_root: PathBuf,
    pub models_dir: PathBuf,
    pub ffmpeg: Option<String>,
    pub cuda_dirs: Vec<PathBuf>,
    pub models: BTreeMap<String, bool>,
    pub prefer_cpu: bool,
    /// None = no NVIDIA GPU found
    pub gpu: Option<GpuInfo>,
    pub keep_awake: bool,
    /// app preferences file in use
    pub prefs_path: PathBuf,
    /// settings files that could not be read and were set aside (A-3)
    pub warnings: Vec<String>,
    pub cookies: CookieStatus,
    /// the CUDA runtime add-on (install it when `needed`)
    pub cuda_addon: crate::addon::CudaAddon,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct GpuInfo {
    pub name: String,
    pub vram_mb: u64,
    /// ONNX Runtime's CUDA provider started (None = not tried yet; see system.probe_gpu)
    pub cuda_ok: Option<bool>,
    /// why CUDA is not used
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ProjectSummary {
    pub id: String,
    pub title: Option<String>,
    pub channel: Option<String>,
    pub platform: Option<String>,
    pub duration: Option<f64>,
    pub songs: usize,
    pub exported: usize,
    pub thumbnail: Option<String>,
    pub updated_at: String,
    /// enabled songs whose boundaries aren't reviewed yet
    pub unreviewed: usize,
    /// mute suggestions waiting for confirmation (enabled songs)
    pub unconfirmed_mutes: usize,
    /// enabled songs whose whole range is separated (formerly `clips_ready`)
    pub separated_songs: usize,
    /// kept out of cleanup (always sent by the engine; optional so clients
    /// that build summaries themselves may leave it out)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub pinned: Option<bool>,
    /// last replay check (absent = never checked)
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub vod: Option<project::VodStatus>,
    pub has_audio: bool,
    pub has_detection: bool,
    pub last_error: Option<JobError>,
    /// absolute folder of the project (projects outside the projects folder
    /// too: `project.open`, `projects.add_root`); always sent by the engine
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub dir: Option<PathBuf>,
}

pub struct Engine {
    pub cfg: EngineConfig,
    pub store: Store,
    events: UnboundedSender<Event>,
    jobs: Mutex<HashMap<String, JobEntry>>,
    sources: tokio::sync::Mutex<HashMap<String, (Source, Instant)>>,
    /// project → lock making concurrent `source()` calls share one resolve
    resolve_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// project → last resolve that failed for network reasons
    resolve_fail: Mutex<HashMap<String, (Instant, ApiError)>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    tagger: Arc<Mutex<Option<Tagger>>>,
    /// separation session, kept open between clips; holding its lock also
    /// serializes GPU separation across jobs
    separator: Mutex<Option<crate::ml::separator::Separator>>,
    cookies: Mutex<CookieJar>,
    keep_awake: AtomicBool,
    /// app preferences and this folder's location registry (A-6); each
    /// changes under its own lock so concurrent updates don't lose writes
    prefs: Mutex<crate::prefs::AppPrefs>,
    locations: Mutex<crate::prefs::Locations>,
    /// problems met loading settings (reported in `system.info`)
    warnings: Mutex<Vec<String>>,
    /// exclusive use of the projects folder (taken by [`Engine::try_new`])
    _root_lock: Option<crate::prefs::RootLock>,
    /// ADTS frame byte offsets of a project's analysis audio (audio.window)
    frame_index: Mutex<HashMap<String, FrameIndex>>,
    /// results of finished jobs, for `wait_job` (kept until read)
    finished: Mutex<FinishedJobs>,
    job_finished: tokio::sync::Notify,
    pub(crate) queue: Mutex<Vec<crate::queue::QueueEntry>>,
    pub(crate) queue_wake: tokio::sync::Notify,
    pub(crate) queue_worker: AtomicBool,
    /// segment store + fetch scheduler
    pub(crate) video: VideoMgr,
    /// project → lock serialising changes to its stems (ensure, export, cleanup)
    stems_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// when the last job ended (model release counts idle time from here)
    last_job_end: Mutex<Instant>,
}

/// Keep Windows from sleeping while jobs run (unattended overnight batches).
/// Every 30 s, if any job is running, the system idle timer is reset.
fn keep_awake_while_busy(eng: std::sync::Weak<Engine>) {
    std::thread::Builder::new()
        .name("keep-awake".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(30));
                let Some(e) = eng.upgrade() else { return };
                let busy = !e.jobs.lk().is_empty() && e.keep_awake.load(Ordering::Relaxed);
                drop(e);
                #[cfg(windows)]
                if busy {
                    use windows_sys::Win32::System::Power::{ES_SYSTEM_REQUIRED, SetThreadExecutionState};
                    // SAFETY: plain Win32 call without pointers
                    unsafe { SetThreadExecutionState(ES_SYSTEM_REQUIRED) };
                }
                #[cfg(not(windows))]
                let _ = busy;
            }
        })
        .ok();
}

/// Drop the ONNX sessions (detection, separation) once no job has run for
/// [`MODEL_IDLE`], so an idle app doesn't sit on GPU memory (and the process
/// commit that mirrors it on Windows). Reopening costs a few seconds.
const MODEL_IDLE: Duration = Duration::from_secs(60);

/// Idle for [`MODEL_IDLE`] since both the first idle check and the end of the last job?
fn idle_long_enough(idle_since: Instant, last_job_end: Instant, now: Instant) -> bool {
    now.saturating_duration_since(idle_since.max(last_job_end)) >= MODEL_IDLE
}

fn release_models_when_idle(eng: std::sync::Weak<Engine>) {
    std::thread::Builder::new()
        .name("model-idle".into())
        .spawn(move || {
            let mut idle_since: Option<Instant> = None;
            loop {
                std::thread::sleep(Duration::from_secs(10));
                let Some(e) = eng.upgrade() else { return };
                if !e.jobs.lk().is_empty() {
                    idle_since = None;
                    continue;
                }
                let since = *idle_since.get_or_insert_with(Instant::now);
                // a job that started and ended between two checks counts too (QA-14)
                if !idle_long_enough(since, *e.last_job_end.lk(), Instant::now()) {
                    continue;
                }
                // a model in use holds its lock; leave it for the next round
                let mut freed = false;
                if let Ok(mut t) = e.tagger.try_lock()
                    && t.take().is_some()
                {
                    freed = true;
                }
                if let Ok(mut s) = e.separator.try_lock()
                    && s.take().is_some()
                {
                    freed = true;
                }
                if freed {
                    tracing::info!("released idle models");
                }
            }
        })
        .ok();
}

fn gpu_name() -> Option<(String, u64)> {
    static GPU: OnceLock<Option<(String, u64)>> = OnceLock::new();
    GPU.get_or_init(runtime::gpu_name).clone()
}

/// Encoders `export.run` understands.
const VIDEO_CODECS: [&str; 3] = ["h264_nvenc", "libx264", "copy"];

fn validate_encode(height: Option<u32>, video_k: Option<u32>, audio_k: Option<u32>) -> ApiResult<()> {
    let range = |v: f64, lo: f64, hi: f64, what: &str| -> ApiResult<()> {
        if (lo..=hi).contains(&v) { Ok(()) } else { Err(invalid(format!("{what} must be within {lo}..{hi}"))) }
    };
    if let Some(h) = height {
        range(h as f64, 144.0, 4320.0, "export_height")?;
    }
    if let Some(k) = video_k {
        range(k as f64, 100.0, 100_000.0, "video_bitrate_k")?;
    }
    if let Some(k) = audio_k {
        range(k as f64, 32.0, 512.0, "audio_bitrate_k")?;
    }
    Ok(())
}

fn validate_settings(s: &Settings) -> ApiResult<()> {
    let range = |v: f64, lo: f64, hi: f64, what: &str| -> ApiResult<()> {
        if v.is_finite() && (lo..=hi).contains(&v) { Ok(()) } else { Err(invalid(format!("{what} must be within {lo}..{hi}"))) }
    };
    range(s.margin_before, 0.0, 300.0, "margin_before")?;
    range(s.margin_after, 0.0, 300.0, "margin_after")?;
    range(s.mute_fade, 0.0, 5.0, "mute_fade")?;
    if let Some(g) = s.mute_gain_db {
        range(g, -120.0, 0.0, "mute_gain_db")?;
    }
    range(s.song_fade_in, 0.0, 30.0, "song_fade_in")?;
    range(s.song_fade_out, 0.0, 30.0, "song_fade_out")?;
    if !VIDEO_CODECS.contains(&s.video_codec.as_str()) {
        return Err(invalid(format!("video_codec must be one of {VIDEO_CODECS:?}")));
    }
    range(s.video_quality as f64, 0.0, 51.0, "video_quality")?;
    validate_encode(s.export_height, s.video_bitrate_k, Some(s.audio_bitrate_k))?;
    if let Some(l) = s.loudness {
        range(l.target_lufs, -70.0, 0.0, "loudness.target_lufs")?;
        range(l.true_peak, -9.0, 0.0, "loudness.true_peak")?;
    }
    if s.filename_template.trim().is_empty() {
        return Err(invalid("filename_template must not be empty"));
    }
    Ok(())
}

/// Give duplicate song / mute ids fresh ones (the first keeps its id).
fn dedupe_ids(songs: &mut [Song]) {
    let mut seen = std::collections::HashSet::new();
    let mut seen_m = std::collections::HashSet::new();
    for s in songs.iter_mut() {
        if !s.id.is_empty() && !seen.insert(s.id.clone()) {
            s.id = project::new_id("s");
        }
        for m in s.mutes.iter_mut() {
            if !m.id.is_empty() && !seen_m.insert(m.id.clone()) {
                m.id = project::new_id("m");
            }
        }
    }
}

fn validate_song(s: &mut Song) -> ApiResult<()> {
    if !(s.start.is_finite() && s.end.is_finite()) || s.end <= s.start || s.start < 0.0 {
        return Err(invalid(format!("song {}: bad range {}–{}", s.id, s.start, s.end)));
    }
    for f in [s.fade_in, s.fade_out].into_iter().flatten() {
        if !f.is_finite() || f < 0.0 {
            return Err(invalid(format!("song {}: bad fade {f}", s.id)));
        }
    }
    if s.id.is_empty() {
        s.id = project::new_id("s");
    }
    for m in s.mutes.iter_mut() {
        validate_mute(m).map_err(|e| invalid(format!("song {}: {}", s.id, e.message)))?;
    }
    s.mutes.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(())
}

fn validate_mute(m: &mut Mute) -> ApiResult<()> {
    if !(m.start.is_finite() && m.end.is_finite())
        || m.end <= m.start
        || !(m.fade_in >= 0.0 && m.fade_out >= 0.0)
        || m.gain_db.is_some_and(|g| !g.is_finite() || g > 0.0)
    {
        return Err(invalid(format!("bad mute {}–{}", m.start, m.end)));
    }
    if m.id.is_empty() {
        m.id = project::new_id("m");
    }
    if m.origin == Origin::Manual {
        m.confirmed = true;
    }
    Ok(())
}

fn check_revision(p: &Project, base: Option<u64>) -> ApiResult<()> {
    match base {
        Some(b) if b != p.revision => Err(ApiError::new("CONFLICT", format!("project changed (revision {} ≠ {b}); reload and retry", p.revision))),
        _ => Ok(()),
    }
}

fn sort_songs(p: &mut Project) {
    p.songs.sort_by(|a, b| a.start.total_cmp(&b.start));
}

/// Byte offset of every ADTS frame in `path` (streamed: only headers are
/// inspected). The engine writes one raw data block (1024 samples) per frame;
/// anything else is rejected so time ↔ frame mapping stays exact.
fn adts_index(path: &Path) -> std::io::Result<Vec<u64>> {
    use std::io::Read;
    let len_total = std::fs::metadata(path)?.len();
    let mut r = std::io::BufReader::with_capacity(1 << 20, std::fs::File::open(path)?);
    let mut out = Vec::with_capacity((len_total / 300) as usize);
    let mut pos = 0u64;
    let mut h = [0u8; 7];
    while pos + 7 <= len_total {
        r.read_exact(&mut h)?;
        if h[0] != 0xFF || h[1] & 0xF0 != 0xF0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("ADTS sync lost at byte {pos}")));
        }
        if h[6] & 3 != 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "ADTS frames with several raw data blocks are not supported"));
        }
        let len = (((h[3] & 3) as u64) << 11) | ((h[4] as u64) << 3) | ((h[5] as u64) >> 5);
        if len < 7 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("bad ADTS frame at byte {pos}")));
        }
        out.push(pos);
        pos += len;
        if pos < len_total {
            r.seek_relative(len as i64 - 7)?;
        }
    }
    Ok(out)
}

/// Where the analysis audio of a project lives and how to cut it.
#[derive(Clone)]
struct AnalysisSource {
    path: PathBuf,
    /// ADTS frame offsets (remote VODs); None = seekable container (local .mka)
    index: Option<Arc<Vec<u64>>>,
    samples_per_frame: f64,
    sample_rate: f64,
}

impl AnalysisSource {
    /// Decode `[start, start+duration)` seconds of the analysis timeline
    /// (decoded-sample time), sample-exact. Blocking.
    fn decode(&self, start: f64, duration: f64, rate: u32, channels: u32) -> ApiResult<Vec<f32>> {
        use std::io::{Read, Seek, SeekFrom};
        let start = start.max(0.0);
        match &self.index {
            // whole ADTS frames with 2 frames of decoder pre-roll, then trim exactly
            Some(idx) if !idx.is_empty() => {
                let (spf, sr) = (self.samples_per_frame, self.sample_rate);
                let k = ((start * sr / spf).floor() as usize).saturating_sub(2).min(idx.len() - 1);
                let k_end = (((start + duration) * sr / spf).ceil() as usize + 2).min(idx.len());
                let mut f = std::fs::File::open(&self.path)?;
                let end = match idx.get(k_end) {
                    Some(&x) => x,
                    None => f.metadata()?.len(),
                };
                f.seek(SeekFrom::Start(idx[k]))?;
                let mut bytes = vec![0u8; (end - idx[k]) as usize];
                f.read_exact(&mut bytes)?;
                // decoded sample 0 is frame k's first sample; trim by counting
                // samples (ffmpeg's -ss on piped raw AAC uses estimated
                // timestamps, which drift on VBR audio)
                let pcm = ffmpeg::decode_bytes(bytes, "aac", rate, channels, 0.0, (k_end - k) as f64 * spf / sr + 1.0)?;
                let ch = channels as usize;
                let a = ((start - k as f64 * spf / sr) * rate as f64).round() as usize * ch;
                let n = (duration * rate as f64).round() as usize * ch;
                Ok(pcm.get(a.min(pcm.len())..(a + n).min(pcm.len())).unwrap_or_default().to_vec())
            }
            Some(_) => Ok(Vec::new()),
            None => {
                let mut rd = ffmpeg::decode(&self.path, rate, channels, Some(start), Some(duration))?;
                let mut out = Vec::new();
                let mut buf = vec![0f32; 1 << 16];
                loop {
                    let n = rd.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    out.extend_from_slice(&buf[..n]);
                }
                Ok(out)
            }
        }
    }
}

/// Builds `overview.peaks` while the analysis audio is still being written:
/// tails the growing ADTS file into an ffmpeg decoder and snapshots the peaks
/// every few seconds. `until_ms` = how far the written peaks reach.
struct PeaksFollower {
    done: Arc<AtomicBool>,
    until_ms: Arc<AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

const FOLLOW_RATE: u32 = 8000;

impl PeaksFollower {
    fn start(audio: PathBuf, out: PathBuf) -> Self {
        let done = Arc::new(AtomicBool::new(false));
        let until_ms = Arc::new(AtomicU64::new(0));
        let (d, u) = (done.clone(), until_ms.clone());
        let handle = std::thread::Builder::new()
            .name("peaks-follow".into())
            .spawn(move || {
                if let Err(e) = follow(&audio, &out, &d, &u) {
                    tracing::warn!("live overview peaks stopped: {e}");
                }
            })
            .ok();
        Self { done, until_ms, handle }
    }

    /// Stop after the rest of the file is consumed; writes the final peaks.
    fn finish(mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for PeaksFollower {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
    }
}

fn follow(audio: &Path, out: &Path, done: &AtomicBool, until_ms: &AtomicU64) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let mut c = std::process::Command::new(ffmpeg::ffmpeg_path());
    c.args(["-hide_banner", "-v", "error", "-f", "aac", "-i", "pipe:0", "-vn", "-ac", "1", "-ar", &FOLLOW_RATE.to_string(), "-f", "f32le", "pipe:1"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let mut child = c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn()?;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let builder = Arc::new(Mutex::new(peaks::PeakBuilder::new(FOLLOW_RATE, 20)));
    let reader = {
        let b = builder.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0u8; 1 << 16];
            let mut carry: Vec<u8> = Vec::new();
            loop {
                let n = match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                carry.extend_from_slice(&buf[..n]);
                let whole = carry.len() / 4 * 4;
                let samples: Vec<f32> = carry[..whole].chunks_exact(4).map(|x| f32::from_le_bytes(x.try_into().unwrap())).collect();
                carry.drain(..whole);
                b.lk().push(&samples);
            }
        })
    };
    let snapshot = |b: &Mutex<peaks::PeakBuilder>| -> std::io::Result<()> {
        let pk = b.lk().snapshot();
        if pk.data.is_empty() {
            return Ok(());
        }
        pk.write(out)?;
        until_ms.store((pk.data.len() as f64 / pk.rate * 1000.0) as u64, Ordering::Relaxed);
        Ok(())
    };

    // the extractor creates the file; wait for it
    let mut file = loop {
        match std::fs::File::open(audio) {
            Ok(f) => break f,
            Err(_) if !done.load(Ordering::Relaxed) => std::thread::sleep(Duration::from_millis(200)),
            Err(e) => return Err(e),
        }
    };
    let mut buf = vec![0u8; 1 << 18];
    let mut last_snap = Instant::now();
    loop {
        let n = file.read(&mut buf)?;
        if n > 0 {
            if stdin.write_all(&buf[..n]).is_err() {
                break;
            }
        } else if done.load(Ordering::Relaxed) {
            // drain anything written between the last read and `done`
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let _ = stdin.write_all(&buf[..n]);
            continue;
        } else {
            std::thread::sleep(Duration::from_millis(250));
        }
        if last_snap.elapsed() > Duration::from_secs(3) {
            last_snap = Instant::now();
            snapshot(&builder)?;
        }
    }
    drop(stdin);
    let _ = reader.join();
    let _ = child.wait();
    snapshot(&builder)
}

/// (audio file length when indexed, ADTS frame offsets)
type FrameIndex = (u64, Arc<Vec<u64>>);

/// Results of finished jobs (for `wait_job`) and their insertion order.
type FinishedJobs = (HashMap<String, ApiResult<Value>>, std::collections::VecDeque<String>);

/// Current stage of a job for the ETA: (stage, item, started, `done` at start).
type StageClock = (String, Option<String>, Instant, u64);

/// Job kinds that must not run together on one project: audio.fetch replaces
/// the analysis audio that detect.run and clips.fetch (alignment) read.
fn conflicts(a: &str, b: &str) -> bool {
    let pair = |x: &str, y: &str| (a == x && b == y) || (a == y && b == x);
    pair("audio.fetch", "detect.run") || pair("audio.fetch", "clips.fetch") || pair("audio.fetch", "stems.ensure") || pair("audio.fetch", "export.run")
}

fn is_local(p: &Project) -> bool {
    p.source.as_ref().is_some_and(|s| s.platform == crate::source::Platform::Local)
}

/// Absolute path without the `\\?\` prefix canonicalize adds on Windows
/// (ffmpeg and concat lists don't take it).
fn canonical(p: &Path) -> Option<PathBuf> {
    let c = std::fs::canonicalize(p).ok()?;
    let s = c.to_string_lossy();
    Some(match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => c,
    })
}

fn invalid(msg: impl std::fmt::Display) -> ApiError {
    ApiError::new("INVALID_PARAMS", msg)
}

fn precondition(msg: impl std::fmt::Display) -> ApiError {
    ApiError::new("PRECONDITION", msg)
}

impl Engine {
    /// An engine without the projects-folder lock (tests, embedding).
    pub fn new(cfg: EngineConfig, events: UnboundedSender<Event>) -> Arc<Self> {
        Self::build(cfg, events, None)
    }

    /// An engine with exclusive use of its projects folder: fails with
    /// `ROOT_LOCKED` while another engine uses it (A-6).
    pub fn try_new(cfg: EngineConfig, events: UnboundedSender<Event>) -> ApiResult<Arc<Self>> {
        let _ = std::fs::create_dir_all(&cfg.projects_root);
        let lock = crate::prefs::lock_root(&cfg.projects_root)
            .map_err(|m| ApiError::new("ROOT_LOCKED", m).with_data(json!({ "root": cfg.projects_root })))?;
        Ok(Self::build(cfg, events, Some(lock)))
    }

    fn build(cfg: EngineConfig, events: UnboundedSender<Event>, lock: Option<crate::prefs::RootLock>) -> Arc<Self> {
        let store = Store::new(&cfg.projects_root);
        let _ = std::fs::create_dir_all(&cfg.projects_root);
        let cookies = CookieJar::load(&cfg.projects_root);
        let mut warnings = Vec::new();
        let (prefs, w) = crate::prefs::load_prefs(&cfg.prefs_file(), &cfg.projects_root);
        warnings.extend(w);
        let (locs, w) = crate::prefs::load_locations(&cfg.projects_root);
        warnings.extend(w);
        // the split files are written once so the old engine.json is no longer needed
        if !crate::prefs::locations_path(&cfg.projects_root).exists()
            && locs != crate::prefs::Locations::default()
            && let Err(e) = crate::prefs::save_guarded(&crate::prefs::locations_path(&cfg.projects_root), &locs)
        {
            tracing::warn!("locations.json: {e}");
        }
        if !cfg.prefs_file().exists() {
            let _ = crate::prefs::save_guarded(&cfg.prefs_file(), &prefs);
        }
        let keep_awake = cfg.keep_awake.unwrap_or(prefs.keep_awake);
        let video_paused = prefs.video_paused;
        for (id, d) in store.set_locations(locs.project_roots.clone(), locs.project_dirs.clone()) {
            tracing::warn!("project {id} at {} is hidden by another project with the same id", d.display());
        }
        let eng = Arc::new(Self {
            cfg,
            store,
            events,
            jobs: Mutex::new(HashMap::new()),
            sources: tokio::sync::Mutex::new(HashMap::new()),
            resolve_locks: Mutex::new(HashMap::new()),
            resolve_fail: Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            tagger: Arc::new(Mutex::new(None)),
            separator: Mutex::new(None),
            cookies: Mutex::new(cookies),
            keep_awake: AtomicBool::new(keep_awake),
            prefs: Mutex::new(prefs),
            locations: Mutex::new(locs),
            warnings: Mutex::new(warnings),
            _root_lock: lock,
            frame_index: Mutex::new(HashMap::new()),
            finished: Mutex::new((HashMap::new(), std::collections::VecDeque::new())),
            job_finished: tokio::sync::Notify::new(),
            queue: Mutex::new(Vec::new()),
            queue_wake: tokio::sync::Notify::new(),
            queue_worker: AtomicBool::new(false),
            video: VideoMgr::new(video_paused),
            stems_locks: Mutex::new(HashMap::new()),
            last_job_end: Mutex::new(Instant::now()),
        });
        keep_awake_while_busy(Arc::downgrade(&eng));
        release_models_when_idle(Arc::downgrade(&eng));
        eng.video_start();
        eng
    }

    pub(crate) fn emit_event(&self, e: Event) {
        self.emit(e);
    }

    /// Wait for a job started by this engine to finish; its result or error.
    pub async fn wait_job(&self, job_id: &str) -> ApiResult<Value> {
        loop {
            let notified = self.job_finished.notified();
            {
                // results are recorded before a job leaves `jobs`, so checking
                // both under the `finished` lock can't miss a finishing job
                let mut f = self.finished.lk();
                if let Some(r) = f.0.remove(job_id) {
                    return r;
                }
                if !self.jobs.lk().contains_key(job_id) {
                    return Err(ApiError::new("NOT_FOUND", format!("job {job_id}")));
                }
            }
            notified.await;
        }
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    // ── system ──────────────────────────────────────────────────────────

    pub fn system_info(&self) -> SystemInfo {
        let rt = runtime::init();
        static FFMPEG: OnceLock<Option<String>> = OnceLock::new();
        let ffmpeg = FFMPEG
            .get_or_init(|| {
                let mut c = std::process::Command::new(ffmpeg::ffmpeg_path());
                c.arg("-version");
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    c.creation_flags(0x0800_0000);
                }
                c.output().ok().filter(|o| o.status.success()).and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_string))
            })
            .clone();
        let mut models = BTreeMap::new();
        models.insert("panns_sed.onnx".to_string(), self.cfg.models_dir.join("panns_sed.onnx").is_file());
        models.insert("panns_labels.json".to_string(), self.cfg.models_dir.join("panns_labels.json").is_file());
        let mdx = ModelSpec::voc_ft();
        models.insert(mdx.file.clone(), self.cfg.models_dir.join(&mdx.file).is_file());
        SystemInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            projects_root: self.cfg.projects_root.clone(),
            models_dir: self.cfg.models_dir.clone(),
            ffmpeg,
            cuda_dirs: rt.cuda_dirs.clone(),
            models,
            prefer_cpu: self.cfg.prefer_cpu,
            gpu: self.gpu_info(),
            keep_awake: self.keep_awake.load(Ordering::Relaxed),
            prefs_path: self.cfg.prefs_file(),
            warnings: self.warnings.lk().clone(),
            cookies: self.cookies.lk().status(),
            cuda_addon: crate::addon::cuda_status(gpu_name().is_some() && !self.cfg.prefer_cpu, &rt.cuda_dirs),
        }
    }

    fn gpu_info(&self) -> Option<GpuInfo> {
        let (name, vram_mb) = gpu_name()?;
        let (cuda_ok, reason) = if self.cfg.prefer_cpu {
            (Some(false), Some("CPU mode (--cpu)".to_string()))
        } else {
            match runtime::cuda_status() {
                Some(st) => (Some(st.ok), st.reason),
                None => (None, None),
            }
        };
        Some(GpuInfo { name, vram_mb, cuda_ok, reason })
    }

    /// Start the CUDA provider now (loads the detection model, a few seconds)
    /// so the UI knows before the first job whether the GPU works.
    pub async fn probe_gpu(self: &Arc<Self>) -> ApiResult<SystemInfo> {
        let eng = self.clone();
        let r = tokio::task::spawn_blocking(move || -> ApiResult<()> {
            let mut slot = eng.tagger.lk();
            eng.tagger(&mut slot)?;
            Ok(())
        })
        .await?;
        if let Err(e) = r {
            tracing::warn!("gpu probe: {e}");
        }
        Ok(self.system_info())
    }

    /// Login cookies used for resolving (e.g. chzzk `NID_AUT`/`NID_SES`);
    /// persisted (encrypted with DPAPI on Windows). None clears them.
    pub fn set_cookies(&self, cookies: Option<BTreeMap<String, String>>) -> ApiResult<CookieStatus> {
        for (k, v) in cookies.iter().flatten() {
            let name_ok = !k.is_empty() && k.chars().all(|c| c.is_ascii_graphic() && !"()<>@,;:\\\"/[]?={}".contains(c));
            let value_ok = v.chars().all(|c| c.is_ascii_graphic() && c != ';' && c != ',' && c != '"');
            if !name_ok || !value_ok {
                return Err(invalid(format!("cookie {k:?} has characters a Cookie header can't carry")));
            }
        }
        let mut jar = self.cookies.lk();
        jar.set(cookies)?;
        Ok(jar.status())
    }

    pub fn cookies_status(&self) -> CookieStatus {
        self.cookies.lk().status()
    }

    /// Ask the platform whether the stored cookies are still logged in.
    /// chzzk is checked; SOOP can't be checked without a VOD (valid stays null).
    pub async fn check_cookies(&self) -> ApiResult<CookieStatus> {
        let cookies = self.cookies.lk().get();
        let Some(c) = cookies else {
            return Ok(self.cookies_status());
        };
        let chzzk: BTreeMap<String, String> = c.iter().filter(|(k, _)| k.starts_with("NID_")).map(|(k, v)| (k.clone(), v.clone())).collect();
        let (valid, account) = if chzzk.is_empty() {
            (None, None)
        } else {
            let (ok, nick) = crate::platforms::chzzk::check_login(&chzzk).await.map_err(|e| ApiError::new("NETWORK", e))?;
            (Some(ok), nick)
        };
        let mut jar = self.cookies.lk();
        jar.record_check(valid, account);
        Ok(jar.status())
    }

    /// Download + install the CUDA add-on (job on the pseudo project
    /// `_system`). The engine must be restarted afterwards to use it.
    /// `zip`: install from a local copy of the add-on zip instead of downloading
    /// (same pinned checksum) — for a private release, or offline PCs.
    pub fn job_install_cuda(self: &Arc<Self>, zip: Option<PathBuf>) -> ApiResult<String> {
        if let Some(z) = &zip
            && !z.is_file() {
                return Err(invalid(format!("not a file: {}", z.display())));
            }
        self.spawn_job("addon.install_cuda", "_system", Vec::new(), move |_eng, ctx| async move {
            let r = match zip {
                Some(z) => crate::addon::install_cuda_from_file(&z, ctx.progress.clone(), ctx.cancel.clone()).await,
                None => crate::addon::install_cuda(ctx.progress.clone(), ctx.cancel.clone()).await,
            };
            let dir = r.map_err(|e| match e {
                crate::addon::AddonError::Cancelled => ApiError::new("CANCELLED", "cancelled"),
                crate::addon::AddonError::Net(m) => ApiError::new("NETWORK", m),
                e => ApiError::new("ADDON", e),
            })?;
            Ok(json!({ "dir": dir, "restart_required": true }))
        })
    }

    pub fn remove_cuda_addon(&self) -> ApiResult<()> {
        if self.jobs.lk().values().any(|j| j.info.kind == "addon.install_cuda") {
            return Err(ApiError::new("BUSY", "the CUDA add-on is being installed"));
        }
        crate::addon::remove_cuda()?;
        Ok(())
    }

    /// Keep the PC awake while jobs run (persisted).
    pub fn set_keep_awake(&self, enabled: bool) -> ApiResult<()> {
        self.keep_awake.store(enabled, Ordering::Relaxed);
        self.update_prefs(|p| p.keep_awake = enabled)
    }

    /// Change and save the app preferences (one writer at a time).
    pub(crate) fn update_prefs(&self, f: impl FnOnce(&mut crate::prefs::AppPrefs)) -> ApiResult<()> {
        let mut p = self.prefs.lk();
        f(&mut p);
        crate::prefs::save_guarded(&self.cfg.prefs_file(), &*p)?;
        Ok(())
    }

    // ── jobs ────────────────────────────────────────────────────────────

    pub fn jobs(&self) -> Vec<JobInfo> {
        self.jobs.lk().values().map(|j| j.info.clone()).collect()
    }

    pub fn cancel(&self, job_id: &str) -> bool {
        match self.jobs.lk().get(job_id) {
            Some(j) => {
                j.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Start `f` as a background job. One job of each kind per project at a time.
    fn spawn_job<F, Fut>(self: &Arc<Self>, kind: &str, project_id: &str, items: Vec<String>, f: F) -> ApiResult<String>
    where
        F: FnOnce(Arc<Self>, JobCtx) -> Fut + Send + 'static,
        Fut: Future<Output = ApiResult<Value>> + Send + 'static,
    {
        let job_id = project::new_id("j");
        let cancel = CancellationToken::new();
        {
            let mut jobs = self.jobs.lk();
            if let Some(j) = jobs.values().find(|j| j.info.project_id == project_id && (j.info.kind == kind || conflicts(&j.info.kind, kind))) {
                return Err(ApiError::new("BUSY", format!("{} is running for {project_id}", j.info.kind)));
            }
            let info = JobInfo { job_id: job_id.clone(), kind: kind.into(), project_id: project_id.into(), started_at: project::now() };
            jobs.insert(job_id.clone(), JobEntry { info, cancel: cancel.clone() });
        }
        let (kind_s, pid) = (kind.to_string(), project_id.to_string());
        let progress: ProgressFn = {
            let (eng, job_id, kind, pid) = (Arc::downgrade(self), job_id.clone(), kind_s.clone(), pid.clone());
            // throttle: at most ~10 progress events/s per job, notes always pass
            let last = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(1)));
            // (stage, item, started, done at start) for the stage ETA
            let stage: Arc<Mutex<StageClock>> = Arc::new(Mutex::new((String::new(), None, Instant::now(), 0)));
            let items = Arc::new(items.clone());
            Arc::new(move |mut p: Progress| {
                if !p.is_note() {
                    let mut st = stage.lk();
                    if st.0 != p.stage || st.1 != p.item || p.done < st.3 {
                        *st = (p.stage.clone(), p.item.clone(), Instant::now(), p.done);
                    }
                    let (did, el) = (p.done.saturating_sub(st.3), st.2.elapsed().as_secs_f64());
                    if did > 0 && p.total > p.done && el > 1.0 {
                        p.eta_sec = Some((el / did as f64 * (p.total - p.done) as f64).round());
                    }
                }
                if !items.is_empty() {
                    p.items_total = Some(items.len() as u64);
                    if p.item_index.is_none() {
                        p.item_index = p.item.as_ref().and_then(|it| items.iter().position(|x| x == it)).map(|i| i as u64);
                    }
                }
                let mut l = last.lk();
                if !p.is_note() && p.peaks_until.is_none() && p.done != p.total && l.elapsed() < Duration::from_millis(100) {
                    return;
                }
                *l = Instant::now();
                if let Some(e) = eng.upgrade() {
                    e.emit(Event::JobProgress { job_id: job_id.clone(), kind: kind.clone(), project_id: pid.clone(), progress: p });
                }
            })
        };
        self.emit(Event::JobStarted { job_id: job_id.clone(), kind: kind_s.clone(), project_id: pid.clone(), items });
        let ctx = JobCtx { job_id: job_id.clone(), cancel, progress };
        let eng = self.clone();
        let jid = job_id.clone();
        tokio::spawn(async move {
            use futures::FutureExt;
            // a panic must not leave the job BUSY forever or hang wait_job
            let res = std::panic::AssertUnwindSafe(f(eng.clone(), ctx))
                .catch_unwind()
                .await
                .unwrap_or_else(|p| Err(ApiError::new("INTERNAL", format!("job panicked: {}", panic_message(p)))));
            {
                // remember the outcome for wait_job (bounded, oldest evicted first),
                // then retire the job — in this order, under the `finished` lock
                let mut f = eng.finished.lk();
                f.0.insert(jid.clone(), res.clone());
                f.1.push_back(jid.clone());
                while f.1.len() > 256 {
                    if let Some(old) = f.1.pop_front() {
                        f.0.remove(&old);
                    }
                }
                eng.jobs.lk().remove(&jid);
                *eng.last_job_end.lk() = Instant::now();
            }
            eng.job_finished.notify_waiters();
            eng.record_outcome(&pid, &kind_s, &res).await;
            match res {
                Ok(result) => eng.emit(Event::JobDone { job_id: jid, kind: kind_s, project_id: pid.clone(), result }),
                Err(error) => eng.emit(Event::JobFailed { job_id: jid, kind: kind_s, project_id: pid.clone(), error }),
            }
            // separation queued by fetches that finished while a job blocked it
            eng.after_stems_job(&pid);
        });
        Ok(job_id)
    }

    /// Remember a failed job on the project (for the home screen); clear it
    /// when a job of the same kind succeeds. Cancellation is not an error.
    async fn record_outcome(&self, pid: &str, kind: &str, res: &ApiResult<Value>) {
        let err = match res {
            Err(e) if e.code == "CANCELLED" => return,
            Err(e) => Some(JobError { kind: kind.into(), code: e.code.clone(), message: e.message.clone(), at: project::now() }),
            Ok(_) => None,
        };
        let stale = self.store.load(pid).ok().and_then(|p| p.last_error).is_some_and(|e| e.kind == kind);
        if err.is_none() && !stale {
            return;
        }
        let _ = self
            .mutate(pid, |p| {
                p.last_error = err;
                Ok(())
            })
            .await;
    }

    // ── projects ────────────────────────────────────────────────────────

    fn lock_for(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lk().entry(id.to_string()).or_default().clone()
    }

    /// Load → `f` → save → `ProjectChanged`.
    pub async fn mutate<F>(&self, id: &str, f: F) -> ApiResult<Project>
    where
        F: FnOnce(&mut Project) -> ApiResult<()>,
    {
        let lock = self.lock_for(id);
        let _g = lock.lock().await;
        let mut p = self.store.load(id)?;
        f(&mut p)?;
        self.store.save(&mut p)?;
        self.emit(Event::ProjectChanged { project: Box::new(p.clone()) });
        Ok(p)
    }

    pub fn get(&self, id: &str) -> ApiResult<Project> {
        Ok(self.store.load(id)?)
    }

    pub async fn list_async(&self) -> ApiResult<Vec<ProjectSummary>> {
        let store = self.store.clone();
        Ok(tokio::task::spawn_blocking(move || summarize(&store, store.list())).await?)
    }

    pub fn list(&self) -> Vec<ProjectSummary> {
        summarize(&self.store, self.store.list())
    }

    /// Cancel every running job (shutdown).
    pub fn cancel_all(&self) {
        for j in self.jobs.lk().values() {
            j.cancel.cancel();
        }
    }
}

fn summarize(store: &Store, projects: Vec<Project>) -> Vec<ProjectSummary> {
    projects
        .into_iter()
            .map(|p| ProjectSummary {
                dir: Some(store.dir(&p.id)),
                id: p.id.clone(),
                title: p.source.as_ref().map(|s| s.title.clone()),
                channel: p.source.as_ref().map(|s| s.channel.clone()),
                platform: p.source.as_ref().map(|s| s.platform.as_str().to_string()),
                duration: p.source.as_ref().map(|s| s.duration),
                songs: p.songs.iter().filter(|s| s.enabled).count(),
                exported: p.songs.iter().filter(|s| s.export.is_some()).count(),
                thumbnail: p.source.as_ref().and_then(|s| s.catalog.thumbnail.clone()),
                unreviewed: p.songs.iter().filter(|s| s.enabled && !s.reviewed).count(),
                unconfirmed_mutes: p.songs.iter().filter(|s| s.enabled).flat_map(|s| &s.mutes).filter(|m| !m.confirmed).count(),
                separated_songs: {
                    let sep: Vec<Range> = p.stems.iter().map(|x| (x.start, x.end)).collect();
                    p.songs.iter().filter(|s| s.enabled && ranges::covers(&sep, (s.start, s.end), 0.05)).count()
                },
                pinned: Some(p.pinned),
                vod: p.vod.clone(),
                has_audio: p.audio.as_ref().is_some_and(|a| !a.dropped),
                has_detection: p.detection.is_some(),
                last_error: p.last_error.clone(),
                updated_at: p.updated_at,
            })
            .collect()
}

impl Engine {
    pub async fn delete(&self, id: &str) -> ApiResult<()> {
        let lock = self.lock_for(id);
        let _g = lock.lock().await;
        if self.jobs.lk().values().any(|j| j.info.project_id == id) {
            return Err(ApiError::new("BUSY", "project has running jobs"));
        }
        // in-flight downloads would recreate the folder (R-21)
        if self.with_ps(id, |ps| !ps.idx.requests.is_empty() || !ps.fetching.is_empty()).unwrap_or(false) {
            return Err(ApiError::new("VIDEO_BUSY", "video is being fetched for this project; cancel it first (video.cancel)"));
        }
        self.forget_ps(id);
        let store = self.store.clone();
        let idc = id.to_string();
        let external = self.store.is_external(id);
        tokio::task::spawn_blocking(move || store.delete(&idc)).await??;
        self.frame_index.lk().remove(id);
        self.sources.lock().await.remove(id);
        self.locks.lk().remove(id);
        if external {
            self.apply_locations(|_, dirs| dirs.retain(|d| d.exists()))?;
        }
        Ok(())
    }

    // ── project locations (C2-2) ────────────────────────────────────────

    /// Change the remembered other locations, re-index them, persist.
    /// Returns ids hidden by a project with the same id elsewhere.
    fn apply_locations(&self, f: impl FnOnce(&mut Vec<PathBuf>, &mut Vec<PathBuf>)) -> ApiResult<Vec<(String, PathBuf)>> {
        let mut locs = self.locations.lk();
        let l = &mut *locs;
        f(&mut l.project_roots, &mut l.project_dirs);
        let before: Vec<String> = self.store.ids();
        let skipped = self.store.set_locations(locs.project_roots.clone(), locs.project_dirs.clone());
        crate::prefs::save_guarded(&crate::prefs::locations_path(&self.cfg.projects_root), &*locs)?;
        drop(locs);
        let after: Vec<String> = self.store.ids();
        for id in before.iter().filter(|id| !after.contains(id)) {
            self.forget_ps(id);
        }
        Ok(skipped)
    }

    /// Where projects are listed from: the projects folder, other roots,
    /// single project folders.
    pub fn locations(&self) -> Value {
        let (roots, projects) = self.store.locations();
        json!({ "root": self.cfg.projects_root, "roots": roots, "projects": projects })
    }

    /// Open a project folder that lives elsewhere (moved by hand, another
    /// drive, an old projects folder): it joins `project.list` and stays
    /// there across restarts. Its files stay where they are (all paths inside
    /// a project are relative).
    pub async fn open_project_at(&self, path: &Path) -> ApiResult<Project> {
        let dir = canonical(path).ok_or_else(|| ApiError::new("NOT_FOUND", format!("no folder {}", path.display())))?;
        let id = project::store::project_id_at(&dir).ok_or_else(|| invalid(format!("{} is not a Cliper project folder (no project.json)", path.display())))?;
        let known = self.store.dir(&id);
        if known.join("project.json").is_file() {
            if canonical(&known).as_deref() == Some(dir.as_path()) {
                return self.get(&id);
            }
            return Err(ApiError::new("DUPLICATE_PROJECT", format!("a project with the same id ({id}) is already open from {}", known.display()))
                .with_data(json!({ "id": id, "dir": known })));
        }
        let d = dir.clone();
        self.apply_locations(move |_, dirs| {
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        })?;
        let p = self.get(&id)?;
        self.emit(Event::ProjectChanged { project: Box::new(p.clone()) });
        Ok(p)
    }

    /// Also list every project in another folder (e.g. the previous projects
    /// folder after the user picked a new one). Returns `{projects, skipped}`.
    pub fn add_root(&self, path: &Path) -> ApiResult<Value> {
        let dir = canonical(path).ok_or_else(|| ApiError::new("NOT_FOUND", format!("no folder {}", path.display())))?;
        if canonical(&self.cfg.projects_root).as_deref() == Some(dir.as_path()) {
            return Err(invalid("that is the projects folder itself"));
        }
        // what that folder itself listed from elsewhere comes along (QA-6:
        // projects opened from other places were lost when switching folders)
        let (old, _) = crate::prefs::load_locations(&dir);
        let me = canonical(&self.cfg.projects_root);
        let d = dir.clone();
        let skipped = self.apply_locations(move |roots, dirs| {
            for r in std::iter::once(d).chain(old.project_roots) {
                if !roots.contains(&r) && Some(&r) != me.as_ref() {
                    roots.push(r);
                }
            }
            for x in old.project_dirs {
                if !dirs.contains(&x) {
                    dirs.push(x);
                }
            }
        })?;
        let projects: Vec<ProjectSummary> = self.list().into_iter().filter(|s| s.dir.as_ref().is_some_and(|x| !x.starts_with(&self.cfg.projects_root))).collect();
        let skipped: Vec<Value> = skipped.into_iter().map(|(id, d)| json!({ "id": id, "dir": d })).collect();
        Ok(json!({ "projects": projects, "skipped": skipped }))
    }

    /// Stop listing a root / project folder (nothing is deleted).
    pub fn remove_location(&self, path: &Path) -> ApiResult<bool> {
        let dir = canonical(path).unwrap_or_else(|| path.to_path_buf());
        let mut found = false;
        self.apply_locations(|roots, dirs| {
            let n = roots.len() + dirs.len();
            roots.retain(|r| r != &dir && r != path);
            dirs.retain(|r| r != &dir && r != path);
            found = n != roots.len() + dirs.len();
        })?;
        Ok(found)
    }

    /// Resolve `input` and create (or reopen) its project. Id = `<platform>-<video id>`.
    pub async fn create(&self, input: &str) -> ApiResult<Project> {
        if crate::local::is_local(input) {
            let path = crate::local::normalize(input);
            // an existing project of this file keeps its id (ids from older
            // versions were hashed differently)
            let key = path.to_string_lossy().to_lowercase();
            let store = self.store.clone();
            let existing = tokio::task::spawn_blocking(move || {
                store
                    .list()
                    .into_iter()
                    .find(|p| p.source.as_ref().is_some_and(|s| s.platform == crate::source::Platform::Local && s.url.to_lowercase() == key))
                    .map(|p| p.id)
            })
            .await?;
            let id = existing.unwrap_or_else(|| crate::local::project_id(&path));
            if self.store.exists(&id) {
                let p = self.get(&id)?;
                if p.source.as_ref().is_some_and(|s| s.audio_tracks.is_empty()) {
                    // projects made before audio tracks were listed
                    let info = tokio::task::spawn_blocking(move || crate::local::source_info(&path)).await??;
                    return self
                        .mutate(&id, |p| {
                            if let Some(s) = p.source.as_mut() {
                                s.audio_tracks = info.audio_tracks;
                            }
                            Ok(())
                        })
                        .await;
                }
                return Ok(p);
            }
            let info = tokio::task::spawn_blocking(move || crate::local::source_info(&path)).await??;
            let mut p = self.store.create(&id, &info.url)?;
            p.source = Some(info);
            self.store.save(&mut p)?;
            self.emit(Event::ProjectChanged { project: Box::new(p.clone()) });
            return Ok(p);
        }
        let (platform, vid) = crate::platforms::detect(input)?;
        let id = format!("{}-{vid}", platform.as_str());
        if self.store.exists(&id) {
            return self.get(&id);
        }
        let src = self.resolve_fresh(input).await?;
        let mut p = self.store.create(&id, input)?;
        p.source = Some(src.info.clone());
        project::store::write_atomic(&self.store.path(&id, "raw.json"), &serde_json::to_vec_pretty(&src.raw).unwrap())?;
        self.store.save(&mut p)?;
        self.sources.lock().await.insert(id, (src, Instant::now()));
        self.emit(Event::ProjectChanged { project: Box::new(p.clone()) });
        Ok(p)
    }

    async fn resolve_fresh(&self, input: &str) -> ApiResult<Source> {
        let cookies = self.cookies.lk().get();
        Ok(crate::platforms::resolve(input, cookies.as_ref()).await?)
    }

    /// Resolved source for a project, re-resolving when the cached playback
    /// URLs are getting old. Verifies the segmentation hasn't changed.
    async fn source(&self, p: &Project) -> ApiResult<Source> {
        if is_local(p) {
            return Err(precondition("local recordings have no remote source"));
        }
        // one resolve at a time per project (segment downloads ask concurrently)
        let lock = self.resolve_locks.lk().entry(p.id.clone()).or_default().clone();
        let _g = lock.lock().await;
        // a network failure is remembered briefly: offline, every segment
        // would otherwise wait for its own slow resolve (QA-13)
        if let Some((at, e)) = self.resolve_fail.lk().get(&p.id)
            && at.elapsed() < RESOLVE_FAIL_TTL
        {
            return Err(e.clone());
        }
        {
            let cache = self.sources.lock().await;
            if let Some((s, at)) = cache.get(&p.id)
                && at.elapsed() < SOURCE_TTL {
                    return Ok(s.clone());
                }
        }
        let src = match self.resolve_fresh(&p.input).await {
            Ok(s) => {
                self.resolve_fail.lk().remove(&p.id);
                s
            }
            Err(e) => {
                if e.code == "NETWORK" {
                    self.resolve_fail.lk().insert(p.id.clone(), (Instant::now(), e.clone()));
                }
                return Err(e);
            }
        };
        if let Some(a) = &p.audio {
            let n = a.index.first_segment + a.index.frames.len();
            if src.playlist.segments.len() < n {
                return Err(ApiError::new("FORMAT", "the VOD's segment layout changed since the audio was fetched"));
            }
        }
        self.sources.lock().await.insert(p.id.clone(), (src.clone(), Instant::now()));
        // the platform may reorder, add or drop renditions (A-1): keep the
        // stored list current; folders stay tied to rendition keys
        let key = |l: &[crate::hls::m3u8::Variant]| l.iter().map(vstore::rendition_key).collect::<Vec<_>>();
        if let Some(old) = p.source.as_ref().map(|s| s.variants.clone())
            && key(&old) != key(&src.info.variants)
        {
            tracing::warn!("{}: the rendition list changed: {:?} → {:?}", p.id, key(&old), key(&src.info.variants));
            let fresh = src.info.variants.clone();
            let f2 = fresh.clone();
            let _ = self
                .mutate(&p.id, move |p| {
                    if let Some(s) = p.source.as_mut() {
                        s.variants = f2;
                    }
                    Ok(())
                })
                .await;
            let _ = self.with_ps(&p.id, |ps| {
                ps.variants = fresh;
                ps.refresh_heights();
                ps.dirty = true;
            });
        }
        Ok(src)
    }

    pub async fn update_settings(&self, id: &str, settings: Settings) -> ApiResult<Project> {
        validate_settings(&settings)?;
        self.mutate(id, |p| {
            p.settings = settings;
            Ok(())
        })
        .await
    }

    /// Replace the song list (the UI owns song editing). Validates ranges,
    /// keeps ids, re-plans clips.
    /// `base_revision`: fail with CONFLICT if the project changed since the
    /// client's copy (e.g. a job just added mute suggestions).
    pub async fn set_songs(&self, id: &str, mut songs: Vec<Song>, base_revision: Option<u64>) -> ApiResult<Project> {
        dedupe_ids(&mut songs);
        for s in songs.iter_mut() {
            validate_song(s)?;
        }
        songs.sort_by(|a, b| a.start.total_cmp(&b.start));
        self.mutate(id, |p| {
            check_revision(p, base_revision)?;
            p.songs = songs;
            Ok(())
        })
        .await
    }

    /// Change some fields of one song. `patch` is a partial Song (`id`,
    /// `clip_id`, `export`, `confidence` can't be changed; `mutes` replaces the
    /// song's mute list).
    pub async fn update_song(&self, id: &str, song_id: &str, patch: Value, base_revision: Option<u64>) -> ApiResult<Project> {
        let Value::Object(patch) = patch else { return Err(invalid("patch must be an object")) };
        for k in ["id", "export", "confidence", "uid"] {
            if patch.contains_key(k) {
                return Err(invalid(format!("{k} can't be changed")));
            }
        }
        self.mutate(id, |p| {
            check_revision(p, base_revision)?;
            let s = p.song_mut(song_id).ok_or_else(|| ApiError::new("NOT_FOUND", format!("song {song_id}")))?;
            let mut v = serde_json::to_value(&*s).map_err(|e| ApiError::new("INTERNAL", e))?;
            // only fields a song has (unknown ones would land in `extra`)
            if let Some(k) = patch.keys().find(|k| v.get(k.as_str()).is_none() || s.extra.contains_key(k.as_str())) {
                return Err(invalid(format!("songs have no field {k}")));
            }
            for (k, x) in patch {
                v[k] = x;
            }
            let mut ns: Song = serde_json::from_value(v).map_err(invalid)?;
            validate_song(&mut ns)?;
            *s = ns;
            sort_songs(p);
            Ok(())
        })
        .await
    }

    /// Add one song (id assigned when empty). Returns the project and the song id.
    pub async fn add_song(&self, id: &str, mut song: Song, base_revision: Option<u64>) -> ApiResult<(Project, String)> {
        song.id = String::new();
        song.export = None;
        validate_song(&mut song)?;
        let sid = song.id.clone();
        let p = self
            .mutate(id, |p| {
                check_revision(p, base_revision)?;
                p.songs.push(song);
                sort_songs(p);
                    Ok(())
            })
            .await?;
        Ok((p, sid))
    }

    pub async fn remove_song(&self, id: &str, song_id: &str, base_revision: Option<u64>) -> ApiResult<Project> {
        self.mutate(id, |p| {
            check_revision(p, base_revision)?;
            let n = p.songs.len();
            p.songs.retain(|s| s.id != song_id);
            if p.songs.len() == n {
                return Err(ApiError::new("NOT_FOUND", format!("song {song_id}")));
            }
            Ok(())
        })
        .await
    }

    /// Add (empty / unknown id) or replace (known id) one mute of a song.
    /// Returns the project and the mute id.
    pub async fn upsert_mute(&self, id: &str, song_id: &str, mut mute: Mute, base_revision: Option<u64>) -> ApiResult<(Project, String)> {
        let mut mid = String::new();
        let p = self
            .mutate(id, |p| {
                check_revision(p, base_revision)?;
                let s = p.song_mut(song_id).ok_or_else(|| ApiError::new("NOT_FOUND", format!("song {song_id}")))?;
                if !s.mutes.iter().any(|m| m.id == mute.id) {
                    mute.id = String::new();
                }
                validate_mute(&mut mute)?;
                mid = mute.id.clone();
                match s.mutes.iter_mut().find(|m| m.id == mute.id) {
                    Some(m) => *m = mute,
                    None => s.mutes.push(mute),
                }
                s.mutes.sort_by(|a, b| a.start.total_cmp(&b.start));
                Ok(())
            })
            .await?;
        Ok((p, mid))
    }

    pub async fn remove_mute(&self, id: &str, song_id: &str, mute_id: &str, base_revision: Option<u64>) -> ApiResult<Project> {
        self.mutate(id, |p| {
            check_revision(p, base_revision)?;
            let s = p.song_mut(song_id).ok_or_else(|| ApiError::new("NOT_FOUND", format!("song {song_id}")))?;
            let n = s.mutes.len();
            s.mutes.retain(|m| m.id != mute_id);
            if s.mutes.len() == n {
                return Err(ApiError::new("NOT_FOUND", format!("mute {mute_id}")));
            }
            Ok(())
        })
        .await
    }

    /// Match a pasted setlist / fan timestamps to the songs. `apply` writes
    /// titles/artists (empty fields only unless `overwrite`).
    pub async fn match_setlist(&self, id: &str, text: &str, tolerance: Option<f64>, apply: bool, overwrite: bool) -> ApiResult<SetlistMatch> {
        let tol = tolerance.unwrap_or(150.0).max(0.0);
        if !apply {
            let mut p = self.get(id)?;
            return Ok(setlist::apply(&mut p, text, tol, false, false));
        }
        let mut out = None;
        self.mutate(id, |p| {
            out = Some(setlist::apply(p, text, tol, true, overwrite));
            Ok(())
        })
        .await?;
        Ok(out.unwrap())
    }

    /// Song list as CSV / JSON / YouTube chapters / timestamp text; optionally
    /// also written to `path`.
    pub fn export_metadata(&self, id: &str, format: ReportFormat, include_disabled: bool, path: Option<&str>) -> ApiResult<serde_json::Value> {
        let p = self.get(id)?;
        let text = report::render(&p, format, include_disabled);
        if let Some(path) = path {
            let path = Path::new(path);
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(path, &text)?;
        }
        Ok(json!({ "text": text, "path": path }))
    }

    /// Bytes on disk by kind (S-E8).
    pub fn storage_usage(&self, id: &str) -> ApiResult<StorageUsage> {
        let p = self.get(id)?;
        let dir = self.store.dir(id);
        let used = crate::storage::used_ranges(&p);
        let (video, video_sec, unused_video) = self.with_ps(id, |ps| {
            let legacy_size = |n: usize| ps.idx.legacy.get(n).and_then(|l| std::fs::metadata(dir.join(&l.file)).ok()).map(|m| m.len()).unwrap_or(0);
            let raw: u64 = ps.have.values().filter(|h| matches!(h.kind, vstore::HaveKind::Raw(_))).map(|h| h.bytes).sum();
            let legacy: u64 = (0..ps.idx.legacy.len()).map(legacy_size).sum();
            let unused = match (&ps.tl, ps.local) {
                (Some(tl), false) => {
                    let (r, l) = crate::storage::unused_selection(&ps.have, tl, &used);
                    r.iter().map(|i| ps.have[i].bytes).sum::<u64>() + l.iter().map(|&n| legacy_size(n)).sum::<u64>()
                }
                _ => 0,
            };
            let sec = if ps.local { 0.0 } else { ranges::total(&ps.have_ranges()) };
            (if ps.local { 0 } else { raw + legacy }, sec, unused)
        })?;
        let fsize = |rel: &str| std::fs::metadata(dir.join(rel)).map(|m| m.len()).unwrap_or(0);
        let audio = p.audio.as_ref().map(|a| fsize(&a.file)).unwrap_or(0) + fsize("overview.peaks") + fsize("probs.bin");
        let stems: u64 = p.stems.iter().map(|s| ["orig.wav", "vocals.wav", "orig.peaks", "vocals.peaks"].iter().map(|f| fsize(&s.file(f))).sum::<u64>()).sum();
        let views = self.with_ps(id, |ps| ps.idx.views.clone())?;
        let orphans = crate::storage::orphans(&dir, &p, &views, std::time::Duration::ZERO);
        let orphan: u64 = orphans.iter().map(|o| o.1).sum();
        // orphaned views are counted once, as orphans
        let orphan_views: u64 = orphans.iter().filter(|o| o.0.parent().is_some_and(|d| d.ends_with("views"))).map(|o| o.1).sum();
        let cache = (vstore::dir_size(&dir.join("cache")) + vstore::dir_size(&dir.join(vstore::DIR).join("views"))).saturating_sub(orphan_views);
        let exports = vstore::dir_size(&dir.join("exports"));
        Ok(StorageUsage {
            project_id: id.into(),
            audio,
            video,
            stems,
            unused_video,
            cache,
            exports,
            orphan,
            total: audio + video + stems + cache + orphan,
            pinned: p.pinned,
            video_sec,
            updated_at: p.updated_at,
        })
    }

    /// Usage of every project (+ free space on the projects drive).
    pub async fn storage_all(self: &Arc<Self>) -> ApiResult<Value> {
        let eng = self.clone();
        tokio::task::spawn_blocking(move || {
            let projects: Vec<StorageUsage> = eng.store.list().iter().filter_map(|p| eng.storage_usage(&p.id).ok()).collect();
            Ok(json!({ "projects": projects, "free": crate::storage::free_space(&eng.cfg.projects_root), "root": eng.cfg.projects_root }))
        })
        .await?
    }

    /// Keep (pin) this project out of cleanup.
    pub async fn set_pinned(&self, id: &str, pinned: bool) -> ApiResult<Project> {
        self.mutate(id, |p| {
            p.pinned = pinned;
            Ok(())
        })
        .await
    }

    /// Cleanup (S-E8): delete media up to `level`; decisions always stay.
    /// Refused for pinned projects unless `force`, and while the project's
    /// video is being fetched or a job runs on it. Returns the bytes freed.
    pub async fn media_drop(self: &Arc<Self>, id: &str, level: DropLevel, force: bool) -> ApiResult<(Project, u64)> {
        if self.get(id)?.pinned && !force {
            return Err(ApiError::new("PINNED", "this project is pinned (kept); unpin it or pass force"));
        }
        if let Some(j) = self.jobs.lk().values().find(|j| j.info.project_id == id) {
            return Err(ApiError::new("BUSY", format!("{} is running for {id}", j.info.kind)));
        }
        let lock = self.stems_lock(id);
        let _g = lock.lock().await;
        // A-4: the busy check and the "dropping" mark are one step, so no
        // download starts between them; the scheduler skips the project and
        // video.fetch answers VIDEO_BUSY until the drop is over
        self.with_ps(id, |ps| {
            if !ps.idx.requests.is_empty() || !ps.fetching.is_empty() {
                return Err(ApiError::new("VIDEO_BUSY", "video is being fetched for this project; cancel it first (video.cancel)"));
            }
            ps.dropping = true;
            Ok(())
        })??;
        struct Undrop<'a>(&'a Engine, &'a str);
        impl Drop for Undrop<'_> {
            fn drop(&mut self) {
                let _ = self.0.with_ps(self.1, |ps| ps.dropping = false);
            }
        }
        let _undrop = Undrop(self, id);
        let p = self.get(id)?;
        let dir = self.store.dir(id);
        let before = self.storage_usage(id)?;
        // 1. decide what goes
        let mut files: Vec<PathBuf> = Vec::new();
        let mut dirs: Vec<PathBuf> = Vec::new();
        let mut legacy_gone: Vec<String> = Vec::new();
        if level == DropLevel::Unused {
            // audio windows are a cache too (QA-12: the estimate counted them)
            for e in std::fs::read_dir(dir.join("cache")).into_iter().flatten().flatten() {
                if e.file_name().to_string_lossy().starts_with("window_") {
                    files.push(e.path());
                }
            }
        }
        if level == DropLevel::Unused && !is_local(&p) {
            let used = crate::storage::used_ranges(&p);
            let (raw, legacy, container) = self.with_ps(id, |ps| {
                let tl = ps.tl.clone();
                let (r, l) = tl.as_ref().map(|tl| crate::storage::unused_selection(&ps.have, tl, &used)).unwrap_or_default();
                let raw: Vec<(usize, usize)> = r
                    .iter()
                    .filter_map(|i| match ps.have[i].kind {
                        vstore::HaveKind::Raw(v) => Some((v, *i)),
                        _ => None,
                    })
                    .collect();
                let legacy: Vec<String> = l.iter().filter_map(|&n| ps.idx.legacy.get(n).map(|x| x.file.clone())).collect();
                (raw, legacy, ps.container)
            })?;
            files.extend(raw.into_iter().map(|(v, i)| vstore::segment_path(&dir, v, i, container)));
            legacy_gone = legacy;
        }
        if level >= DropLevel::Stems {
            dirs.extend(p.stems.iter().filter(|s| s.dir.starts_with("stems/")).map(|s| dir.join(&s.dir)));
            for s in p.stems.iter().filter(|s| !s.dir.starts_with("stems/")) {
                files.extend(["orig.wav", "vocals.wav", "orig.peaks", "vocals.peaks"].iter().map(|f| dir.join(s.file(f))));
            }
        }
        if level >= DropLevel::Video {
            for e in std::fs::read_dir(dir.join(vstore::DIR)).into_iter().flatten().flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.strip_prefix('v').is_some_and(|x| x.parse::<usize>().is_ok()) {
                    dirs.push(e.path());
                }
            }
            legacy_gone = self.with_ps(id, |ps| ps.idx.legacy.iter().map(|l| l.file.clone()).collect())?;
        }
        if level >= DropLevel::Audio {
            if let Some(a) = &p.audio {
                files.push(dir.join(&a.file));
            }
            dirs.push(dir.join("cache"));
        }
        // 2. the project and the index stop referring to it (a crash after
        // this leaves only unreferenced files, swept later)
        let p = self
            .mutate(id, |p| {
                if level >= DropLevel::Stems {
                    p.stems.clear();
                }
                if level >= DropLevel::Audio
                    && let Some(a) = p.audio.as_mut()
                {
                    a.dropped = true;
                }
                Ok(())
            })
            .await?;
        self.with_ps(id, |ps| {
            ps.idx.legacy.retain(|l| !legacy_gone.contains(&l.file));
            if level >= DropLevel::Video {
                ps.idx.failed.clear();
            }
            ps.idx_dirty = true;
        })?;
        self.save_index(id);
        // 3. delete
        self.drop_views(id);
        files.extend(legacy_gone.iter().map(|f| dir.join(f)));
        let (eng, idc, pc, dirc) = (self.clone(), id.to_string(), p.clone(), dir.clone());
        tokio::task::spawn_blocking(move || {
            for f in &files {
                let _ = std::fs::remove_file(f);
            }
            for d in &dirs {
                let _ = std::fs::remove_dir_all(d);
            }
            // folders the adopted clips / spans lived in, if now empty
            for f in &legacy_gone {
                if let Some(d) = Path::new(f).parent() {
                    let _ = std::fs::remove_dir(dirc.join(d));
                }
            }
            let _ = std::fs::remove_dir(dirc.join("stems"));
            if level >= DropLevel::Audio {
                eng.frame_index.lk().remove(&idc);
            }
            let views = eng.with_ps(&idc, |ps| ps.idx.views.clone()).unwrap_or_default();
            crate::storage::sweep_orphans(&dirc, &pc, &views);
        })
        .await
        .map_err(|e| ApiError::new("INTERNAL", e))?;
        self.reload_ps(id)?;
        let after = self.storage_usage(id)?;
        Ok((p, before.total.saturating_sub(after.total)))
    }

    /// Decode `[start, start+duration)` (VOD seconds) of the analysis audio to
    /// a 32-bit float WAV for previewing before any video exists. Sample-exact:
    /// VOD time t is decoded sample t·rate of the whole audio (the timeline the
    /// detector and clip alignment use).
    pub async fn audio_window(self: &Arc<Self>, id: &str, start: f64, duration: f64, sample_rate: Option<u32>, channels: Option<u32>) -> ApiResult<Value> {
        let p = self.get(id)?;
        p.audio.as_ref().ok_or_else(|| precondition("fetch the audio first"))?;
        if self.jobs.lk().values().any(|j| j.info.project_id == id && j.info.kind == "audio.fetch") {
            return Err(ApiError::new("BUSY", "the audio is being fetched"));
        }
        if !(start.is_finite() && duration.is_finite()) || start < 0.0 || duration <= 0.0 || duration > 3600.0 {
            return Err(invalid("bad window"));
        }
        let (rate, ch) = (sample_rate.unwrap_or(48000).clamp(8000, 192000), channels.unwrap_or(2).clamp(1, 2));
        let dir = self.store.dir(id).join("cache");
        std::fs::create_dir_all(&dir)?;
        let out = dir.join(format!("window_{}_{}_{rate}_{ch}.wav", (start * 1000.0).round() as u64, (duration * 1000.0).round() as u64));
        if !out.is_file() {
            let eng = self.clone();
            let (pc, outc, dirc) = (p.clone(), out.clone(), dir.clone());
            tokio::task::spawn_blocking(move || -> ApiResult<()> {
                let src = eng.analysis_source(&pc)?;
                let pcm = src.decode(start, duration, rate, ch)?;
                // unique temp name: identical concurrent requests don't collide
                let tmp = dirc.join(format!("tmp_{}.wav", project::new_id("w")));
                wav::write_f32(&tmp, rate, ch as u16, &pcm)?;
                if std::fs::rename(&tmp, &outc).is_err() {
                    let _ = std::fs::remove_file(&tmp); // another request won the race
                }
                prune_cache(&dirc, 24);
                Ok(())
            })
            .await??;
        }
        let rel = out.strip_prefix(self.store.dir(id)).unwrap_or(&out).to_string_lossy().replace('\\', "/");
        Ok(json!({ "path": out, "file": rel, "start": start, "duration": duration, "sample_rate": rate, "channels": ch }))
    }

    /// The project's analysis audio with its frame index (built once, cached;
    /// blocking — call from a worker thread).
    fn analysis_source(&self, p: &Project) -> ApiResult<AnalysisSource> {
        let audio = p.audio.as_ref().ok_or_else(|| precondition("fetch the audio first"))?;
        let path = self.store.path(&p.id, &audio.file);
        let index = if is_local(p) {
            None
        } else {
            let len = std::fs::metadata(&path)?.len();
            let cached = self.frame_index.lk().get(&p.id).filter(|(l, _)| *l == len).map(|(_, i)| i.clone());
            Some(match cached {
                Some(i) => i,
                None => {
                    let idx = Arc::new(adts_index(&path)?);
                    self.frame_index.lk().insert(p.id.clone(), (len, idx.clone()));
                    idx
                }
            })
        };
        Ok(AnalysisSource { path, index, samples_per_frame: audio.index.samples_per_frame as f64, sample_rate: audio.index.sample_rate as f64 })
    }

    /// Engine preference + per-run override for keeping the PC awake.
    pub fn keep_awake(&self) -> bool {
        self.keep_awake.load(Ordering::Relaxed)
    }

    // ── audio ───────────────────────────────────────────────────────────

    pub fn job_fetch_audio(self: &Arc<Self>, id: &str) -> ApiResult<String> {
        let p = self.get(id)?;
        self.spawn_job("audio.fetch", id, Vec::new(), move |eng, ctx| async move {
            // the old probabilities / frame index describe the old audio — except
            // when audio dropped by cleanup comes back unchanged (C2-4): then
            // the detection and probs.bin stay (checked after the download)
            let refetch = p.audio.clone().filter(|a| a.dropped);
            if refetch.is_none() {
                let _ = std::fs::remove_file(eng.store.path(&p.id, "probs.bin"));
            }
            eng.frame_index.lk().remove(&p.id);
            let keep_or_drop = {
                let (eng, pid) = (eng.clone(), p.id.clone());
                move |same: bool| {
                    if !same {
                        let _ = std::fs::remove_file(eng.store.path(&pid, "probs.bin"));
                    }
                    same
                }
            };
            if is_local(&p) {
                let path = PathBuf::from(&p.source.as_ref().unwrap().url);
                let (file, track) = ("audio.mka", p.settings.audio_track);
                let out = eng.store.path(&p.id, "audio.part.mka");
                let fin = eng.store.path(&p.id, file);
                (ctx.progress)(Progress::note("audio.fetch", "copy_audio_track", "copying the audio track"));
                let t0 = Instant::now();
                let duration = tokio::task::spawn_blocking(move || -> ApiResult<f64> {
                    crate::local::extract_audio(&path, track, &out)?;
                    let d = crate::local::probe(&out)?.duration;
                    std::fs::rename(&out, &fin)?;
                    Ok(d)
                })
                .await??;
                let bytes = std::fs::metadata(eng.store.path(&p.id, file)).map(|m| m.len()).unwrap_or(0);
                {
                    let (input, peaks_path) = (eng.store.path(&p.id, file), eng.store.path(&p.id, "overview.peaks"));
                    tokio::task::spawn_blocking(move || -> ApiResult<()> {
                        let mut rd = ffmpeg::decode(&input, FOLLOW_RATE, 1, None, None)?;
                        let mut pk = peaks::PeakBuilder::new(FOLLOW_RATE, 20);
                        let mut buf = vec![0f32; 1 << 16];
                        loop {
                            let n = rd.read(&mut buf)?;
                            if n == 0 {
                                break;
                            }
                            pk.push(&buf[..n]);
                        }
                        pk.finish().write(&peaks_path)?;
                        Ok(())
                    })
                    .await??;
                }
                let state = project::AudioState {
                    file: file.into(),
                    variant: 0,
                    duration,
                    index: crate::hls::audio_extract::SegIndex {
                        first_segment: 0,
                        frames: Vec::new(),
                        durations: Vec::new(),
                        samples_per_frame: 1024,
                        sample_rate: 48000,
                    },
                    fetched_bytes: bytes,
                    full_source_bytes: bytes,
                    fallback_segments: 0,
                    elapsed_sec: t0.elapsed().as_secs_f64(),
                    dropped: false,
                    track: Some(track),
                    rendition: None,
                };
                let same = keep_or_drop(refetch.as_ref().is_some_and(|a| a.track == Some(track) && (a.duration - state.duration).abs() < 0.01));
                let p = eng
                    .mutate(&p.id, |p| {
                        p.audio = Some(state);
                        if !same {
                            p.detection = None;
                        }
                        Ok(())
                    })
                    .await?;
                eng.reload_ps(&p.id)?;
                return Ok(json!({ "audio": p.audio }));
            }
            let src = eng.source(&p).await?;
            let file = "audio.aac";
            // download next to the current audio; it is replaced only on success
            let out = eng.store.path(&p.id, "audio.part.aac");
            let _ = std::fs::remove_file(&out);
            // overview waveform fills in while the audio downloads
            let follower = PeaksFollower::start(out.clone(), eng.store.path(&p.id, "overview.peaks"));
            let progress: ProgressFn = {
                let (inner, until, sent) = (ctx.progress.clone(), follower.until_ms.clone(), Arc::new(AtomicU64::new(0)));
                Arc::new(move |mut pr: Progress| {
                    let u = until.load(Ordering::Relaxed);
                    if u > sent.load(Ordering::Relaxed) {
                        sent.store(u, Ordering::Relaxed);
                        pr.peaks_until = Some(u as f64 / 1000.0);
                    }
                    inner(pr)
                })
            };
            let r = audio_extract::extract_audio(&src, &out, &ExtractOptions::default(), progress, ctx.cancel.clone()).await;
            tokio::task::spawn_blocking(move || follower.finish()).await?;
            let r = match r {
                Ok(r) => r,
                Err(e) => {
                    let _ = std::fs::remove_file(&out);
                    return Err(e.into());
                }
            };
            std::fs::rename(&out, eng.store.path(&p.id, file))?;
            let variant = src.info.audio_variant;
            let rkey = src.info.variants.get(variant).map(vstore::rendition_key);
            // the same audio: same rendition, same AAC frames in every segment
            let same = keep_or_drop(refetch.as_ref().is_some_and(|a| {
                let same_rendition = match (&a.rendition, &rkey) {
                    (Some(x), Some(y)) => x == y,
                    _ => a.variant == variant,
                };
                same_rendition && a.index.first_segment == r.index.first_segment && a.index.frames == r.index.frames && a.index.sample_rate == r.index.sample_rate
            }));
            if refetch.is_some() {
                tracing::info!("{}: re-fetched audio is {}; detection {}", p.id, if same { "unchanged" } else { "different" }, if same { "kept" } else { "dropped" });
            }
            // R-17: a different audio index moves the segment times; what was
            // measured against the old one (alignment, views, VOD-placed
            // stems) no longer fits
            let moved = p.audio.as_ref().is_some_and(|a| {
                a.index.first_segment != r.index.first_segment || a.index.frames != r.index.frames || a.index.sample_rate != r.index.sample_rate
            });
            if moved {
                tracing::warn!("{}: the new audio has a different timeline; alignment, views and stems are reset", p.id);
                eng.drop_views(&p.id);
                eng.with_ps(&p.id, |ps| {
                    ps.idx.align.clear();
                    ps.idx_dirty = true;
                })?;
                eng.save_index(&p.id);
            }
            let p = eng
                .mutate(&p.id, |p| {
                    let mut a = project::AudioState::from_result(file, variant, &r);
                    a.rendition = rkey.clone();
                    p.audio = Some(a);
                    if !same {
                        p.detection = None;
                    }
                    if moved {
                        // their files become leftovers, swept later
                        p.stems.clear();
                    }
                    Ok(())
                })
                .await?;
            // the segment timeline comes from the audio index
            eng.reload_ps(&p.id)?;
            Ok(json!({ "audio": p.audio }))
        })
    }

    // ── detection ───────────────────────────────────────────────────────

    fn tagger<'a>(&self, slot: &'a mut Option<Tagger>) -> ApiResult<&'a mut Tagger> {
        if slot.is_none() {
            *slot = Some(Tagger::open(&self.cfg.models_dir.join("panns_sed.onnx"), self.cfg.prefer_cpu)?);
        }
        Ok(slot.as_mut().unwrap())
    }

    /// Tag the analysis audio (also writes overview peaks), then detect.
    pub fn job_detect(self: &Arc<Self>, id: &str, params: Option<Params>, retag: bool) -> ApiResult<String> {
        let p = self.get(id)?;
        let audio = p.audio.clone().ok_or_else(|| precondition("fetch the audio first"))?;
        self.spawn_job("detect.run", id, Vec::new(), move |eng, ctx| async move {
            let probs_path = eng.store.path(&p.id, "probs.bin");
            // probs.bin is reused only while it belongs to the current audio
            // (audio.fetch deletes it and clears `detection`)
            let cached = if retag || p.detection.is_none() { None } else { std::fs::read(&probs_path).ok().and_then(|b| Probs::from_bytes(&b)) };
            let (probs, device) = match cached {
                Some(pr) => (pr, p.detection.as_ref().map(|d| d.device).unwrap_or(runtime::Device::Cpu)),
                None => {
                    let (eng2, ctx2) = (eng.clone(), ctx.clone());
                    let input = eng.store.path(&p.id, &audio.file);
                    let peaks_path = eng.store.path(&p.id, "overview.peaks");
                    let total = (audio.duration * panns::SAMPLE_RATE as f64) as u64;
                    tokio::task::spawn_blocking(move || -> ApiResult<(Probs, runtime::Device)> {
                        let mut slot = eng2.tagger.lk();
                        let tagger = eng2.tagger(&mut slot)?;
                        let mut rd = ffmpeg::decode(&input, panns::SAMPLE_RATE, 1, None, None)?;
                        let mut pk = peaks::PeakBuilder::new(panns::SAMPLE_RATE, 20);
                        let pr = tagger.probs_streaming(
                            |b| {
                                let n = rd.read(b).map_err(std::io::Error::other)?;
                                pk.push(&b[..n]);
                                Ok(n)
                            },
                            Some(total),
                            &ctx2.progress,
                            &ctx2.cancel,
                        )?;
                        pk.finish().write(&peaks_path)?;
                        project::store::write_atomic(&probs_path, &pr.to_bytes())?;
                        Ok((pr, tagger.device))
                    })
                    .await??
                }
            };
            let params = params.unwrap_or_else(|| p.detection.as_ref().map(|d| d.params.clone()).unwrap_or_default()).sanitized();
            let a = {
                let params = params.clone();
                tokio::task::spawn_blocking(move || detect::analyze(&probs, &params)).await?
            };
            let segs = a.segments.clone();
            let p = eng
                .mutate(&p.id, |p| {
                    p.detection = Some(Detection {
                        probs_file: "probs.bin".into(),
                        params,
                        segments: a.segments.clone(),
                        candidates: a.candidates.clone(),
                        device,
                    });
                    p.apply_detection(&a);
                            Ok(())
                })
                .await?;
            Ok(json!({ "segments": segs, "songs": p.songs.len(), "candidates": a.candidates.len() }))
        })
    }

    /// Re-run the segmenter on cached probabilities (fast, synchronous).
    pub async fn rerun_detection(&self, id: &str, params: Params) -> ApiResult<Project> {
        if self.get(id)?.detection.is_none() {
            return Err(precondition("run detection first"));
        }
        let params = params.sanitized();
        let (path, pc) = (self.store.path(id, "probs.bin"), params.clone());
        let a = tokio::task::spawn_blocking(move || {
            std::fs::read(path).ok().and_then(|b| Probs::from_bytes(&b)).map(|probs| detect::analyze(&probs, &pc))
        })
        .await?
        .ok_or_else(|| precondition("run detection first"))?;
        self.mutate(id, |p| {
            let device = p.detection.as_ref().map(|d| d.device).unwrap_or(runtime::Device::Cpu);
            p.detection = Some(Detection {
                probs_file: "probs.bin".into(),
                params,
                segments: a.segments.clone(),
                candidates: a.candidates.clone(),
                device,
            });
            p.apply_detection(&a);
            Ok(())
        })
        .await
    }

    // ── clips.fetch (protocol 2 compatibility) ──────────────────────────

    /// `clips.fetch`: fetch the video of songs ± margins (default: every
    /// enabled song), wait for it, then separate and suggest mutes — the old
    /// one-call pipeline on top of the segment store. New clients use
    /// `video.fetch {process: true}` / `stems.ensure`.
    pub fn job_fetch_clips(self: &Arc<Self>, id: &str, song_ids: Option<Vec<String>>) -> ApiResult<String> {
        self.job_fetch_clips_with(id, song_ids, vstore::Priority::User)
    }

    /// [`Engine::job_fetch_clips`] at a given fetch priority (the batch queue uses `batch`).
    pub fn job_fetch_clips_with(self: &Arc<Self>, id: &str, song_ids: Option<Vec<String>>, priority: vstore::Priority) -> ApiResult<String> {
        let p = self.get(id)?;
        if p.audio.is_none() {
            return Err(precondition("fetch the audio first"));
        }
        let songs: Vec<String> = song_ids.unwrap_or_else(|| p.songs.iter().filter(|s| s.enabled).map(|s| s.id.clone()).collect());
        if songs.is_empty() {
            return Err(precondition("no songs to fetch (run detection first)"));
        }
        let targets = self.stem_targets(&p, Some(&songs), None)?;
        self.spawn_job("clips.fetch", id, songs, move |eng, ctx| async move {
            let failed = eng.fetch_and_wait(&p.id, &targets, priority, "clips.fetch", &ctx).await?;
            let r = eng.stems_ensure(&p.id, &targets, &ctx).await?;
            Ok(json!({ "clips": [], "video_failed": failed, "stems": r }))
        })
    }

    /// Queue the video of `rs` and wait until the request ends; returns the
    /// ranges still without video. Cancelling the job cancels the request.
    pub(crate) async fn fetch_and_wait(self: &Arc<Self>, pid: &str, rs: &[Range], priority: vstore::Priority, reason: &str, ctx: &JobCtx) -> ApiResult<Vec<Range>> {
        // a job waiting for it fetches even while downloads are paused (R-11)
        let plan = self.video_fetch(pid, FetchArgs { ranges: Some(rs.to_vec()), priority: Some(priority), reason: Some(reason.into()), force: true, ..Default::default() })?;
        if let Some(rid) = plan.request_id {
            self.wait_request(pid, &rid, ctx).await?;
        }
        Ok(ranges::drop_short(ranges::subtract(rs, &self.video_have(pid)?), 0.05))
    }

    /// Wait for a fetch request to leave the queue, reporting `video.fetch` progress.
    async fn wait_request(self: &Arc<Self>, pid: &str, rid: &str, ctx: &JobCtx) -> ApiResult<()> {
        loop {
            if ctx.cancel.is_cancelled() {
                let _ = self.video_cancel(pid, Some(rid));
                return Err(ApiError::new("CANCELLED", "cancelled"));
            }
            let st = self.video_state(pid)?;
            let Some(r) = st.requests.iter().find(|r| r.id == rid) else { return Ok(()) };
            let note = match (&st.offline, r.force && st.paused) {
                (Some(o), _) => format!(" (offline, retrying in {:.0} s)", o.retry_in_sec),
                (None, true) => " (downloading although video is paused)".to_string(),
                _ => String::new(),
            };
            (ctx.progress)(Progress {
                message: Some(format!("video {:.0}/{:.0} s{note}", r.have_sec, r.total_sec)),
                ..Progress::new("video.fetch", (r.have_sec * 1000.0) as u64, (r.total_sec * 1000.0) as u64)
            });
            self.video_wait_change(Duration::from_secs(1)).await;
        }
    }

    // ── export ──────────────────────────────────────────────────────────

    /// VOD ranges of a song that have no video.
    fn song_video_missing(&self, p: &Project, s: &Song) -> ApiResult<Vec<Range>> {
        if is_local(p) {
            return Ok(Vec::new());
        }
        let have = self.video_have(&p.id)?;
        Ok(ranges::drop_short(ranges::subtract(&[(s.start, s.end)], &have), 0.05))
    }

    pub fn job_export(self: &Arc<Self>, id: &str, opts: ExportOptions) -> ApiResult<String> {
        let p = self.get(id)?;
        let songs: Vec<Song> = p
            .songs
            .iter()
            .filter(|s| s.enabled && opts.song_ids.as_ref().is_none_or(|ids| ids.contains(&s.id)))
            .cloned()
            .collect();
        if songs.is_empty() {
            return Err(precondition("no songs to export"));
        }
        if p.audio.is_none() {
            return Err(precondition("fetch the audio first"));
        }
        if let Some(c) = &opts.video_codec
            && !VIDEO_CODECS.contains(&c.as_str())
        {
            return Err(invalid(format!("video_codec must be one of {VIDEO_CODECS:?}")));
        }
        validate_encode(opts.export_height, opts.video_bitrate_k, opts.audio_bitrate_k)?;
        let no_video = p.source.as_ref().is_some_and(|s| s.platform == crate::source::Platform::Local && s.variants.first().is_none_or(|v| v.height == 0));
        if no_video && opts.format.unwrap_or_default().has_video() && (!opts.stems || opts.also_render) {
            return Err(precondition("this recording has no video; export as wav / flac / mp3"));
        }
        let mut missing = Vec::new();
        for s in &songs {
            let m = self.song_video_missing(&p, s)?;
            if !m.is_empty() {
                missing.push(json!({ "song_id": s.id, "ranges": m }));
            }
            if let Some(path) = opts.audio_overrides.get(&s.id).filter(|p| !Path::new(p).is_file()) {
                return Err(invalid(format!("audio override for {} not found: {path}", s.id)));
            }
        }
        if !missing.is_empty() && !opts.fetch {
            return Err(ApiError::new("VIDEO_MISSING", format!("{} song(s) lack video in their range (fetch it, or export with fetch: true)", missing.len()))
                .with_data(json!({
                    "ranges": ranges::normalize(&missing.iter().flat_map(|m| serde_json::from_value::<Vec<Range>>(m["ranges"].clone()).unwrap_or_default()).collect::<Vec<_>>()),
                    "songs": missing,
                    "vod_gone": p.vod.as_ref().is_some_and(|v| v.status == project::VodState::Gone),
                })));
        }
        if !missing.is_empty() && p.vod.as_ref().is_some_and(|v| v.status == project::VodState::Gone) && missing.len() == songs.len() {
            return Err(ApiError::new("VOD_GONE", "the replay is gone and these songs lack video").with_data(json!({ "songs": missing })));
        }
        // songs with all their video first; the others download meanwhile
        let missing_ids: Vec<String> = missing.iter().filter_map(|m| m["song_id"].as_str().map(str::to_string)).collect();
        let mut order: Vec<Song> = songs.iter().filter(|s| !missing_ids.contains(&s.id)).cloned().collect();
        order.extend(songs.iter().filter(|s| missing_ids.contains(&s.id)).cloned());
        let items: Vec<String> = order.iter().map(|s| s.id.clone()).collect();
        self.spawn_job("export.run", id, items, move |eng, ctx| async move {
            // queue the missing video now
            let mut waiting: HashMap<String, String> = HashMap::new();
            for s in order.iter().filter(|s| missing_ids.contains(&s.id)) {
                // an export the user started fetches even while downloads are paused (R-11)
                let args = FetchArgs { ranges: Some(vec![(s.start, s.end)]), song_ids: None, reason: Some("export".into()), force: true, ..Default::default() };
                match eng.video_fetch(&p.id, args) {
                    Ok(plan) => {
                        if let Some(r) = plan.request_id {
                            waiting.insert(s.id.clone(), r);
                        }
                    }
                    Err(e) => tracing::warn!("export: fetching video of {}: {e}", s.id),
                }
            }
            let out_dir = p.settings.output_dir.clone().map(PathBuf::from).unwrap_or_else(|| eng.store.dir(&p.id).join("exports"));
            std::fs::create_dir_all(&out_dir)?;
            let format = opts.format.unwrap_or_default();
            let mut results = Vec::new();
            let mut stems_out = Vec::new();
            let mut failed = Vec::new();
            for (k, s0) in order.iter().enumerate() {
                if ctx.cancel.is_cancelled() {
                    for r in waiting.values() {
                        let _ = eng.video_cancel(&p.id, Some(r));
                    }
                    return Err(ApiError::new("CANCELLED", "cancelled"));
                }
                (ctx.progress)(Progress {
                    message: Some(format!("song {}/{}", k + 1, order.len())),
                    item: Some(s0.id.clone()),
                    ..Progress::new("export.run", k as u64, order.len() as u64)
                });
                let tagged: ProgressFn = {
                    let (inner, sid) = (ctx.progress.clone(), s0.id.clone());
                    Arc::new(move |mut pr: Progress| {
                        pr.item.get_or_insert_with(|| sid.clone());
                        inner(pr)
                    })
                };
                let sctx = JobCtx { progress: tagged.clone(), ..ctx.clone() };
                if let Some(rid) = waiting.get(&s0.id) {
                    eng.wait_request(&p.id, rid, &sctx).await?;
                }
                match eng.export_one(&p.id, &s0.id, &opts, format, &out_dir, &sctx).await {
                    Ok(SongExport::Rendered(r)) => results.push(json!({ "song_id": s0.id, "result": r })),
                    Ok(SongExport::Stems(r, rendered)) => {
                        stems_out.push(json!({ "song_id": s0.id, "stems": r }));
                        if let Some(r) = rendered {
                            results.push(json!({ "song_id": s0.id, "result": r }));
                        }
                    }
                    Err(e) if e.code == "CANCELLED" => return Err(e),
                    Err(e) if order.len() == 1 => return Err(e),
                    Err(e) => {
                        tracing::warn!("export of {} failed: {e}", s0.id);
                        failed.push(json!({ "song_id": s0.id, "error": e }));
                    }
                }
            }
            if results.is_empty() && stems_out.is_empty() && !failed.is_empty() {
                let first = serde_json::from_value::<ApiError>(failed[0]["error"].clone()).unwrap_or_else(|_| ApiError::new("INTERNAL", "export failed"));
                return Err(ApiError { data: Some(json!({ "failed": failed })), ..first });
            }
            Ok(json!({ "exports": results, "stems": stems_out, "failed": failed }))
        })
    }

    /// Export one song from the segment store: stems for its range (separated
    /// now if needed), video cut from whatever pieces cover it.
    async fn export_one(self: &Arc<Self>, pid: &str, sid: &str, opts: &ExportOptions, format: OutputFormat, out_dir: &Path, ctx: &JobCtx) -> ApiResult<SongExport> {
        // the latest edits apply
        let p = self.get(pid)?;
        let s = p.song(sid).cloned().ok_or_else(|| ApiError::new("NOT_FOUND", format!("song {sid}")))?;
        let missing = self.song_video_missing(&p, &s)?;
        if !missing.is_empty() {
            let gone = p.vod.as_ref().is_some_and(|v| v.status == project::VodState::Gone);
            let msg = if gone { "the replay is gone and part of this song has no video" } else { "part of this song has no video" };
            return Err(ApiError::new("VIDEO_MISSING", msg).with_data(json!({ "ranges": missing, "songs": [{ "song_id": sid, "ranges": missing }], "vod_gone": gone })));
        }
        let video = format.has_video() && (!opts.stems || opts.also_render);
        let copy = video && opts.video_codec.as_deref().unwrap_or(&p.settings.video_codec) == "copy";
        let handles = if opts.stems { opts.handles.unwrap_or(3.0).max(0.0) } else { 0.0 };
        // audio window: copy mode may start up to a segment early (keyframe)
        let pre = (if copy { 12.0f64 } else { 0.0 }).max(handles);
        let have = if is_local(&p) { vec![(0.0, f64::MAX)] } else { self.video_have(pid)? };
        let around: Vec<Range> = ranges::intersect(&[((s.start - pre).max(0.0), s.end + handles)], &have);
        let window = around.iter().find(|r| r.0 <= s.start + 0.05 && r.1 >= s.end - 0.05).copied().unwrap_or((s.start, s.end));
        self.stems_ensure(pid, &[window], ctx).await?;
        let p = self.get(pid)?;
        // the separated range around the song bounds the audio window
        let sep = p.stems.iter().find(|x| x.start <= s.start + 1e-6 && x.end >= s.end - 1e-6).map(|x| (x.start, x.end));
        let (a0, a1) = match sep {
            Some((x, y)) => (window.0.max(x), window.1.min(y)),
            None => (s.start, s.end),
        };
        let lock = self.stems_lock(pid);
        let _g = lock.lock().await;
        let all: Vec<&Song> = p.songs.iter().filter(|x| x.enabled).collect();
        let number = all.iter().position(|x| x.id == s.id).map(|i| i + 1).unwrap_or(1);
        let name = naming::limit_len(&naming::render_name(&p.settings.filename_template, &p, &s, number));
        let mutes: Vec<MuteSpan> = s
            .mutes
            .iter()
            .filter(|m| opts.apply_mutes.unwrap_or(true) && s.mute_enabled && (m.confirmed || opts.include_unconfirmed))
            .map(|m| MuteSpan { start: m.start - a0, end: m.end - a0, fade_in: m.fade_in, fade_out: m.fade_out, gain_db: m.gain_db })
            .collect();
        let tmp = self.store.dir(pid).join("cache").join(format!("export_{}", project::new_id("")));
        std::fs::create_dir_all(&tmp)?;
        let _cleanup = TempDir(tmp.clone());
        let eng = self.clone();
        let (pc, tmpc) = (p.clone(), tmp.clone());
        let (orig_wav, vocals_wav) = (tmp.join("orig.wav"), tmp.join("vocals.wav"));
        let (ow, vw) = (orig_wav.clone(), vocals_wav.clone());
        tokio::task::spawn_blocking(move || -> ApiResult<()> {
            let (o, v) = eng.read_stems(&pc, a0, a1)?;
            wav::write_f32(&ow, crate::stems::STEM_RATE, 2, &o)?;
            wav::write_f32(&vw, crate::stems::STEM_RATE, 2, &v)?;
            let _ = tmpc;
            Ok(())
        })
        .await??;
        // the video: one file covering [a0, a1] (VOD time origin at its t = 0)
        let need_video = video || (opts.stems && opts.reference_video);
        let (source_mp4, video_offset, copy_ok) = if need_video {
            let eng = self.clone();
            let (pc, tmpc, cancel) = (p.clone(), tmp.clone(), ctx.cancel.clone());
            let (f, origin, single) = tokio::task::spawn_blocking(move || eng.export_source(&pc, a0, a1, &tmpc, &cancel)).await??;
            (f, a0 - origin, single)
        } else {
            (PathBuf::new(), 0.0, true)
        };
        let latest_codec = opts.video_codec.clone().unwrap_or_else(|| p.settings.video_codec.clone());
        let codec = if latest_codec == "copy" && !copy_ok {
            (ctx.progress)(Progress::note("export.encode", "reencode_joined", "the song joins video of two renditions; re-encoding"));
            "libx264".to_string()
        } else {
            latest_codec
        };
        let mut stems_r = None;
        if opts.stems {
            let spec = StemsSpec {
                source_mp4: source_mp4.clone(),
                orig_wav: orig_wav.clone(),
                vocals_wav: vocals_wav.clone(),
                in_t: s.start - a0,
                out_t: s.end - a0,
                handles,
                mutes: mutes.clone(),
                dir: naming::unique_dir(out_dir, &format!("{name}_stems")),
                name: name.clone(),
                vod_offset: a0,
                video_offset,
                reference_video: opts.reference_video,
            };
            let progress = ctx.progress.clone();
            stems_r = Some(tokio::task::spawn_blocking(move || export_stems(&spec, &progress)).await??);
            if !opts.also_render {
                return Ok(SongExport::Stems(stems_r.unwrap(), None));
            }
        }
        let spec = ExportSpec {
            source_mp4,
            orig_wav,
            vocals_wav,
            audio_override: opts.audio_overrides.get(&s.id).map(PathBuf::from),
            // an override comes back from an editor aligned to the hand-off files' start
            override_offset: (s.start - opts.handles.unwrap_or(3.0).max(0.0)) - a0,
            in_t: s.start - a0,
            out_t: s.end - a0,
            video_offset,
            keep_frame_times: !copy_ok,
            mutes,
            out: naming::unique_path(out_dir, &name, format.extension()),
            format,
            video_codec: codec,
            video_quality: p.settings.video_quality,
            scale_height: opts.export_height.or(p.settings.export_height),
            video_bitrate_k: opts.video_bitrate_k.or(p.settings.video_bitrate_k),
            audio_bitrate_k: opts.audio_bitrate_k.unwrap_or(p.settings.audio_bitrate_k),
            fade_in: s.fade_in.unwrap_or(p.settings.song_fade_in),
            fade_out: s.fade_out.unwrap_or(p.settings.song_fade_out),
            loudness: p.settings.loudness,
            metadata: song_metadata(&p, &s),
        };
        let (cancel, progress) = (ctx.cancel.clone(), ctx.progress.clone());
        let r = tokio::task::spawn_blocking(move || export_song(&spec, &progress, &cancel)).await??;
        let info = project::ExportInfo { path: r.path.to_string_lossy().into_owned(), exported_at: project::now(), duration: r.duration };
        let sidc = s.id.clone();
        self.mutate(pid, |p| {
            if let Some(s) = p.song_mut(&sidc) {
                s.export = Some(info);
            }
            Ok(())
        })
        .await?;
        Ok(match stems_r {
            Some(st) => SongExport::Stems(st, Some(r)),
            None => SongExport::Rendered(r),
        })
    }

    /// A video file holding VOD `[a, b]`: the view / legacy clip / recording
    /// when one file covers it, else a remux of the covering segments; a range
    /// crossing renditions is joined by re-encoding into one intermediate.
    /// Returns (file, VOD time at its t = 0, single-stream = stream-copy safe). Blocking.
    fn export_source(&self, p: &Project, a: f64, b: f64, tmp: &Path, cancel: &CancellationToken) -> ApiResult<(PathBuf, f64, bool)> {
        if is_local(p) {
            // `-ss` is relative to the file's start time
            let m = self.align_local(p)?.m;
            let f = PathBuf::from(&p.source.as_ref().unwrap().url);
            let s0 = crate::video::media::probe_input(&f.to_string_lossy())?.format_start;
            return Ok((f, s0 - m, true));
        }
        let (tl, pieces, dir, container) = self.with_ps(&p.id, |ps| {
            let tl = ps.tl.clone();
            let pcs: Vec<crate::video::media::Piece> = ps.stretches().into_iter().flat_map(|(_, _, pcs)| pcs).collect();
            (tl, pcs, ps.dir.clone(), ps.container)
        })?;
        let tl = tl.ok_or_else(|| precondition("fetch the audio first"))?;
        let segs = tl.segments_for(a, b, crate::video::timeline::EDGE_EPS);
        let mut parts: Vec<(PathBuf, f64, Range)> = Vec::new();
        for pc in pieces {
            let (f, l) = (pc.first().max(segs.start), pc.last().min(segs.end));
            if l <= f {
                continue;
            }
            let span = (tl.start(f).max(a), tl.end(l - 1).min(b));
            match pc {
                crate::video::media::Piece::Legacy { file, origin, .. } => parts.push((file, origin, span)),
                crate::video::media::Piece::Raw { variant, .. } => {
                    let out = tmp.join(format!("src_{variant}_{f}_{l}.mp4"));
                    crate::video::media::remux(&dir, container, &[crate::video::media::Piece::Raw { variant, first: f, last: l }], &out, cancel)?;
                    let origin = self.probe_raw(&p.id, variant, f)?.format_start - self.align_raw(&p.id, variant)?.m;
                    parts.push((out, origin, span));
                }
                crate::video::media::Piece::Local { .. } => {}
            }
        }
        match parts.len() {
            0 => Err(ApiError::new("VIDEO_MISSING", "no video for this range").with_data(json!({ "ranges": [[a, b]] }))),
            1 => {
                let (f, o, _) = parts.remove(0);
                Ok((f, o, true))
            }
            _ => {
                // join on the VOD clock: keep each part's own timestamps
                // (-copyts), cut it to its span, shift it by its origin, merge
                // in time order. Frame times stay exact across the joint
                // (a concat would restart each part at its first frame).
                let out = tmp.join("joined.mp4");
                // origins count from a file's start time (what `-ss` and a
                // decode see); with -copyts the filters see raw timestamps, so
                // shift each origin by its file's start time (an adopted clip
                // starts at e.g. 24 ms: R-1 joins were a frame off)
                for (f, o, _) in parts.iter_mut() {
                    *o -= crate::video::media::probe_input(&f.to_string_lossy())?.format_start;
                }
                let x1 = parts[0].2.0;
                let mut c = ffmpeg::command();
                c.arg("-y").arg("-copyts");
                for (f, o, (x, _)) in &parts {
                    c.args(["-ss", &format!("{:.6}", (x - o - 3.0).max(0.0))]).arg("-i").arg(f);
                }
                // the largest of the parts joined (QA-3: the smallest rendition
                // anywhere in the project made every joined song small); the
                // export's own height / bitrate settings apply when encoding
                let h = parts.iter().filter_map(|(f, _, _)| crate::video::media::video_height(f)).max().unwrap_or(720);
                let mut fc = String::new();
                for (i, (_, o, (x, y))) in parts.iter().enumerate() {
                    fc.push_str(&format!(
                        "[{i}:v:0]trim=start={:.6}:end={:.6},setpts=PTS+({:.6})/TB,scale=-2:{h},setsar=1,format=yuv420p[v{i}];",
                        x - o,
                        y - o,
                        o - x1
                    ));
                }
                for i in 0..parts.len() {
                    fc.push_str(&format!("[v{i}]"));
                }
                fc.push_str(&format!("interleave=nb_inputs={}[v]", parts.len()));
                c.args(["-filter_complex", &fc, "-map", "[v]", "-fps_mode", "passthrough", "-c:v", "libx264", "-preset", "veryfast", "-crf", "12", "-an"]).arg(&out);
                ffmpeg::run_cancellable(c, cancel)?;
                // ffmpeg seeks relative to a file's start time (its first frame here)
                let s0 = crate::video::media::probe_input(&out.to_string_lossy())?.format_start;
                Ok((out, x1 + s0, false))
            }
        }
    }
}

enum SongExport {
    Rendered(crate::export::ExportResult),
    Stems(crate::export::StemsResult, Option<crate::export::ExportResult>),
}

/// Removes a temporary folder on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Options for `export.run`.
#[derive(Debug, Clone, Default, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct ExportOptions {
    /// default: every enabled song
    pub song_ids: Option<Vec<String>>,
    /// default: mp4
    pub format: Option<OutputFormat>,
    /// override settings.video_codec for this run ("copy" = fast, lossless video)
    pub video_codec: Option<String>,
    /// override settings.export_height for this run (e.g. 720)
    pub export_height: Option<u32>,
    /// override settings.video_bitrate_k for this run (e.g. 2000)
    pub video_bitrate_k: Option<u32>,
    /// override settings.audio_bitrate_k for this run (e.g. 128)
    pub audio_bitrate_k: Option<u32>,
    /// apply unconfirmed mute suggestions too (default false: only confirmed)
    pub include_unconfirmed: bool,
    /// false = export the original audio for this run (songs' `mute_enabled`
    /// and mutes stay as they are). Default true.
    pub apply_mutes: Option<bool>,
    /// write editor/DAW stems (+ markers, Reaper project) instead of a rendered file
    pub stems: bool,
    /// with `stems`: also render the normal output
    pub also_render: bool,
    /// with `stems`: seconds of handle before/after the song (default 3)
    pub handles: Option<f64>,
    /// with `stems`: add a light 720p reference video
    pub reference_video: bool,
    /// song id → audio file replacing the rendered audio, aligned to the start
    /// of that song's editor hand-off files (song start − `handles`, default 3 s)
    pub audio_overrides: BTreeMap<String, String>,
    /// songs whose range lacks video: fetch it and export when it arrives
    /// (other songs export meanwhile). false = fail with VIDEO_MISSING.
    pub fetch: bool,
}

/// Keep at most `keep` newest finished windows in a cache dir (in-flight temp
/// files and windows younger than a few seconds are never touched).
fn prune_cache(dir: &Path, keep: usize) {
    let now = std::time::SystemTime::now();
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("window_"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    let young = files.iter().filter(|(t, _)| now.duration_since(*t).unwrap_or_default() < Duration::from_secs(10)).count();
    let keep = keep.max(young);
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    for (_, f) in files.into_iter().skip(keep) {
        let _ = std::fs::remove_file(f);
    }
}

/// Container tags: title, artist, channel, date, source link.
fn song_metadata(p: &Project, s: &Song) -> Vec<(String, String)> {
    let src = p.source.as_ref();
    let mut m = vec![
        ("title".to_string(), s.title.clone()),
        ("artist".to_string(), s.artist.clone()),
        ("album_artist".to_string(), src.map(|x| x.channel.clone()).unwrap_or_default()),
        ("album".to_string(), src.map(|x| x.title.clone()).unwrap_or_default()),
        ("date".to_string(), naming::broadcast_date(p).unwrap_or_default()),
    ];
    if let Some(x) = src {
        let t = s.start.max(0.0) as u64;
        m.push(("comment".into(), format!("{} @ {}:{:02}:{:02}", x.url, t / 3600, t / 60 % 60, t % 60)));
    }
    m
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
