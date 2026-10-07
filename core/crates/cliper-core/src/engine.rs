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
use crate::hls::video_fetch::{self, VideoOptions};
use crate::ml::panns::{self, Probs, Tagger};
use crate::ml::runtime;
use crate::ml::separator::ModelSpec;
use crate::progress::{CancellationToken, Progress, ProgressFn};
use crate::project::report::{self, ReportFormat};
use crate::project::setlist::{self, SetlistMatch};
use crate::cookies::{CookieJar, CookieStatus};
use crate::project::{self, Detection, JobError, naming, Mute, Origin, Project, Settings, Song, Store, plan};
use crate::render::MuteSpan;
use crate::source::{ResolveError, Source};

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
    /// INVALID_PARAMS | NOT_FOUND | BUSY | CONFLICT | UNRECOGNIZED_INPUT | AUTH_REQUIRED | VOD_NOT_FOUND |
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
});
api_from!(crate::stems::StemError, |e: crate::stems::StemError| match e {
    crate::stems::StemError::Ffmpeg(f) => f.into(),
    crate::stems::StemError::Io(e) => e.into(),
    crate::stems::StemError::Separate(crate::ml::separator::SeparateError::Model(m)) => m.into(),
    crate::stems::StemError::Separate(crate::ml::separator::SeparateError::Cancelled(_)) => ApiError::new("CANCELLED", "cancelled"),
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
    cookies: Mutex<CookieJar>,
    keep_awake: AtomicBool,
    /// ADTS frame byte offsets of a project's analysis audio (audio.window)
    frame_index: Mutex<HashMap<String, FrameIndex>>,
    /// results of finished jobs, for `wait_job` (kept until read)
    finished: Mutex<HashMap<String, ApiResult<Value>>>,
    job_finished: tokio::sync::Notify,
    pub(crate) queue: Mutex<Vec<crate::queue::QueueEntry>>,
    pub(crate) queue_wake: tokio::sync::Notify,
    pub(crate) queue_worker: Mutex<bool>,
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
                let busy = !e.jobs.lock().unwrap().is_empty() && e.keep_awake.load(Ordering::Relaxed);
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
    s.mutes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
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
    p.songs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
}

/// Byte offset of every ADTS frame in `path`.
fn adts_index(path: &Path) -> std::io::Result<Vec<u64>> {
    let b = std::fs::read(path)?;
    let mut out = Vec::with_capacity(b.len() / 300);
    let mut i = 0usize;
    while i + 7 <= b.len() {
        if b[i] != 0xFF || b[i + 1] & 0xF0 != 0xF0 {
            // resync (shouldn't happen in files we wrote)
            i += 1;
            continue;
        }
        let len = (((b[i + 3] & 3) as usize) << 11) | ((b[i + 4] as usize) << 3) | ((b[i + 5] as usize) >> 5);
        if len < 7 {
            i += 1;
            continue;
        }
        out.push(i as u64);
        i += len;
    }
    Ok(out)
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
                b.lock().unwrap().push(&samples);
            }
        })
    };
    let snapshot = |b: &Mutex<peaks::PeakBuilder>| -> std::io::Result<()> {
        let pk = b.lock().unwrap().snapshot();
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

/// Current stage of a job for the ETA: (stage, item, started, `done` at start).
type StageClock = (String, Option<String>, Instant, u64);

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
            cookies: Mutex::new(cookies),
            keep_awake: AtomicBool::new(keep_awake),
            frame_index: Mutex::new(HashMap::new()),
            finished: Mutex::new(HashMap::new()),
            job_finished: tokio::sync::Notify::new(),
            queue: Mutex::new(Vec::new()),
            queue_wake: tokio::sync::Notify::new(),
            queue_worker: Mutex::new(false),
        });
        keep_awake_while_busy(Arc::downgrade(&eng));
        eng
    }

    pub(crate) fn emit_event(&self, e: Event) {
        self.emit(e);
    }

    /// Wait for a job started by this engine to finish; its result or error.
    pub async fn wait_job(&self, job_id: &str) -> ApiResult<Value> {
        loop {
            let notified = self.job_finished.notified();
            if let Some(r) = self.finished.lock().unwrap().remove(job_id) {
                return r;
            }
            if !self.jobs.lock().unwrap().contains_key(job_id) && !self.finished.lock().unwrap().contains_key(job_id) {
                return Err(ApiError::new("NOT_FOUND", format!("job {job_id}")));
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
        let ffmpeg = std::process::Command::new(ffmpeg::ffmpeg_path())
            .arg("-version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8_lossy(&o.stdout).lines().next().map(str::to_string));
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
            cookies: self.cookies.lock().unwrap().status(),
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
            let mut slot = eng.tagger.lock().unwrap();
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
        let mut jar = self.cookies.lock().unwrap();
        jar.set(cookies)?;
        Ok(jar.status())
    }

    pub fn cookies_status(&self) -> CookieStatus {
        self.cookies.lock().unwrap().status()
    }

    /// Ask the platform whether the stored cookies are still logged in.
    /// chzzk is checked; SOOP can't be checked without a VOD (valid stays null).
    pub async fn check_cookies(&self) -> ApiResult<CookieStatus> {
        let cookies = self.cookies.lock().unwrap().get();
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
        let mut jar = self.cookies.lock().unwrap();
        jar.record_check(valid, account);
        Ok(jar.status())
    }

    /// Keep the PC awake while jobs run (persisted).
    pub fn set_keep_awake(&self, enabled: bool) -> ApiResult<()> {
        self.keep_awake.store(enabled, Ordering::Relaxed);
        EnginePrefs { keep_awake: enabled }.save(&self.cfg.projects_root)?;
        Ok(())
    }

    // ── jobs ────────────────────────────────────────────────────────────

    pub fn jobs(&self) -> Vec<JobInfo> {
        self.jobs.lock().unwrap().values().map(|j| j.info.clone()).collect()
    }

    pub fn cancel(&self, job_id: &str) -> bool {
        match self.jobs.lock().unwrap().get(job_id) {
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
            let mut jobs = self.jobs.lock().unwrap();
            if jobs.values().any(|j| j.info.kind == kind && j.info.project_id == project_id) {
                return Err(ApiError::new("BUSY", format!("{kind} is already running for {project_id}")));
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
                    let mut st = stage.lock().unwrap();
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
                let mut l = last.lock().unwrap();
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
            let res = f(eng.clone(), ctx).await;
            eng.jobs.lock().unwrap().remove(&jid);
            {
                // remember the outcome for wait_job (bounded)
                let mut f = eng.finished.lock().unwrap();
                if f.len() > 256 {
                    f.clear();
                }
                f.insert(jid.clone(), res.clone());
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
        self.locks.lock().unwrap().entry(id.to_string()).or_default().clone()
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

    pub fn list(&self) -> Vec<ProjectSummary> {
        self.store
            .list()
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

    pub fn delete(&self, id: &str) -> ApiResult<()> {
        if self.jobs.lock().unwrap().values().any(|j| j.info.project_id == id) {
            return Err(ApiError::new("BUSY", "project has running jobs"));
        }
        Ok(self.store.delete(id)?)
    }

    /// Resolve `input` and create (or reopen) its project. Id = `<platform>-<video id>`.
    pub async fn create(&self, input: &str) -> ApiResult<Project> {
        if crate::local::is_local(input) {
            let path = crate::local::normalize(input);
            let id = crate::local::project_id(&path);
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
        let cookies = self.cookies.lock().unwrap().get();
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
        if settings.margin_before < 0.0 || settings.margin_after < 0.0 {
            return Err(invalid("margins must be ≥ 0"));
        }
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
        for s in songs.iter_mut() {
            validate_song(s)?;
        }
        songs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
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
        let p = self.get(id)?;
        let known = p.song(song_id).is_some_and(|s| s.mutes.iter().any(|m| m.id == mute.id));
        if !known {
            mute.id = String::new();
        }
        validate_mute(&mut mute)?;
        let mid = mute.id.clone();
        let p = self
            .mutate(id, |p| {
                check_revision(p, base_revision)?;
                let s = p.song_mut(song_id).ok_or_else(|| ApiError::new("NOT_FOUND", format!("song {song_id}")))?;
                match s.mutes.iter_mut().find(|m| m.id == mute.id) {
                    Some(m) => *m = mute,
                    None => s.mutes.push(mute),
                }
                s.mutes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
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
        if self.jobs.lock().unwrap().values().any(|j| j.info.project_id == id && (j.info.kind == "clips.fetch" || j.info.kind == "export.run")) {
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
        let mut freed = 0u64;
        for d in dirs {
            let d = root.join(d);
            freed += dir_size(&d);
            let _ = std::fs::remove_dir_all(&d);
        }
        Ok((p, freed))
    }

    /// Decode `[start, start+duration)` (VOD seconds) of the analysis audio to
    /// a 32-bit float WAV for previewing before any video exists. Sample-exact:
    /// VOD time t is decoded sample t·rate of the whole audio (the timeline the
    /// detector and clip alignment use).
    pub async fn audio_window(&self, id: &str, start: f64, duration: f64, sample_rate: Option<u32>, channels: Option<u32>) -> ApiResult<Value> {
        let p = self.get(id)?;
        let audio = p.audio.clone().ok_or_else(|| precondition("fetch the audio first"))?;
        if !(start.is_finite() && duration.is_finite()) || start < 0.0 || duration <= 0.0 || duration > 3600.0 {
            return Err(invalid("bad window"));
        }
        let (rate, ch) = (sample_rate.unwrap_or(48000).clamp(8000, 192000), channels.unwrap_or(2).clamp(1, 2));
        let src = self.store.path(id, &audio.file);
        let dir = self.store.dir(id).join("cache");
        std::fs::create_dir_all(&dir)?;
        let out = dir.join(format!("window_{}_{}_{rate}_{ch}.wav", (start * 1000.0).round() as u64, (duration * 1000.0).round() as u64));
        prune_cache(&dir, 24);
        if !out.is_file() {
            let index = if is_local(&p) { None } else { Some(self.frame_index(id, &src)?) };
            let (src, outc, spf, sr) = (src.clone(), out.clone(), audio.index.samples_per_frame as f64, audio.index.sample_rate as f64);
            tokio::task::spawn_blocking(move || -> ApiResult<()> {
                let tmp = outc.with_extension("part.wav");
                let mut c = ffmpeg::command();
                c.arg("-y");
                let mut _slice = None;
                match index {
                    // ADTS has no seek index: cut whole frames (2 frames of decoder
                    // pre-roll), decode, and trim the pre-roll exactly
                    Some(idx) => {
                        let k = ((start * sr / spf).floor() as usize).saturating_sub(2).min(idx.len().saturating_sub(1));
                        let k_end = (((start + duration) * sr / spf).ceil() as usize + 2).min(idx.len());
                        let bytes = std::fs::read(&src)?;
                        let (a, b) = (idx[k] as usize, idx.get(k_end).map(|&x| x as usize).unwrap_or(bytes.len()));
                        let slice = outc.with_extension("adts");
                        std::fs::write(&slice, &bytes[a..b])?;
                        let skip = start - k as f64 * spf / sr;
                        c.args(["-f", "aac", "-i"]).arg(&slice).args(["-ss", &format!("{skip:.6}")]);
                        _slice = Some(slice);
                    }
                    None => {
                        c.args(["-ss", &format!("{start:.6}"), "-i"]).arg(&src);
                    }
                }
                c.args(["-t", &format!("{duration:.6}"), "-vn", "-ac", &ch.to_string(), "-ar", &rate.to_string(), "-c:a", "pcm_f32le", "-f", "wav"]).arg(&tmp);
                let r = ffmpeg::run(c);
                if let Some(s) = _slice {
                    let _ = std::fs::remove_file(s);
                }
                r?;
                std::fs::rename(&tmp, &outc)?;
                Ok(())
            })
            .await??;
        }
        let rel = out.strip_prefix(self.store.dir(id)).unwrap_or(&out).to_string_lossy().replace('\\', "/");
        Ok(json!({ "path": out, "file": rel, "start": start, "duration": duration, "sample_rate": rate, "channels": ch }))
    }

    fn frame_index(&self, id: &str, path: &Path) -> ApiResult<Arc<Vec<u64>>> {
        let len = std::fs::metadata(path)?.len();
        if let Some((l, idx)) = self.frame_index.lock().unwrap().get(id)
            && *l == len {
                return Ok(idx.clone());
            }
        let idx = Arc::new(adts_index(path)?);
        self.frame_index.lock().unwrap().insert(id.to_string(), (len, idx.clone()));
        Ok(idx)
    }

    /// Engine preference + per-run override for keeping the PC awake.
    pub fn keep_awake(&self) -> bool {
        self.keep_awake.load(Ordering::Relaxed)
    }

    // ── audio ───────────────────────────────────────────────────────────

    pub fn job_fetch_audio(self: &Arc<Self>, id: &str) -> ApiResult<String> {
        let p = self.get(id)?;
        self.spawn_job("audio.fetch", id, Vec::new(), move |eng, ctx| async move {
            if is_local(&p) {
                let path = PathBuf::from(&p.source.as_ref().unwrap().url);
                let (file, track) = ("audio.mka", p.settings.audio_track);
                let out = eng.store.path(&p.id, file);
                (ctx.progress)(Progress::note("audio.fetch", "copy_audio_track", "copying the audio track"));
                let t0 = Instant::now();
                let duration = tokio::task::spawn_blocking(move || -> ApiResult<f64> {
                    crate::local::extract_audio(&path, track, &out)?;
                    Ok(crate::local::probe(&out)?.duration)
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
            let out = eng.store.path(&p.id, file);
            let _ = std::fs::remove_file(&out);
            // overview waveform fills in while the audio downloads
            let follower = PeaksFollower::start(out.clone(), eng.store.path(&p.id, "overview.peaks"));
            let progress: ProgressFn = {
                let (inner, until) = (ctx.progress.clone(), follower.until_ms.clone());
                Arc::new(move |mut pr: Progress| {
                    let u = until.load(Ordering::Relaxed);
                    if u > 0 {
                        pr.peaks_until = Some(u as f64 / 1000.0);
                    }
                    inner(pr)
                })
            };
            let r = audio_extract::extract_audio(&src, &out, &ExtractOptions::default(), progress, ctx.cancel.clone()).await;
            tokio::task::spawn_blocking(move || follower.finish()).await?;
            let r = r?;
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
            let cached = if retag { None } else { std::fs::read(&probs_path).ok().and_then(|b| Probs::from_bytes(&b)) };
            let (probs, device) = match cached {
                Some(pr) => (pr, p.detection.as_ref().map(|d| d.device).unwrap_or(runtime::Device::Cpu)),
                None => {
                    let (eng2, ctx2) = (eng.clone(), ctx.clone());
                    let input = eng.store.path(&p.id, &audio.file);
                    let peaks_path = eng.store.path(&p.id, "overview.peaks");
                    let total = (audio.duration * panns::SAMPLE_RATE as f64) as u64;
                    tokio::task::spawn_blocking(move || -> ApiResult<(Probs, runtime::Device)> {
                        let mut slot = eng2.tagger.lock().unwrap();
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
            let params = params.unwrap_or_else(|| p.detection.as_ref().map(|d| d.params.clone()).unwrap_or_default());
            let a = detect::analyze(&probs, &params);
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
        let probs = std::fs::read(self.store.path(id, "probs.bin"))
            .ok()
            .and_then(|b| Probs::from_bytes(&b))
            .ok_or_else(|| precondition("run detection first"))?;
        let a = detect::analyze(&probs, &params);
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
                if self.jobs.lock().unwrap().values().any(|j| j.info.project_id == id && j.info.kind == "clips.fetch") {
                    return Err(ApiError::new("BUSY", format!("clips.fetch is already running for {id}")));
                }
                p = self
                    .mutate(id, |p| {
                        plan::pin_exact(p, ids);
                        Ok(())
                    })
                    .await?;
            }
        let from_songs: Option<Vec<String>> = song_ids.map(|ids| {
            p.songs.iter().filter(|s| ids.contains(&s.id)).filter_map(|s| s.clip_id.clone()).collect()
        });
        let todo: Vec<String> = p
            .clips
            .iter()
            .filter(|c| match (&clip_ids, &from_songs) {
                (Some(ids), _) | (None, Some(ids)) => ids.contains(&c.id),
                (None, None) => c.video.is_none() || c.stems.is_none(),
            })
            .map(|c| c.id.clone())
            .collect();
        if todo.is_empty() {
            return Err(precondition("no clips to fetch (plan clips / run detection first)"));
        }
        self.spawn_job("clips.fetch", id, todo.clone(), move |eng, ctx| async move {
            let n = todo.len();
            let mut done = Vec::new();
            for (k, cid) in todo.iter().enumerate() {
                if ctx.cancel.is_cancelled() {
                    return Err(ApiError::new("CANCELLED", "cancelled"));
                }
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
                eng.process_clip(&p.id, cid, &cctx).await?;
                done.push(cid.clone());
            }
            Ok(json!({ "clips": done }))
        })
    }

    async fn process_clip(self: &Arc<Self>, pid: &str, cid: &str, ctx: &JobCtx) -> ApiResult<()> {
        let p = self.get(pid)?;
        let clip = p.clip(cid).cloned().ok_or_else(|| ApiError::new("NOT_FOUND", format!("clip {cid}")))?;
        let dir = self.store.dir(pid).join(clip.dir_name());
        std::fs::create_dir_all(&dir)?;
        let mp4 = dir.join("source.mp4");

        // 1) video
        let clip = if clip.video.is_none() || !mp4.is_file() {
            let v = if is_local(&p) {
                (ctx.progress)(Progress::note("video.fetch", "cut_recording", "cutting the recording"));
                let (path, track, mp4c, (a, b)) = (PathBuf::from(&p.source.as_ref().unwrap().url), p.settings.audio_track, mp4.clone(), (clip.start, clip.end));
                tokio::task::spawn_blocking(move || crate::local::cut_clip(&path, a, b, track, &mp4c)).await??
            } else {
                let src = self.source(&p).await?;
                let opts = VideoOptions { height: p.settings.video_height, workers: None };
                video_fetch::fetch_video(&src, clip.start, clip.end, &mp4, &opts, ctx.progress.clone(), ctx.cancel.clone()).await?
            };
            // 2) align against the analysis audio
            let offset = self.measure_offset(&p, &v.path, v.vod_start).await.unwrap_or_else(|e| {
                tracing::warn!("alignment failed ({e}); assuming 0");
                0.0
            });
            let p2 = self
                .mutate(pid, |p| {
                    let c = p.clip_mut(cid).ok_or_else(|| ApiError::new("NOT_FOUND", cid))?;
                    c.video = Some(v);
                    c.offset = offset;
                    c.stems = None;
                    Ok(())
                })
                .await?;
            p2.clip(cid).cloned().unwrap()
        } else {
            clip
        };

        // 3) stems
        let clip = if clip.stems.as_ref().is_none_or(|s| !s.vocals.is_file()) {
            let (models, cpu, progress, cancel, mp4c, dirc) =
                (self.cfg.models_dir.clone(), self.cfg.prefer_cpu, ctx.progress.clone(), ctx.cancel.clone(), mp4.clone(), dir.clone());
            let stems = tokio::task::spawn_blocking(move || -> ApiResult<crate::stems::Stems> {
                let st = crate::stems::separate_clip(&mp4c, &dirc, &models, ModelSpec::voc_ft(), cpu, &progress, &cancel)?;
                for (wavp, name) in [(&st.orig, "orig.peaks"), (&st.vocals, "vocals.peaks")] {
                    let w = wav::read_f32(wavp)?;
                    peaks::of_stereo(&w.samples, w.sample_rate, 100).write(&dirc.join(name))?;
                }
                Ok(st)
            })
            .await??;
            let p2 = self
                .mutate(pid, |p| {
                    let c = p.clip_mut(cid).ok_or_else(|| ApiError::new("NOT_FOUND", cid))?;
                    c.stems = Some(stems);
                    Ok(())
                })
                .await?;
            p2.clip(cid).cloned().unwrap()
        } else {
            clip
        };

        // 4) talk → mute suggestions
        if !p.settings.suggest_mutes {
            return Ok(());
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
            let mut slot = eng.tagger.lock().unwrap();
            let tagger = eng.tagger(&mut slot)?;
            let v = talk::probs_of_file(tagger, &stems.vocals, &cancel)?;
            let m = talk::probs_of_file(tagger, &stems.orig, &cancel)?;
            Ok(talk::talk_regions(&v, &m, &songs_clip, &TalkParams::default()))
        })
        .await??;
        let (fade, gain_db) = (p.settings.mute_fade, p.settings.mute_gain_db);
        self.mutate(pid, |p| {
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
                    .map(|&(a, b)| Mute {
                        id: project::new_id("m"),
                        start: a,
                        end: b,
                        fade_in: fade,
                        fade_out: fade,
                        origin: Origin::Auto,
                        confirmed: false,
                        gain_db,
                    })
                    .collect();
                s.mutes.extend(fresh);
                s.mutes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
            }
            Ok(())
        })
        .await?;
        Ok(())
    }

    /// Lag of the clip's audio against the analysis audio around the clip start.
    async fn measure_offset(&self, p: &Project, mp4: &Path, vod_start: f64) -> ApiResult<f64> {
        let audio = p.audio.as_ref().ok_or_else(|| precondition("no audio"))?;
        let analysis = self.store.path(&p.id, &audio.file);
        // The engine always extracts the whole VOD, so analysis t=0 is VOD t=0
        // (its frame-count timeline may drift slightly from EXTINF; the measured
        // offset absorbs that locally).
        if audio.index.first_segment != 0 {
            return Err(precondition("analysis audio does not start at the VOD start"));
        }
        let (mp4, rate) = (mp4.to_path_buf(), 8000u32);
        // clip window [5 s, 45 s); analysis window starts 3 s earlier in VOD time,
        // so the expected lag is ~3 s and ±6 s covers any plausible drift
        let a_start = (vod_start + 2.0).max(0.0);
        tokio::task::spawn_blocking(move || -> ApiResult<f64> {
            let clip = ffmpeg::decode(&mp4, rate, 1, Some(5.0), Some(40.0))?.read_all()?;
            let ana = ffmpeg::decode(&analysis, rate, 1, Some(a_start), Some(50.0))?.read_all()?;
            // clip[t] ≈ ana[t + lag]  →  clip t=5 ↔ analysis a_start + lag
            let (lag, corr) = align::find_lag(&ana, &clip, rate, 6.0);
            tracing::info!("clip alignment: lag {lag:.4}s corr {corr:.3}");
            if corr < 0.3 {
                return Err(ApiError::new("FORMAT", format!("weak alignment correlation {corr:.2}")));
            }
            // VOD time of clip t=5 = a_start + lag; offset = that − vod_start − 5
            Ok(a_start + lag - vod_start - 5.0)
        })
        .await?
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
        let no_video = p.source.as_ref().is_some_and(|s| s.platform == crate::source::Platform::Local && s.variants.first().is_none_or(|v| v.height == 0));
        if no_video && opts.format.unwrap_or_default().has_video() && !(opts.stems && !opts.also_render) {
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
                let clip = p.clip(s.clip_id.as_ref().unwrap()).unwrap();
                let (stems, video) = (clip.stems.clone().unwrap(), clip.video.clone().unwrap());
                let ct = |t: f64| clip.to_clip_time(t).unwrap();
                let number = all.iter().position(|x| x.id == s.id).map(|i| i + 1).unwrap_or(k + 1);
                let name = naming::render_name(&p.settings.filename_template, &p, s, number);
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
                        dir: out_dir.join(format!("{name}_stems")),
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
                    audio_bitrate_k: p.settings.audio_bitrate_k,
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

/// Keep at most `keep` newest files in a cache dir.
fn prune_cache(dir: &Path, keep: usize) {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
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
