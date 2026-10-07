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
use std::sync::{Arc, Mutex};
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
use crate::project::{self, Detection, naming, Mute, Origin, Project, Settings, Song, Store, plan};
use crate::render::MuteSpan;
use crate::source::{ResolveError, Source};

/// Re-resolve playback URLs older than this (tokens expire after a few hours).
const SOURCE_TTL: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub projects_root: PathBuf,
    pub models_dir: PathBuf,
    pub prefer_cpu: bool,
}

/// Machine-readable error for clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ApiError {
    /// INVALID_PARAMS | NOT_FOUND | BUSY | UNRECOGNIZED_INPUT | AUTH_REQUIRED | VOD_NOT_FOUND |
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

/// Notifications pushed to the client.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    JobStarted { job_id: String, kind: String, project_id: String },
    JobProgress { job_id: String, kind: String, project_id: String, progress: Progress },
    JobDone { job_id: String, kind: String, project_id: String, result: Value },
    JobFailed { job_id: String, kind: String, project_id: String, error: ApiError },
    ProjectChanged { project: Box<Project> },
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
}

pub struct Engine {
    pub cfg: EngineConfig,
    pub store: Store,
    events: UnboundedSender<Event>,
    jobs: Mutex<HashMap<String, JobEntry>>,
    sources: tokio::sync::Mutex<HashMap<String, (Source, Instant)>>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    tagger: Arc<Mutex<Option<Tagger>>>,
    cookies: Mutex<Option<BTreeMap<String, String>>>,
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
        let cookies = std::fs::read(cfg.projects_root.join("cookies.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
        Arc::new(Self {
            cfg,
            store,
            events,
            jobs: Mutex::new(HashMap::new()),
            sources: tokio::sync::Mutex::new(HashMap::new()),
            locks: Mutex::new(HashMap::new()),
            tagger: Arc::new(Mutex::new(None)),
            cookies: Mutex::new(cookies),
        })
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
        }
    }

    /// Login cookies used for resolving (e.g. chzzk `NID_AUT`/`NID_SES`); persisted.
    pub fn set_cookies(&self, cookies: Option<BTreeMap<String, String>>) -> ApiResult<()> {
        let path = self.cfg.projects_root.join("cookies.json");
        match &cookies {
            Some(c) => project::store::write_atomic(&path, &serde_json::to_vec_pretty(c).unwrap())?,
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
        *self.cookies.lock().unwrap() = cookies;
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
    fn spawn_job<F, Fut>(self: &Arc<Self>, kind: &str, project_id: &str, f: F) -> ApiResult<String>
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
            Arc::new(move |p: Progress| {
                let mut l = last.lock().unwrap();
                if p.message.is_none() && p.done != p.total && l.elapsed() < Duration::from_millis(100) {
                    return;
                }
                *l = Instant::now();
                if let Some(e) = eng.upgrade() {
                    e.emit(Event::JobProgress { job_id: job_id.clone(), kind: kind.clone(), project_id: pid.clone(), progress: p });
                }
            })
        };
        self.emit(Event::JobStarted { job_id: job_id.clone(), kind: kind_s.clone(), project_id: pid.clone() });
        let ctx = JobCtx { job_id: job_id.clone(), cancel, progress };
        let eng = self.clone();
        let jid = job_id.clone();
        tokio::spawn(async move {
            let res = f(eng.clone(), ctx).await;
            eng.jobs.lock().unwrap().remove(&jid);
            match res {
                Ok(result) => eng.emit(Event::JobDone { job_id: jid, kind: kind_s, project_id: pid, result }),
                Err(error) => eng.emit(Event::JobFailed { job_id: jid, kind: kind_s, project_id: pid, error }),
            }
        });
        Ok(job_id)
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
        let cookies = self.cookies.lock().unwrap().clone();
        Ok(crate::platforms::resolve(input, cookies.as_ref()).await?)
    }

    /// Resolved source for a project, re-resolving when the cached playback
    /// URLs are getting old. Verifies the segmentation hasn't changed.
    async fn source(&self, p: &Project) -> ApiResult<Source> {
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
    pub async fn set_songs(&self, id: &str, mut songs: Vec<Song>) -> ApiResult<Project> {
        for s in songs.iter_mut() {
            if !(s.start.is_finite() && s.end.is_finite()) || s.end <= s.start || s.start < 0.0 {
                return Err(invalid(format!("song {}: bad range {}–{}", s.id, s.start, s.end)));
            }
            if s.id.is_empty() {
                s.id = project::new_id("s");
            }
            for m in s.mutes.iter_mut() {
                if m.end <= m.start || m.fade_in < 0.0 || m.fade_out < 0.0 || m.gain_db.is_some_and(|g| !g.is_finite() || g > 0.0) {
                    return Err(invalid(format!("song {}: bad mute {}–{}", s.id, m.start, m.end)));
                }
                if m.id.is_empty() {
                    m.id = project::new_id("m");
                }
                if m.origin == Origin::Manual {
                    m.confirmed = true;
                }
            }
            s.mutes.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
        }
        songs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
        self.mutate(id, |p| {
            p.songs = songs;
            plan::plan_clips(p);
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

    // ── audio ───────────────────────────────────────────────────────────

    pub fn job_fetch_audio(self: &Arc<Self>, id: &str) -> ApiResult<String> {
        let p = self.get(id)?;
        self.spawn_job("audio.fetch", id, move |eng, ctx| async move {
            let src = eng.source(&p).await?;
            let file = "audio.aac";
            let out = eng.store.path(&p.id, file);
            let r = audio_extract::extract_audio(&src, &out, &ExtractOptions::default(), ctx.progress.clone(), ctx.cancel.clone()).await?;
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
        self.spawn_job("detect.run", id, move |eng, ctx| async move {
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
    pub fn job_fetch_clips(self: &Arc<Self>, id: &str, clip_ids: Option<Vec<String>>, song_ids: Option<Vec<String>>) -> ApiResult<String> {
        let p = self.get(id)?;
        if p.audio.is_none() {
            return Err(precondition("fetch the audio first"));
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
        self.spawn_job("clips.fetch", id, move |eng, ctx| async move {
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
            let src = self.source(&p).await?;
            let opts = VideoOptions { height: p.settings.video_height, workers: None };
            let v = video_fetch::fetch_video(&src, clip.start, clip.end, &mp4, &opts, ctx.progress.clone(), ctx.cancel.clone()).await?;
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
        (ctx.progress)(Progress::note("talk", "finding talk in the vocal stem"));
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
        for s in &songs {
            let ok = s.clip_id.as_ref().and_then(|c| p.clip(c)).is_some_and(|c| c.stems.is_some() && c.video.is_some());
            if !ok {
                return Err(precondition(format!("song {} has no processed clip yet (fetch clips first)", s.id)));
            }
            if let Some(path) = opts.audio_overrides.get(&s.id).filter(|p| !Path::new(p).is_file()) {
                return Err(invalid(format!("audio override for {} not found: {path}", s.id)));
            }
        }
        self.spawn_job("export.run", id, move |eng, ctx| async move {
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
                    .filter(|m| m.confirmed || opts.include_unconfirmed)
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
                    fade_in: p.settings.song_fade_in,
                    fade_out: p.settings.song_fade_out,
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
