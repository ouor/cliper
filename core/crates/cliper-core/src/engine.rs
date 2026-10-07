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
use crate::analysis::talk::{self, TalkParams};
use crate::audio::{align, ffmpeg, peaks, wav};
use crate::export::{ExportSpec, OutputFormat, StemsSpec, export_song, export_stems};
use crate::hls::audio_extract::{self, ExtractOptions};
use crate::hls::video_fetch::{self, VideoClip, VideoOptions};
use crate::ml::panns::{self, Probs, Tagger};
use crate::ml::runtime;
use crate::ml::separator::ModelSpec;
use crate::progress::{CancellationToken, Progress, ProgressFn};
use crate::project::report::{self, ReportFormat};
use crate::project::setlist::{self, SetlistMatch};
use crate::cookies::{CookieJar, CookieStatus};
use crate::project::{self, Alignment, Detection, JobError, naming, Mute, Origin, Project, Settings, Song, Store, plan};
use crate::render::MuteSpan;
use crate::source::{ResolveError, Source};

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

/// Re-resolve playback URLs older than this (tokens expire after a few hours).
const SOURCE_TTL: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub projects_root: PathBuf,
    pub models_dir: PathBuf,
    pub prefer_cpu: bool,
    /// keep the PC awake while jobs run; None = the saved preference (default on)
    pub keep_awake: Option<bool>,
}

/// Machine-readable error for clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ApiError {
    /// INVALID_PARAMS | NOT_FOUND | BUSY | CONFLICT | ADDON | UNRECOGNIZED_INPUT | AUTH_REQUIRED | VOD_NOT_FOUND |
    /// NETWORK | CDN_TRUNCATED | FORMAT | FFMPEG_MISSING | FFMPEG_FAILED | MODEL_MISSING |
    /// MODEL | CANCELLED | PRECONDITION | IO | INTERNAL
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: &str, message: impl std::fmt::Display) -> Self {
        Self { code: code.into(), message: message.to_string() }
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
    /// enabled songs whose clip is downloaded and separated
    pub clips_ready: usize,
    pub has_audio: bool,
    pub has_detection: bool,
    pub last_error: Option<JobError>,
}

pub struct Engine {
    pub cfg: EngineConfig,
    pub store: Store,
    events: UnboundedSender<Event>,
    jobs: Mutex<HashMap<String, JobEntry>>,
    sources: tokio::sync::Mutex<HashMap<String, (Source, Instant)>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    tagger: Arc<Mutex<Option<Tagger>>>,
    /// separation session, kept open between clips; holding its lock also
    /// serializes GPU separation across jobs
    separator: Mutex<Option<crate::ml::separator::Separator>>,
    cookies: Mutex<CookieJar>,
    keep_awake: AtomicBool,
    /// ADTS frame byte offsets of a project's analysis audio (audio.window)
    frame_index: Mutex<HashMap<String, FrameIndex>>,
    /// results of finished jobs, for `wait_job` (kept until read)
    finished: Mutex<FinishedJobs>,
    job_finished: tokio::sync::Notify,
    pub(crate) queue: Mutex<Vec<crate::queue::QueueEntry>>,
    pub(crate) queue_wake: tokio::sync::Notify,
    pub(crate) queue_worker: AtomicBool,
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
                if since.elapsed() < MODEL_IDLE {
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

struct AlignOutcome {
    method: String,
    offset: f64,
    corr: f32,
    predicted: f64,
    ok: bool,
}

/// Which clips a `clips.fetch` job works on (re-evaluated as the plan changes).
enum FetchTarget {
    /// every clip missing video or stems
    Missing,
    /// clips hosting these songs
    Songs(Vec<String>),
    /// these clips, or — once re-planned — unfinished clips hosting their songs
    Clips(Vec<String>, Vec<String>),
}

impl FetchTarget {
    fn wants(&self, c: &project::Clip) -> bool {
        let unfinished = c.video.is_none() || c.stems.is_none();
        match self {
            FetchTarget::Missing => unfinished,
            FetchTarget::Songs(s) => c.song_ids.iter().any(|x| s.contains(x)),
            FetchTarget::Clips(ids, songs) => ids.contains(&c.id) || (unfinished && c.song_ids.iter().any(|x| songs.contains(x))),
        }
    }
}

/// Engine preferences persisted in `<projects>/engine.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
struct EnginePrefs {
    keep_awake: bool,
}

impl Default for EnginePrefs {
    fn default() -> Self {
        Self { keep_awake: true }
    }
}

impl EnginePrefs {
    fn load(root: &Path) -> Self {
        std::fs::read(root.join("engine.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self, root: &Path) -> std::io::Result<()> {
        project::store::write_atomic(&root.join("engine.json"), &serde_json::to_vec_pretty(self).unwrap())
    }
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
    range(s.margin_before, 0.0, 600.0, "margin_before")?;
    range(s.margin_after, 0.0, 600.0, "margin_after")?;
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

/// Clip offset predicted from stored indexes: where the clip's first segment
/// starts on the analysis (decoded-sample) timeline vs. on the EXTINF timeline.
fn predicted_offset(p: &Project, v: &VideoClip) -> f64 {
    if is_local(p) {
        return 0.0;
    }
    let Some(a) = &p.audio else { return 0.0 };
    let seg = v.segments.first;
    if seg < a.index.first_segment || seg - a.index.first_segment > a.index.frames.len() || a.index.sample_rate == 0 {
        return 0.0;
    }
    a.index.sample_at_segment(seg - a.index.first_segment) as f64 / a.index.sample_rate as f64 - v.vod_start
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
    pair("audio.fetch", "detect.run") || pair("audio.fetch", "clips.fetch")
}

fn is_local(p: &Project) -> bool {
    p.source.as_ref().is_some_and(|s| s.platform == crate::source::Platform::Local)
}

fn invalid(msg: impl std::fmt::Display) -> ApiError {
    ApiError::new("INVALID_PARAMS", msg)
}

fn precondition(msg: impl std::fmt::Display) -> ApiError {
    ApiError::new("PRECONDITION", msg)
}

impl Engine {
    pub fn new(cfg: EngineConfig, events: UnboundedSender<Event>) -> Arc<Self> {
        let store = Store::new(&cfg.projects_root);
        let _ = std::fs::create_dir_all(&cfg.projects_root);
        let cookies = CookieJar::load(&cfg.projects_root);
        let prefs = EnginePrefs::load(&cfg.projects_root);
        let keep_awake = cfg.keep_awake.unwrap_or(prefs.keep_awake);
        let eng = Arc::new(Self {
            cfg,
            store,
            events,
            jobs: Mutex::new(HashMap::new()),
            sources: tokio::sync::Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            tagger: Arc::new(Mutex::new(None)),
            separator: Mutex::new(None),
            cookies: Mutex::new(cookies),
            keep_awake: AtomicBool::new(keep_awake),
            frame_index: Mutex::new(HashMap::new()),
            finished: Mutex::new((HashMap::new(), std::collections::VecDeque::new())),
            job_finished: tokio::sync::Notify::new(),
            queue: Mutex::new(Vec::new()),
            queue_wake: tokio::sync::Notify::new(),
            queue_worker: AtomicBool::new(false),
        });
        keep_awake_while_busy(Arc::downgrade(&eng));
        release_models_when_idle(Arc::downgrade(&eng));
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
        EnginePrefs { keep_awake: enabled }.save(&self.cfg.projects_root)?;
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
            }
            eng.job_finished.notify_waiters();
            eng.record_outcome(&pid, &kind_s, &res).await;
            match res {
                Ok(result) => eng.emit(Event::JobDone { job_id: jid, kind: kind_s, project_id: pid, result }),
                Err(error) => eng.emit(Event::JobFailed { job_id: jid, kind: kind_s, project_id: pid, error }),
            }
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
        Ok(tokio::task::spawn_blocking(move || summarize(store.list())).await?)
    }

    pub fn list(&self) -> Vec<ProjectSummary> {
        summarize(self.store.list())
    }

    /// Cancel every running job (shutdown).
    pub fn cancel_all(&self) {
        for j in self.jobs.lk().values() {
            j.cancel.cancel();
        }
    }
}

fn summarize(projects: Vec<Project>) -> Vec<ProjectSummary> {
    projects
        .into_iter()
            .map(|p| ProjectSummary {
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
                clips_ready: p
                    .songs
                    .iter()
                    .filter(|s| s.enabled)
                    .filter(|s| s.clip_id.as_deref().and_then(|c| p.clip(c)).is_some_and(|c| c.video.is_some() && c.stems.is_some()))
                    .count(),
                has_audio: p.audio.is_some(),
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
        let store = self.store.clone();
        let idc = id.to_string();
        tokio::task::spawn_blocking(move || store.delete(&idc)).await??;
        self.frame_index.lk().remove(id);
        self.sources.lock().await.remove(id);
        self.locks.lk().remove(id);
        Ok(())
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
        {
            let cache = self.sources.lock().await;
            if let Some((s, at)) = cache.get(&p.id)
                && at.elapsed() < SOURCE_TTL {
                    return Ok(s.clone());
                }
        }
        let src = self.resolve_fresh(&p.input).await?;
        if let Some(a) = &p.audio {
            let n = a.index.first_segment + a.index.frames.len();
            if src.playlist.segments.len() < n {
                return Err(ApiError::new("FORMAT", "the VOD's segment layout changed since the audio was fetched"));
            }
        }
        self.sources.lock().await.insert(p.id.clone(), (src.clone(), Instant::now()));
        Ok(src)
    }

    pub async fn update_settings(&self, id: &str, settings: Settings) -> ApiResult<Project> {
        validate_settings(&settings)?;
        self.mutate(id, |p| {
            p.settings = settings;
            plan::plan_clips(p);
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
            plan::plan_clips(p);
            Ok(())
        })
        .await
    }

    /// Change some fields of one song. `patch` is a partial Song (`id`,
    /// `clip_id`, `export`, `confidence` can't be changed; `mutes` replaces the
    /// song's mute list).
    pub async fn update_song(&self, id: &str, song_id: &str, patch: Value, base_revision: Option<u64>) -> ApiResult<Project> {
        let Value::Object(patch) = patch else { return Err(invalid("patch must be an object")) };
        for k in ["id", "clip_id", "export", "confidence"] {
            if patch.contains_key(k) {
                return Err(invalid(format!("{k} can't be changed")));
            }
        }
        self.mutate(id, |p| {
            check_revision(p, base_revision)?;
            let s = p.song_mut(song_id).ok_or_else(|| ApiError::new("NOT_FOUND", format!("song {song_id}")))?;
            let mut v = serde_json::to_value(&*s).map_err(|e| ApiError::new("INTERNAL", e))?;
            for (k, x) in patch {
                v[k] = x;
            }
            let mut ns: Song = serde_json::from_value(v).map_err(invalid)?;
            validate_song(&mut ns)?;
            *s = ns;
            sort_songs(p);
            plan::plan_clips(p);
            Ok(())
        })
        .await
    }

    /// Add one song (id assigned when empty). Returns the project and the song id.
    pub async fn add_song(&self, id: &str, mut song: Song, base_revision: Option<u64>) -> ApiResult<(Project, String)> {
        song.id = String::new();
        song.clip_id = None;
        song.export = None;
        validate_song(&mut song)?;
        let sid = song.id.clone();
        let p = self
            .mutate(id, |p| {
                check_revision(p, base_revision)?;
                p.songs.push(song);
                sort_songs(p);
                plan::plan_clips(p);
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
            plan::plan_clips(p);
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

    pub async fn plan(&self, id: &str) -> ApiResult<Project> {
        self.mutate(id, |p| {
            plan::plan_clips(p);
            Ok(())
        })
        .await
    }

    /// Change a clip's VOD range (e.g. a song was extended past its margins).
    /// Drops the clip's video/stems; the next `clips.fetch` re-downloads it.
    pub async fn extend_clip(&self, id: &str, clip_id: &str, start: f64, end: f64) -> ApiResult<Project> {
        if !(start.is_finite() && end.is_finite()) || end <= start || start < 0.0 {
            return Err(invalid("bad clip range"));
        }
        if self.jobs.lk().values().any(|j| j.info.project_id == id && (j.info.kind == "clips.fetch" || j.info.kind == "export.run")) {
            return Err(ApiError::new("BUSY", "clips are being fetched or exported"));
        }
        let dir = self.store.dir(id).join(format!("clips/{clip_id}"));
        let p = self
            .mutate(id, |p| {
                let c = p.clip_mut(clip_id).ok_or_else(|| ApiError::new("NOT_FOUND", format!("clip {clip_id}")))?;
                c.start = start;
                c.end = end;
                c.video = None;
                c.stems = None;
                c.talk.clear();
                c.offset = 0.0;
                c.alignment = None;
                // keep the requested range through re-planning
                c.pinned = true;
                plan::plan_clips(p);
                Ok(())
            })
            .await?;
        let _ = std::fs::remove_dir_all(dir);
        Ok(p)
    }

    /// Delete downloaded video + stems of one clip (or all clips) to free disk
    /// space. Songs, mutes and edits stay; `clips.fetch` downloads again.
    /// Returns the project and the bytes freed.
    pub async fn drop_media(&self, id: &str, clip_id: Option<&str>) -> ApiResult<(Project, u64)> {
        if self.jobs.lk().values().any(|j| j.info.project_id == id && (j.info.kind == "clips.fetch" || j.info.kind == "export.run")) {
            return Err(ApiError::new("BUSY", "clips are being fetched or exported"));
        }
        let mut dirs = Vec::new();
        let p = self
            .mutate(id, |p| {
                if let Some(c) = clip_id
                    && p.clip(c).is_none() {
                        return Err(ApiError::new("NOT_FOUND", format!("clip {c}")));
                    }
                for c in p.clips.iter_mut().filter(|c| clip_id.is_none_or(|x| x == c.id)) {
                    dirs.push(c.dir_name());
                    c.video = None;
                    c.stems = None;
                    c.talk.clear();
                    c.offset = 0.0;
                }
                plan::plan_clips(p);
                Ok(())
            })
            .await?;
        let root = self.store.dir(id);
        let freed = tokio::task::spawn_blocking(move || {
            let mut freed = 0u64;
            for d in dirs {
                let d = root.join(d);
                freed += dir_size(&d);
                let _ = std::fs::remove_dir_all(&d);
            }
            freed
        })
        .await?;
        Ok((p, freed))
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
            // the old probabilities / frame index describe the old audio
            let _ = std::fs::remove_file(eng.store.path(&p.id, "probs.bin"));
            eng.frame_index.lk().remove(&p.id);
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
                };
                let p = eng
                    .mutate(&p.id, |p| {
                        p.audio = Some(state);
                        p.detection = None;
                        Ok(())
                    })
                    .await?;
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
            let p = eng
                .mutate(&p.id, |p| {
                    p.audio = Some(project::AudioState::from_result(file, variant, &r));
                    p.detection = None;
                    Ok(())
                })
                .await?;
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
                    plan::plan_clips(p);
                    Ok(())
                })
                .await?;
            Ok(json!({ "segments": segs, "songs": p.songs.len(), "clips": p.clips.len(), "candidates": a.candidates.len() }))
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
            plan::plan_clips(p);
            Ok(())
        })
        .await
    }

    // ── clips: video → align → stems → talk ─────────────────────────────

    /// Fetch + process clips: video → alignment → stems → talk suggestions.
    /// Select by `clip_ids`, or by `song_ids` (their clips — "finish this song
    /// first", "fetch only the reviewed songs"); neither = every clip still
    /// missing video or stems.
    ///
    /// `exact` (with `song_ids`): plan clips covering just those songs (± margins)
    /// instead of the merged clips, so close neighbours aren't fetched along.
    pub async fn job_fetch_clips(self: &Arc<Self>, id: &str, clip_ids: Option<Vec<String>>, song_ids: Option<Vec<String>>, exact: bool) -> ApiResult<String> {
        let mut p = self.get(id)?;
        if p.audio.is_none() {
            return Err(precondition("fetch the audio first"));
        }
        if exact
            && let Some(ids) = &song_ids {
                if self.jobs.lk().values().any(|j| j.info.project_id == id && j.info.kind == "clips.fetch") {
                    return Err(ApiError::new("BUSY", format!("clips.fetch is already running for {id}")));
                }
                p = self
                    .mutate(id, |p| {
                        plan::pin_exact(p, ids);
                        Ok(())
                    })
                    .await?;
            }
        // What to work on, expressed so it survives re-planning while the job
        // runs (edits re-plan unfetched clips and may change their ids): the
        // target songs, plus explicitly named clips.
        let target = match (&clip_ids, &song_ids) {
            (Some(ids), _) => FetchTarget::Clips(
                ids.clone(),
                p.clips.iter().filter(|c| ids.contains(&c.id)).flat_map(|c| c.song_ids.clone()).collect(),
            ),
            (None, Some(ids)) => FetchTarget::Songs(ids.clone()),
            (None, None) => FetchTarget::Missing,
        };
        let todo: Vec<String> = p.clips.iter().filter(|c| target.wants(c)).map(|c| c.id.clone()).collect();
        if todo.is_empty() {
            return Err(precondition("no clips to fetch (plan clips / run detection first)"));
        }
        self.spawn_job("clips.fetch", id, todo.clone(), move |eng, ctx| async move {
            let mut done = Vec::new();
            let mut attempted: std::collections::HashSet<String> = std::collections::HashSet::new();
            loop {
                if ctx.cancel.is_cancelled() {
                    return Err(ApiError::new("CANCELLED", "cancelled"));
                }
                // re-read the plan every time: the user may have edited songs
                let cur = eng.get(&p.id)?;
                let Some(cid) = cur.clips.iter().filter(|c| !attempted.contains(&c.id)).find(|c| target.wants(c)).map(|c| c.id.clone()) else {
                    break;
                };
                attempted.insert(cid.clone());
                let (k, n) = (attempted.len() - 1, todo.len().max(attempted.len()));
                (ctx.progress)(Progress {
                    message: Some(format!("clip {}/{n}", k + 1)),
                    item: Some(cid.clone()),
                    ..Progress::new("clips.fetch", k as u64, n as u64)
                });
                // tag every progress event of this clip with its id
                let tagged: ProgressFn = {
                    let (inner, cid) = (ctx.progress.clone(), cid.clone());
                    Arc::new(move |mut pr: Progress| {
                        pr.item.get_or_insert_with(|| cid.clone());
                        inner(pr)
                    })
                };
                let cctx = JobCtx { progress: tagged, ..ctx.clone() };
                if eng.process_clip(&p.id, &cid, &cctx).await? {
                    done.push(cid.clone());
                }
            }
            eng.remove_orphan_clip_dirs(&p.id);
            Ok(json!({ "clips": done }))
        })
    }

    /// Delete `clips/<id>/` folders no clip refers to (left behind when a clip
    /// was re-planned away while it downloaded).
    fn remove_orphan_clip_dirs(&self, pid: &str) {
        let Ok(p) = self.get(pid) else { return };
        let Ok(rd) = std::fs::read_dir(self.store.dir(pid).join("clips")) else { return };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !p.clips.iter().any(|c| c.id == name) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    /// Returns false when the clip was re-planned away (the user edited its
    /// songs) — not an error; the job moves on.
    async fn process_clip(self: &Arc<Self>, pid: &str, cid: &str, ctx: &JobCtx) -> ApiResult<bool> {
        // pin it so edits during the download don't re-plan it away
        let p = match self
            .mutate(pid, |p| {
                let c = p.clip_mut(cid).ok_or_else(|| ApiError::new("NOT_FOUND", format!("clip {cid}")))?;
                c.pinned = true;
                Ok(())
            })
            .await
        {
            Ok(p) => p,
            Err(e) if e.code == "NOT_FOUND" && self.store.exists(pid) => return Ok(false),
            Err(e) => return Err(e),
        };
        let clip = p.clip(cid).cloned().unwrap();
        let dir = self.store.dir(pid).join(clip.dir_name());
        std::fs::create_dir_all(&dir)?;
        let mp4 = dir.join("source.mp4");

        // 1) video
        let clip = if clip.video.is_none() || !mp4.is_file() {
            let v = if is_local(&p) {
                (ctx.progress)(Progress::note("video.fetch", "cut_recording", "cutting the recording"));
                let (path, track, mp4c, (a, b), cancel) =
                    (PathBuf::from(&p.source.as_ref().unwrap().url), p.settings.audio_track, mp4.clone(), (clip.start, clip.end), ctx.cancel.clone());
                let r = tokio::task::spawn_blocking(move || crate::local::cut_clip(&path, a, b, track, &mp4c, &cancel)).await?;
                if ctx.cancel.is_cancelled() {
                    return Err(ApiError::new("CANCELLED", "cancelled"));
                }
                r?
            } else {
                let src = self.source(&p).await?;
                let opts = VideoOptions { height: p.settings.video_height, workers: None };
                video_fetch::fetch_video(&src, clip.start, clip.end, &mp4, &opts, ctx.progress.clone(), ctx.cancel.clone()).await?
            };
            // 2) align against the analysis audio
            let al = self.align_clip(&p, &v).await;
            if !al.ok {
                (ctx.progress)(
                    Progress::note("align", "alignment_weak", format!("clip audio didn't match the analysis audio (corr {:.2}); using the predicted offset", al.corr))
                        .arg("corr", (al.corr as f64 * 100.0).round() / 100.0),
                );
            }
            let offset = if al.method == "measured" { al.offset } else { al.predicted };
            let alignment = Alignment { method: al.method, corr: al.corr, predicted: al.predicted, ok: al.ok };
            let r = self
                .mutate(pid, |p| {
                    let c = p.clip_mut(cid).ok_or_else(|| ApiError::new("NOT_FOUND", cid))?;
                    c.video = Some(v);
                    c.offset = offset;
                    c.alignment = Some(alignment);
                    c.stems = None;
                    Ok(())
                })
                .await;
            match r {
                Ok(p2) => p2.clip(cid).cloned().unwrap(),
                Err(e) if e.code == "NOT_FOUND" => {
                    // re-planned away while downloading: nothing refers to it any more
                    let _ = std::fs::remove_dir_all(&dir);
                    return Ok(false);
                }
                Err(e) => return Err(e),
            }
        } else {
            clip
        };

        // 3) stems
        let clip = if clip.stems.as_ref().is_none_or(|s| !s.vocals.is_file()) {
            let (eng, progress, cancel, mp4c, dirc) = (self.clone(), ctx.progress.clone(), ctx.cancel.clone(), mp4.clone(), dir.clone());
            let stems = tokio::task::spawn_blocking(move || eng.separate_with_fallback(&mp4c, &dirc, &progress, &cancel)).await??;
            let r = self
                .mutate(pid, |p| {
                    let c = p.clip_mut(cid).ok_or_else(|| ApiError::new("NOT_FOUND", cid))?;
                    c.stems = Some(stems);
                    Ok(())
                })
                .await;
            match r {
                Ok(p2) => p2.clip(cid).cloned().unwrap(),
                Err(e) if e.code == "NOT_FOUND" => return Ok(false),
                Err(e) => return Err(e),
            }
        } else {
            clip
        };

        // 4) talk → mute suggestions
        if !p.settings.suggest_mutes {
            return Ok(true);
        }
        (ctx.progress)(Progress::note("talk", "find_talk", "finding talk in the vocal stem"));
        let stems = clip.stems.clone().unwrap();
        let songs_clip: Vec<(f64, f64)> = p
            .songs
            .iter()
            .filter(|s| s.clip_id.as_deref() == Some(cid))
            .filter_map(|s| Some((clip.to_clip_time(s.start)?, clip.to_clip_time(s.end)?)))
            .collect();
        let (eng, cancel) = (self.clone(), ctx.cancel.clone());
        let regions = tokio::task::spawn_blocking(move || -> ApiResult<Vec<(f64, f64)>> {
            let mut slot = eng.tagger.lk();
            let tagger = eng.tagger(&mut slot)?;
            let v = talk::probs_of_file(tagger, &stems.vocals, &cancel)?;
            let m = talk::probs_of_file(tagger, &stems.orig, &cancel)?;
            drop(slot);
            let tp = TalkParams::default();
            let regs = talk::talk_regions(&v, &m, &songs_clip, &tp);
            Ok(talk::refine_edges(&regs, &stems.vocals, &tp)?)
        })
        .await??;
        let (fade, gain_db) = (p.settings.mute_fade, p.settings.mute_gain_db);
        let r = self.mutate(pid, |p| {
            let c = p.clip_mut(cid).ok_or_else(|| ApiError::new("NOT_FOUND", cid))?;
            let talk_vod: Vec<(f64, f64)> = regions.iter().filter_map(|&(a, b)| Some((c.to_vod_time(a)?, c.to_vod_time(b)?))).collect();
            c.talk = talk_vod.clone();
            let (mb, ma) = (p.settings.margin_before, p.settings.margin_after);
            for s in p.songs.iter_mut().filter(|s| s.clip_id.as_deref() == Some(cid)) {
                // keep the user's own and confirmed mutes; refresh unconfirmed suggestions
                s.mutes.retain(|m| m.origin == Origin::Manual || m.confirmed);
                let kept: Vec<(f64, f64)> = s.mutes.iter().map(|m| (m.start, m.end)).collect();
                let fresh: Vec<Mute> = talk_vod
                    .iter()
                    .filter(|&&(a, b)| b > s.start - mb && a < s.end + ma)
                    .filter(|&&(a, b)| !kept.iter().any(|&(x, y)| a < y && x < b))
                    // fades lie inside a mute: put them outside the speech so the
                    // voice is fully removed from its first syllable on
                    .map(|&(a, b)| Mute {
                        id: project::new_id("m"),
                        start: (a - fade).max(0.0),
                        end: b + fade,
                        fade_in: fade.min(a),
                        fade_out: fade,
                        origin: Origin::Auto,
                        confirmed: false,
                        gain_db,
                    })
                    .collect();
                s.mutes.extend(fresh);
                s.mutes.sort_by(|a, b| a.start.total_cmp(&b.start));
            }
            Ok(())
        })
        .await;
        match r {
            Ok(_) => Ok(true),
            Err(e) if e.code == "NOT_FOUND" => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Offset of the clip's files against the analysis timeline.
    ///
    /// The offset is *predicted* exactly from the segment index (frame-count
    /// time of the clip's first segment − its EXTINF time) and *confirmed* by
    /// cross-correlating the clip audio with the analysis audio in a narrow
    /// window around the prediction, trying a few spots in the clip (a silent
    /// intro can't be matched). Without confirmation the prediction is used
    /// and the clip is flagged (`ok: false`) instead of silently assuming 0.
    async fn align_clip(self: &Arc<Self>, p: &Project, v: &VideoClip) -> AlignOutcome {
        let predicted = predicted_offset(p, v);
        let fallback = move |corr: f32| AlignOutcome { method: "predicted".into(), offset: predicted, corr, predicted, ok: false };
        let (eng, pc, vc) = (self.clone(), p.clone(), v.clone());
        let r = tokio::task::spawn_blocking(move || -> ApiResult<AlignOutcome> {
            const RATE: u32 = 8000;
            const W: f64 = 2.0; // search ± around the prediction
            let src = eng.analysis_source(&pc)?;
            let mut best = (0.0f32, None);
            for t0 in [5.0, 60.0, 120.0, 0.0] {
                let len = (vc.duration - t0).min(40.0);
                if len < 8.0 {
                    continue;
                }
                let clip = ffmpeg::decode(&vc.path, RATE, 1, Some(t0), Some(len))?.read_all()?;
                // analysis time of clip t0 is expected at vod_start + predicted + t0
                let expect = vc.vod_start + predicted + t0;
                let s0 = (expect - W).max(0.0);
                let ana = src.decode(s0, len + 2.0 * W, RATE, 1)?;
                if clip.is_empty() || ana.is_empty() {
                    continue;
                }
                // clip[t] ≈ ana[t + lag]; expected lag = expect − s0
                let (lag, corr) = align::find_lag(&ana, &clip, RATE, (expect - s0) + W);
                let offset = s0 + lag - vc.vod_start - t0;
                tracing::info!("clip alignment @{t0}s: offset {offset:.4}s (predicted {predicted:.4}) corr {corr:.3}");
                if corr >= 0.3 && (offset - predicted).abs() <= W {
                    return Ok(AlignOutcome { method: "measured".into(), offset, corr, predicted, ok: true });
                }
                if corr > best.0 {
                    best = (corr, Some(offset));
                }
            }
            Ok(fallback(best.0))
        })
        .await;
        match r {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                tracing::warn!("alignment failed ({e}); using the predicted offset");
                fallback(0.0)
            }
            Err(e) => {
                tracing::warn!("alignment task failed ({e}); using the predicted offset");
                fallback(0.0)
            }
        }
    }

    /// Separate a clip with the cached separator. On GPU out-of-memory at
    /// batch 1: free the detection model's GPU memory and retry once, then
    /// redo this clip on the CPU. Blocking.
    fn separate_with_fallback(&self, mp4: &Path, dir: &Path, progress: &ProgressFn, cancel: &CancellationToken) -> ApiResult<crate::stems::Stems> {
        use crate::ml::separator::{SeparateError, Separator};
        use crate::stems::StemError;
        let mut slot = self.separator.lk();
        let mut freed = false;
        loop {
            if slot.is_none() {
                *slot = Some(Separator::open(&self.cfg.models_dir, ModelSpec::voc_ft(), self.cfg.prefer_cpu)?);
            }
            match crate::stems::separate_clip(mp4, dir, slot.as_mut().unwrap(), progress, cancel) {
                Err(StemError::Separate(SeparateError::OutOfMemory)) if !freed => {
                    freed = true;
                    *slot = None;
                    *self.tagger.lk() = None;
                    tracing::warn!("separation out of GPU memory; freeing the detection model and retrying");
                }
                Err(StemError::Separate(SeparateError::OutOfMemory)) => {
                    *slot = None;
                    progress(Progress::note("separate", "gpu_out_of_memory", "GPU out of memory; separating this clip on the CPU"));
                    let mut cpu = Separator::open(&self.cfg.models_dir, ModelSpec::voc_ft(), true)?;
                    return Ok(crate::stems::separate_clip(mp4, dir, &mut cpu, progress, cancel)?);
                }
                r => return Ok(r?),
            }
        }
    }

    // ── export ──────────────────────────────────────────────────────────

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
        if let Some(c) = &opts.video_codec
            && !VIDEO_CODECS.contains(&c.as_str()) {
                return Err(invalid(format!("video_codec must be one of {VIDEO_CODECS:?}")));
            }
        validate_encode(opts.export_height, opts.video_bitrate_k, opts.audio_bitrate_k)?;
        let no_video = p.source.as_ref().is_some_and(|s| s.platform == crate::source::Platform::Local && s.variants.first().is_none_or(|v| v.height == 0));
        if no_video && opts.format.unwrap_or_default().has_video() && (!opts.stems || opts.also_render) {
            return Err(precondition("this recording has no video; export as wav / flac / mp3"));
        }
        for s in &songs {
            let ok = s.clip_id.as_ref().and_then(|c| p.clip(c)).is_some_and(|c| c.stems.is_some() && c.video.is_some());
            if !ok {
                return Err(precondition(format!("song {} has no processed clip yet (fetch clips first)", s.id)));
            }
            if let Some(path) = opts.audio_overrides.get(&s.id).filter(|p| !Path::new(p).is_file()) {
                return Err(invalid(format!("audio override for {} not found: {path}", s.id)));
            }
        }
        let items: Vec<String> = songs.iter().map(|s| s.id.clone()).collect();
        self.spawn_job("export.run", id, items, move |eng, ctx| async move {
            let out_dir = p.settings.output_dir.clone().map(PathBuf::from).unwrap_or_else(|| eng.store.dir(&p.id).join("exports"));
            std::fs::create_dir_all(&out_dir)?;
            let all: Vec<&Song> = p.songs.iter().filter(|s| s.enabled).collect();
            let format = opts.format.unwrap_or_default();
            let mut results = Vec::new();
            let mut stems_out = Vec::new();
            for (k, s) in songs.iter().enumerate() {
                if ctx.cancel.is_cancelled() {
                    return Err(ApiError::new("CANCELLED", "cancelled"));
                }
                (ctx.progress)(Progress {
                    message: Some(format!("song {}/{}", k + 1, songs.len())),
                    item: Some(s.id.clone()),
                    ..Progress::new("export.run", k as u64, songs.len() as u64)
                });
                let tagged: ProgressFn = {
                    let (inner, sid) = (ctx.progress.clone(), s.id.clone());
                    Arc::new(move |mut pr: Progress| {
                        pr.item.get_or_insert_with(|| sid.clone());
                        inner(pr)
                    })
                };
                // edits made while earlier songs exported apply to this one
                // (same clip only: a re-fetched clip has different stems)
                let latest = eng.get(&p.id).ok();
                let s = &latest
                    .as_ref()
                    .and_then(|q| q.song(&s.id).cloned())
                    .filter(|x| x.clip_id == s.clip_id && x.enabled)
                    .unwrap_or_else(|| s.clone());
                let clip = p.clip(s.clip_id.as_ref().unwrap()).unwrap();
                let (stems, video) = (clip.stems.clone().unwrap(), clip.video.clone().unwrap());
                let ct = |t: f64| clip.to_clip_time(t).unwrap();
                let number = all.iter().position(|x| x.id == s.id).map(|i| i + 1).unwrap_or(k + 1);
                let name = naming::limit_len(&naming::render_name(&p.settings.filename_template, &p, s, number));
                let mutes: Vec<MuteSpan> = s
                    .mutes
                    .iter()
                    .filter(|m| opts.apply_mutes.unwrap_or(true) && s.mute_enabled && (m.confirmed || opts.include_unconfirmed))
                    .map(|m| MuteSpan { start: ct(m.start), end: ct(m.end), fade_in: m.fade_in, fade_out: m.fade_out, gain_db: m.gain_db })
                    .collect();
                let (in_t, out_t) = (ct(s.start).max(0.0), ct(s.end).min(video.duration));

                if opts.stems {
                    let spec = StemsSpec {
                        source_mp4: video.path.clone(),
                        orig_wav: stems.orig.clone(),
                        vocals_wav: stems.vocals.clone(),
                        in_t,
                        out_t,
                        handles: opts.handles.unwrap_or(3.0).max(0.0),
                        mutes: mutes.clone(),
                        dir: naming::unique_dir(&out_dir, &format!("{name}_stems")),
                        name: name.clone(),
                        vod_offset: video.vod_start + clip.offset,
                        reference_video: opts.reference_video,
                    };
                    let progress = tagged.clone();
                    let r = tokio::task::spawn_blocking(move || export_stems(&spec, &progress)).await??;
                    stems_out.push(json!({ "song_id": s.id, "stems": r }));
                    if !opts.also_render {
                        continue;
                    }
                }

                let metadata = song_metadata(&p, s);
                let spec = ExportSpec {
                    source_mp4: video.path.clone(),
                    orig_wav: stems.orig.clone(),
                    vocals_wav: stems.vocals.clone(),
                    audio_override: opts.audio_overrides.get(&s.id).map(PathBuf::from),
                    in_t,
                    out_t,
                    mutes,
                    out: naming::unique_path(&out_dir, &name, format.extension()),
                    format,
                    video_codec: opts.video_codec.clone().unwrap_or_else(|| p.settings.video_codec.clone()),
                    video_quality: p.settings.video_quality,
                    scale_height: opts.export_height.or(p.settings.export_height),
                    video_bitrate_k: opts.video_bitrate_k.or(p.settings.video_bitrate_k),
                    audio_bitrate_k: opts.audio_bitrate_k.unwrap_or(p.settings.audio_bitrate_k),
                    fade_in: s.fade_in.unwrap_or(p.settings.song_fade_in),
                    fade_out: s.fade_out.unwrap_or(p.settings.song_fade_out),
                    loudness: p.settings.loudness,
                    metadata,
                };
                let cancel = ctx.cancel.clone();
                let r = tokio::task::spawn_blocking(move || export_song(&spec, &tagged, &cancel)).await??;
                let sid = s.id.clone();
                let info = project::ExportInfo { path: r.path.to_string_lossy().into_owned(), exported_at: project::now(), duration: r.duration };
                eng.mutate(&p.id, |p| {
                    if let Some(s) = p.song_mut(&sid) {
                        s.export = Some(info);
                    }
                    Ok(())
                })
                .await?;
                results.push(json!({ "song_id": sid, "result": r }));
            }
            Ok(json!({ "exports": results, "stems": stems_out }))
        })
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
    /// song id → audio file (aligned to the clip start) replacing the rendered audio
    pub audio_overrides: BTreeMap<String, String>,
}

fn dir_size(d: &Path) -> u64 {
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
