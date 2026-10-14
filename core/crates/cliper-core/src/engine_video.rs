//! Engine side of the segment store: the app-wide fetch scheduler (S-E2,
//! S-E3), coverage notifications (S-E1), playable views (S-E4), video ↔
//! analysis alignment, and the replay check (S-E9).
//!
//! One worker loop serves every project: it starts up to [`WORKERS`] segment
//! downloads, best `(priority, request order)` first, across all projects;
//! every HTTP request also passes the app-wide cap of 4 in `http`, which
//! audio extraction shares. State changes are pushed as `video.changed`
//! (throttled) and `video.request_done`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{ApiError, ApiResult, Engine, Event, LockExt, invalid, is_local, precondition};
use crate::hls::m3u8::MediaPlaylist;
use crate::project::{self, Project, VodState, VodStatus};
use crate::ranges::{self, Range};
use crate::source::Container;
use crate::video::media::{self, Piece};
use crate::video::store::{self, Align, FetchRequest, Have, HaveKind, Priority, Probe, SegFailure, VideoIndex, ViewEntry};
use crate::video::timeline::{EDGE_EPS, Timeline};
use crate::video::{PState, RateMeter, SEGMENT_RETRIES, VideoState, WORKERS};

/// Longest view built in one file; longer stretches are viewed in windows.
const VIEW_MAX_SEC: f64 = 30.0 * 60.0;
/// Views kept per project (LRU); older ones are deleted.
const VIEWS_KEPT: usize = 3;
/// Free space kept on the drive when estimating whether a fetch fits.
const SPACE_RESERVE: u64 = 512 << 20;

#[derive(Default)]
pub(crate) struct MgrState {
    pub projects: HashMap<String, PState>,
    pub inflight: usize,
    pub rate: RateMeter,
    /// (project, rendition) → media playlist and when it was loaded
    pub playlists: HashMap<(String, usize), (Arc<MediaPlaylist>, Instant)>,
}

pub(crate) struct VideoMgr {
    pub st: Mutex<MgrState>,
    pub wake: tokio::sync::Notify,
    /// coverage changed somewhere (export waits on it)
    pub changed: tokio::sync::Notify,
    pub paused: std::sync::atomic::AtomicBool,
    pub started: std::sync::atomic::AtomicBool,
    /// project → lock serialising view builds
    pub view_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl VideoMgr {
    pub fn new(paused: bool) -> Self {
        Self {
            st: Mutex::new(MgrState::default()),
            wake: tokio::sync::Notify::new(),
            changed: tokio::sync::Notify::new(),
            paused: std::sync::atomic::AtomicBool::new(paused),
            started: std::sync::atomic::AtomicBool::new(false),
            view_locks: Mutex::new(HashMap::new()),
        }
    }
}

/// `video.fetch` arguments.
#[derive(Debug, Clone, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct FetchArgs {
    /// VOD ranges
    pub ranges: Option<Vec<Range>>,
    /// songs (their range ± the project's margins); combined with `ranges`
    pub song_ids: Option<Vec<String>>,
    /// default "user"
    pub priority: Option<Priority>,
    /// free-form tag shown back in `requests` (default "user")
    pub reason: Option<String>,
    /// separate + suggest mutes for these songs / ranges once fetched
    pub process: bool,
    /// rendition height for new ranges (default settings.video_height)
    pub height: Option<u32>,
}

/// `video.fetch` / `video.estimate` result.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct FetchPlan {
    /// None when nothing was missing (nothing queued)
    pub request_id: Option<String>,
    /// seconds of video the ranges span (whole segments)
    pub total_sec: f64,
    /// seconds not yet on disk (to download)
    pub missing_sec: f64,
    /// rough download size of `missing_sec`
    pub estimated_bytes: u64,
    /// the ranges, rounded out to whole segments
    pub ranges: Vec<Range>,
}

/// A playable file for some stretch of the VOD (S-E4).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VideoView {
    /// project-relative path (serve it like `clips/<id>/source.mp4` before)
    pub file: String,
    pub path: PathBuf,
    /// VOD time at file t = 0: `video.currentTime = vod − origin`
    pub origin: f64,
    /// VOD range this file has pictures for
    pub start: f64,
    pub end: f64,
    pub height: Option<u32>,
    /// "segments" | "legacy" | "local"
    pub kind: String,
}

/// Why a segment download ended without the segment.
enum SegErr {
    Cancelled,
    VodGone(String),
    Failed { code: String, message: String },
}

impl Engine {
    // ── state ───────────────────────────────────────────────────────────

    fn new_pstate(&self, p: &Project) -> ApiResult<PState> {
        let dir = self.store.dir(&p.id);
        let idx = VideoIndex::load(&dir);
        let have = store::scan(&dir, &idx);
        let local = is_local(p);
        let tl = match (&p.audio, local) {
            (Some(_), true) => Some(Timeline::from_durations(0, &[p.audio.as_ref().unwrap().duration])),
            (Some(a), false) => Some(Timeline::from_audio(&a.index)),
            (None, _) => None,
        };
        let src = p.source.as_ref();
        Ok(PState {
            dir,
            idx,
            have,
            fetching: HashMap::new(),
            tl,
            container: src.map(|s| s.container).unwrap_or(Container::Fmp4),
            heights: src.map(|s| s.variants.iter().map(|v| v.height).collect()).unwrap_or_default(),
            local,
            vod_gone: p.vod.as_ref().is_some_and(|v| v.status == VodState::Gone),
            dirty: true,
            idx_dirty: false,
        })
    }

    /// Run `f` on a project's video state (loaded on first use).
    pub(crate) fn with_ps<R>(&self, pid: &str, f: impl FnOnce(&mut PState) -> R) -> ApiResult<R> {
        {
            let mut st = self.video.st.lk();
            if let Some(ps) = st.projects.get_mut(pid) {
                return Ok(f(ps));
            }
        }
        let p = self.store.load(pid)?;
        let ps = self.new_pstate(&p)?;
        let mut st = self.video.st.lk();
        let ps = st.projects.entry(pid.to_string()).or_insert(ps);
        Ok(f(ps))
    }

    /// Re-read a project's store from disk (after cleanup, audio re-fetch, …),
    /// keeping in-flight downloads.
    pub(crate) fn reload_ps(&self, pid: &str) -> ApiResult<()> {
        let p = self.store.load(pid)?;
        let mut fresh = self.new_pstate(&p)?;
        let mut st = self.video.st.lk();
        if let Some(old) = st.projects.remove(pid) {
            fresh.fetching = old.fetching;
        }
        st.projects.insert(pid.to_string(), fresh);
        drop(st);
        self.video.wake.notify_one();
        Ok(())
    }

    pub(crate) fn forget_ps(&self, pid: &str) {
        if let Some(ps) = self.video.st.lk().projects.remove(pid) {
            for (_, (_, t)) in ps.fetching {
                t.cancel();
            }
        }
    }

    /// Every loaded project's video state (`video.status` without id): the
    /// jobs panel's "video" lines, plus the app-wide pause.
    pub fn video_states(&self) -> serde_json::Value {
        let paused = self.video.paused.load(Ordering::Relaxed);
        let st = self.video.st.lk();
        let rate = st.rate.rate();
        let projects: BTreeMap<String, VideoState> = st.projects.iter().map(|(pid, ps)| (pid.clone(), ps.state(pid, paused, rate))).collect();
        json!({ "paused": paused, "projects": projects })
    }

    pub fn video_state(&self, pid: &str) -> ApiResult<VideoState> {
        let paused = self.video.paused.load(Ordering::Relaxed);
        let rate = self.video.st.lk().rate.rate();
        self.with_ps(pid, |ps| ps.state(pid, paused, rate))
    }

    pub(crate) fn save_index(&self, pid: &str) {
        let snap = self.video.st.lk().projects.get_mut(pid).map(|ps| {
            ps.idx_dirty = false;
            (ps.dir.clone(), ps.idx.clone())
        });
        if let Some((dir, idx)) = snap
            && let Err(e) = idx.save(&dir)
        {
            tracing::warn!("saving video index of {pid}: {e}");
        }
    }

    // ── requests ────────────────────────────────────────────────────────

    /// VOD ranges of `args` (ranges + songs ± margins), validated.
    fn fetch_ranges(&self, p: &Project, args: &FetchArgs) -> ApiResult<Vec<Range>> {
        let mut rs: Vec<Range> = Vec::new();
        for &(a, b) in args.ranges.iter().flatten() {
            if !(a.is_finite() && b.is_finite()) || b <= a || a < 0.0 {
                return Err(invalid(format!("bad range {a}–{b}")));
            }
            rs.push((a, b));
        }
        for id in args.song_ids.iter().flatten() {
            let s = p.song(id).ok_or_else(|| ApiError::new("NOT_FOUND", format!("song {id}")))?;
            rs.push(s.with_margins(&p.settings));
        }
        if rs.is_empty() {
            return Err(invalid("give ranges or song_ids"));
        }
        Ok(ranges::normalize(&rs))
    }

    /// Plan without queuing: how much is missing and roughly how big.
    pub fn video_estimate(&self, pid: &str, args: &FetchArgs) -> ApiResult<FetchPlan> {
        let p = self.get(pid)?;
        let rs = self.fetch_ranges(&p, args)?;
        let height = args.height.or(p.settings.video_height);
        self.with_ps(pid, |ps| plan_of(ps, &p, &rs, height, None))?
    }

    /// Queue the video of some VOD ranges (S-E2). Only missing segments are
    /// fetched; ones already queued for another request are shared.
    pub fn video_fetch(self: &Arc<Self>, pid: &str, args: FetchArgs) -> ApiResult<FetchPlan> {
        let p = self.get(pid)?;
        if p.audio.is_none() {
            return Err(precondition("fetch the audio first (it places the video segments)"));
        }
        let rs = self.fetch_ranges(&p, &args)?;
        let height = args.height.or(p.settings.video_height);
        let request_id = project::new_id("r");
        let plan = self.with_ps(pid, |ps| -> ApiResult<FetchPlan> {
            let plan = plan_of(ps, &p, &rs, height, Some(request_id.clone()))?;
            if ps.local || plan.missing_sec <= 0.0 {
                return Ok(FetchPlan { request_id: None, ..plan });
            }
            if ps.vod_gone {
                return Err(ApiError::new("VOD_GONE", "the replay is no longer available; only video already downloaded can be used"));
            }
            if let Some(free) = crate::storage::free_space(&ps.dir)
                && plan.estimated_bytes.saturating_add(SPACE_RESERVE) > free
            {
                return Err(ApiError::new("NO_SPACE", format!("not enough disk space: need about {} MB, {} MB free", plan.estimated_bytes >> 20, free >> 20))
                    .with_data(json!({ "need": plan.estimated_bytes, "free": free })));
            }
            // a new request retries what failed before in its ranges
            let tl = ps.tl.clone().unwrap();
            for i in tl.segments_in(&rs, EDGE_EPS) {
                if ps.idx.failed.remove(&i).is_some() {
                    ps.idx_dirty = true;
                }
            }
            ps.idx.next_seq = next_seq();
            ps.idx.requests.push(FetchRequest {
                id: request_id.clone(),
                ranges: rs.clone(),
                priority: args.priority.unwrap_or_default(),
                reason: args.reason.clone().unwrap_or_else(|| "user".into()),
                song_ids: args.song_ids.clone().unwrap_or_default(),
                height,
                process: args.process,
                created_at: project::now(),
                seq: ps.idx.next_seq,
            });
            ps.idx_dirty = true;
            ps.dirty = true;
            Ok(plan)
        })??;
        if plan.request_id.is_some() {
            self.save_index(pid);
            self.video.wake.notify_one();
        } else if args.process {
            // already on disk: go straight to separation
            self.queue_process(pid, rs.clone());
        }
        Ok(plan)
    }

    /// Cancel one request (or all of the project's): segments not started are
    /// dropped, downloads in progress finish, everything fetched stays.
    pub fn video_cancel(&self, pid: &str, request_id: Option<&str>) -> ApiResult<usize> {
        let mut gone = Vec::new();
        let n = self.with_ps(pid, |ps| {
            let before = ps.idx.requests.len();
            ps.idx.requests.retain(|r| {
                let keep = request_id.is_some_and(|id| id != r.id);
                if !keep {
                    gone.push(r.id.clone());
                }
                keep
            });
            ps.idx_dirty = true;
            ps.dirty = true;
            before - ps.idx.requests.len()
        })?;
        if let Some(id) = request_id
            && n == 0
        {
            return Err(ApiError::new("NOT_FOUND", format!("request {id}")));
        }
        self.save_index(pid);
        for id in gone {
            self.emit(Event::VideoRequestDone { project_id: pid.into(), request_id: id, status: "cancelled".into(), failed: Vec::new(), code: None });
        }
        Ok(n)
    }

    pub fn video_prioritize(&self, pid: &str, request_id: &str, priority: Priority) -> ApiResult<()> {
        let found = self.with_ps(pid, |ps| {
            let next = next_seq();
            match ps.idx.requests.iter_mut().find(|r| r.id == request_id) {
                Some(r) => {
                    r.priority = priority;
                    // "fetch this first": also first among its priority
                    if priority == Priority::Now {
                        r.seq = 0;
                    } else {
                        r.seq = next;
                    }
                    ps.idx.next_seq = next;
                    ps.idx_dirty = true;
                    ps.dirty = true;
                    true
                }
                None => false,
            }
        })?;
        if !found {
            return Err(ApiError::new("NOT_FOUND", format!("request {request_id}")));
        }
        self.save_index(pid);
        self.video.wake.notify_one();
        Ok(())
    }

    /// Clear failures (in `ranges`, or all) so the requests still holding
    /// them try again; failed segments without a request get a new one.
    pub fn video_retry(self: &Arc<Self>, pid: &str, ranges_: Option<Vec<Range>>) -> ApiResult<Option<FetchPlan>> {
        let orphan = self.with_ps(pid, |ps| -> ApiResult<Vec<Range>> {
            if ps.vod_gone {
                return Err(ApiError::new("VOD_GONE", "the replay is no longer available"));
            }
            let tl = ps.tl.clone().ok_or_else(|| precondition("fetch the audio first"))?;
            let pick: Vec<usize> = match &ranges_ {
                Some(r) => tl.segments_in(r, 0.0).into_iter().filter(|i| ps.idx.failed.contains_key(i)).collect(),
                None => ps.idx.failed.keys().copied().collect(),
            };
            let queued: std::collections::BTreeSet<usize> = ps.idx.requests.iter().flat_map(|r| ps.needed(r)).collect();
            let orphan: Vec<usize> = pick.iter().copied().filter(|i| !queued.contains(i)).collect();
            for i in &pick {
                ps.idx.failed.remove(i);
            }
            ps.idx_dirty = true;
            ps.dirty = true;
            Ok(tl.ranges_of(orphan))
        })??;
        self.save_index(pid);
        self.video.wake.notify_one();
        if orphan.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.video_fetch(pid, FetchArgs { ranges: Some(orphan), reason: Some("retry".into()), ..Default::default() })?))
    }

    /// Pause / resume every video download of the app (persisted).
    pub fn video_pause(&self, paused: bool) -> ApiResult<()> {
        self.video.paused.store(paused, Ordering::Relaxed);
        {
            let mut st = self.video.st.lk();
            for ps in st.projects.values_mut() {
                if paused {
                    for (_, t) in ps.fetching.values() {
                        t.cancel();
                    }
                }
                ps.dirty = true;
            }
        }
        let mut prefs = super::EnginePrefs::load(&self.cfg.projects_root);
        prefs.video_paused = paused;
        prefs.save(&self.cfg.projects_root)?;
        self.video.wake.notify_one();
        Ok(())
    }

    // ── worker ──────────────────────────────────────────────────────────

    /// Start the scheduler (once) and resume requests left by a previous run.
    pub(crate) fn video_start(self: &Arc<Self>) {
        if self.video.started.swap(true, Ordering::SeqCst) || tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        // resume: projects with pending requests
        for e in std::fs::read_dir(&self.cfg.projects_root).into_iter().flatten().flatten() {
            let id = e.file_name().to_string_lossy().into_owned();
            if !project::Store::valid_id(&id) {
                continue;
            }
            let idx = VideoIndex::load(&e.path());
            if !idx.requests.is_empty() {
                tracing::info!("resuming {} video request(s) of {id}", idx.requests.len());
                let _ = self.with_ps(&id, |_| ());
            }
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let Some(eng) = weak.upgrade() else { return };
                {
                    let notified = eng.video.wake.notified();
                    eng.video_dispatch();
                    tokio::select! {
                        _ = notified => {}
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                    }
                }
                drop(eng);
            }
        });
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let Some(eng) = weak.upgrade() else { return };
                eng.video_flush();
            }
        });
    }

    /// Emit `video.changed` for projects that changed; save their index.
    fn video_flush(&self) {
        let paused = self.video.paused.load(Ordering::Relaxed);
        let mut out = Vec::new();
        let mut save = Vec::new();
        {
            let mut st = self.video.st.lk();
            let rate = st.rate.rate();
            for (pid, ps) in st.projects.iter_mut() {
                if ps.dirty {
                    ps.dirty = false;
                    out.push(ps.state(pid, paused, rate));
                }
                if ps.idx_dirty {
                    save.push(pid.clone());
                }
            }
        }
        for pid in save {
            self.save_index(&pid);
        }
        for s in out {
            self.emit(Event::VideoChanged { video: Box::new(s) });
        }
    }

    /// Start downloads while there are free workers.
    fn video_dispatch(self: &Arc<Self>) {
        if self.video.paused.load(Ordering::Relaxed) {
            return;
        }
        let mut st = self.video.st.lk();
        while st.inflight < WORKERS {
            let best = st
                .projects
                .iter()
                .filter(|(_, ps)| !ps.vod_gone && !ps.local && ps.tl.is_some())
                .filter_map(|(pid, ps)| ps.next_segment().map(|(pr, seq, i)| (pr, seq, pid.clone(), i)))
                .min_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
            let Some((_, seq, pid, i)) = best else { break };
            let ps = st.projects.get_mut(&pid).unwrap();
            let height = ps.idx.requests.iter().find(|r| r.seq == seq).and_then(|r| r.height);
            let preferred = crate::source::pick_variant(&self.variants_of(&pid), height);
            let v = ps.choose_variant(i, preferred);
            let token = tokio_util::sync::CancellationToken::new();
            ps.fetching.insert(i, (v, token.clone()));
            ps.dirty = true;
            let container = ps.container;
            let dir = ps.dir.clone();
            st.inflight += 1;
            let eng = self.clone();
            tokio::spawn(async move {
                let r = eng.fetch_segment(&pid, &dir, container, i, v, &token).await;
                eng.segment_done(&pid, i, r);
            });
        }
    }

    /// Renditions of a project (from the stored source info, no network).
    fn variants_of(&self, pid: &str) -> Vec<crate::hls::m3u8::Variant> {
        self.store.load(pid).ok().and_then(|p| p.source.map(|s| s.variants)).unwrap_or_default()
    }

    async fn playlist(&self, pid: &str, src: &crate::source::Source, v: usize) -> Result<Arc<MediaPlaylist>, ApiError> {
        let key = (pid.to_string(), v);
        // concurrent misses share one download
        let lock = self.resolve_locks.lk().entry(format!("{pid}#pl{v}")).or_default().clone();
        let _g = lock.lock().await;
        if let Some((pl, at)) = self.video.st.lk().playlists.get(&key)
            && at.elapsed() < super::SOURCE_TTL
        {
            return Ok(pl.clone());
        }
        let pl = Arc::new(src.variant_playlist(v).await?);
        self.video.st.lk().playlists.insert(key, (pl.clone(), Instant::now()));
        Ok(pl)
    }

    async fn drop_source_cache(&self, pid: &str) {
        self.sources.lock().await.remove(pid);
        self.video.st.lk().playlists.retain(|(p, _), _| p != pid);
    }

    /// Download one segment (rendition `v`, lower ones if the CDN copy is
    /// truncated), with [`SEGMENT_RETRIES`] retries. Writes `<index>.<ext>`.
    async fn fetch_segment(&self, pid: &str, dir: &Path, container: Container, i: usize, v: usize, cancel: &tokio_util::sync::CancellationToken) -> Result<(usize, u64), SegErr> {
        let failed = |code: &str, m: String| SegErr::Failed { code: code.into(), message: m };
        let p = self.store.load(pid).map_err(|e| failed("IO", e.to_string()))?;
        let mut refreshed = false;
        let mut attempt = 0u32;
        let mut truncated: Vec<usize> = Vec::new();
        'outer: loop {
            if cancel.is_cancelled() {
                return Err(SegErr::Cancelled);
            }
            let src = match self.source(&p).await {
                Ok(s) => s,
                Err(e) if e.code == "VOD_NOT_FOUND" => return Err(SegErr::VodGone(e.message)),
                Err(e) if e.code == "AUTH_REQUIRED" || e.code == "FORMAT" => return Err(failed(&e.code, e.message)),
                Err(e) => {
                    attempt += 1;
                    if attempt > SEGMENT_RETRIES {
                        return Err(failed(&e.code, e.message));
                    }
                    backoff(attempt, cancel).await?;
                    continue;
                }
            };
            // the requested rendition, then lower ones (CDN truncation only)
            let mut order: Vec<usize> = (0..src.info.variants.len()).collect();
            order.sort_by_key(|&k| std::cmp::Reverse(src.info.variants[k].pixels()));
            let h0 = src.info.variants.get(v).map(|x| x.height).unwrap_or(0);
            let mut cands = vec![v];
            cands.extend(order.into_iter().filter(|&k| k != v && src.info.variants[k].height < h0));
            let cands: Vec<usize> = cands.into_iter().filter(|k| !truncated.contains(k)).collect();
            for vv in cands {
                let pl = match self.playlist(pid, &src, vv).await {
                    Ok(pl) => pl,
                    Err(e) => {
                        attempt += 1;
                        if attempt > SEGMENT_RETRIES {
                            return Err(failed(&e.code, e.message));
                        }
                        backoff(attempt, cancel).await?;
                        continue 'outer;
                    }
                };
                if pl.segments.len() != src.playlist.segments.len() || i >= pl.segments.len() {
                    tracing::warn!("{pid}: rendition {vv} segmentation differs; skipping");
                    truncated.push(vv);
                    continue;
                }
                let vdir = store::variant_dir(dir, vv);
                if let Err(e) = tokio::fs::create_dir_all(&vdir).await {
                    return Err(failed("IO", e.to_string()));
                }
                if container == Container::Fmp4 && !store::init_path(dir, vv).is_file() {
                    let Some(u) = &pl.init_url else { return Err(failed("FORMAT", "rendition has no init segment".into())) };
                    match src.http.get_bytes(u).await {
                        Ok(b) => write_atomic_async(&store::init_path(dir, vv), &b).await.map_err(|e| failed("IO", e.to_string()))?,
                        Err(e) => {
                            attempt += 1;
                            if attempt > SEGMENT_RETRIES {
                                return Err(failed(super::net_code(&e), e.to_string()));
                            }
                            backoff(attempt, cancel).await?;
                            continue 'outer;
                        }
                    }
                }
                let url = pl.segments[i].clone();
                let r = tokio::select! {
                    _ = cancel.cancelled() => return Err(SegErr::Cancelled),
                    r = src.http.get_full(&url, 1) => r,
                };
                match r {
                    Ok((bytes, _)) => {
                        let ok = match container {
                            Container::Fmp4 => crate::hls::video_fetch::fmp4_complete(&bytes),
                            Container::Ts => !bytes.is_empty() && bytes[0] == 0x47 && bytes.len() % 188 == 0,
                        };
                        if !ok && container == Container::Fmp4 {
                            tracing::warn!("{pid}: segment {i} of rendition {vv} is truncated on the CDN; trying a lower one");
                            truncated.push(vv);
                            continue;
                        }
                        if !ok {
                            attempt += 1;
                            if attempt > SEGMENT_RETRIES {
                                return Err(failed("FORMAT", format!("segment {i} is not MPEG-TS")));
                            }
                            backoff(attempt, cancel).await?;
                            continue 'outer;
                        }
                        let path = store::segment_path(dir, vv, i, container);
                        write_atomic_async(&path, &bytes).await.map_err(|e| failed("IO", e.to_string()))?;
                        return Ok((vv, bytes.len() as u64));
                    }
                    Err(e) => {
                        let status = e.status();
                        if matches!(status, Some(403) | Some(404) | Some(410)) && !refreshed {
                            // expired playback token, or the VOD went away: resolve again
                            refreshed = true;
                            self.drop_source_cache(pid).await;
                            continue 'outer;
                        }
                        attempt += 1;
                        if attempt > SEGMENT_RETRIES {
                            return Err(failed(super::net_code(&e), e.to_string()));
                        }
                        backoff(attempt, cancel).await?;
                        continue 'outer;
                    }
                }
            }
            return Err(failed("CDN_TRUNCATED", format!("no rendition could serve segment {i} (truncated on the CDN)")));
        }
    }

    fn segment_done(self: &Arc<Self>, pid: &str, i: usize, r: Result<(usize, u64), SegErr>) {
        let mut done: Vec<(FetchRequest, bool, Vec<Range>, Option<String>)> = Vec::new();
        let mut gone = None;
        {
            let mut st = self.video.st.lk();
            st.inflight = st.inflight.saturating_sub(1);
            let mut fetched_sec = 0.0;
            if let Some(ps) = st.projects.get_mut(pid) {
                ps.fetching.remove(&i);
                ps.dirty = true;
                match r {
                    Ok((v, bytes)) => {
                        ps.have.insert(i, Have { kind: HaveKind::Raw(v), bytes });
                        if ps.idx.failed.remove(&i).is_some() {
                            ps.idx_dirty = true;
                        }
                        fetched_sec = ps.tl.as_ref().map(|t| t.dur(i)).unwrap_or(0.0);
                    }
                    Err(SegErr::Cancelled) => {}
                    Err(SegErr::VodGone(msg)) => {
                        ps.vod_gone = true;
                        for (_, (_, t)) in ps.fetching.iter() {
                            t.cancel();
                        }
                        for r in std::mem::take(&mut ps.idx.requests) {
                            let failed = ps.tl.as_ref().map(|t| t.ranges_of(ps.needed(&r))).unwrap_or_default();
                            done.push((r, false, failed, Some("VOD_GONE".into())));
                        }
                        ps.idx_dirty = true;
                        gone = Some(msg);
                    }
                    Err(SegErr::Failed { code, message }) => {
                        tracing::warn!("{pid}: segment {i} failed: {code}: {message}");
                        let at = project::now();
                        ps.idx.failed.insert(i, SegFailure { code, message, attempts: SEGMENT_RETRIES + 1, at });
                        ps.idx_dirty = true;
                    }
                }
                if gone.is_none() {
                    let (fin, keep): (Vec<FetchRequest>, Vec<FetchRequest>) = std::mem::take(&mut ps.idx.requests).into_iter().partition(|r| ps.finished(r));
                    ps.idx.requests = keep;
                    for r in fin {
                        let missing = ps.needed(&r);
                        let failed = ps.tl.as_ref().map(|t| t.ranges_of(missing.iter().copied())).unwrap_or_default();
                        let code = missing.first().and_then(|i| ps.idx.failed.get(i)).map(|f| f.code.clone());
                        ps.idx_dirty = true;
                        done.push((r, missing.is_empty(), failed, code));
                    }
                }
            }
            if fetched_sec > 0.0 {
                st.rate.add(fetched_sec);
            }
        }
        if !done.is_empty() {
            self.save_index(pid);
        }
        for (r, ok, failed, code) in done {
            self.emit(Event::VideoRequestDone {
                project_id: pid.into(),
                request_id: r.id.clone(),
                status: if ok { "done" } else { "failed" }.into(),
                failed,
                code,
            });
            if r.process && ok {
                let mut rs = r.ranges.clone();
                if let Ok(p) = self.get(pid) {
                    for id in &r.song_ids {
                        if let Some(s) = p.song(id) {
                            rs.push(s.with_margins(&p.settings));
                        }
                    }
                }
                self.queue_process(pid, ranges::normalize(&rs));
            }
        }
        if let Some(msg) = gone {
            let eng = self.clone();
            let pid = pid.to_string();
            tokio::spawn(async move {
                let _ = eng.set_vod(&pid, VodStatus { status: VodState::Gone, checked_at: project::now(), code: Some("VOD_NOT_FOUND".into()) }).await;
                tracing::warn!("{pid}: the VOD is gone ({msg})");
            });
        }
        self.video.wake.notify_one();
        self.video.changed.notify_waiters();
    }

    // ── replay check (S-E9) ─────────────────────────────────────────────

    async fn set_vod(&self, pid: &str, v: VodStatus) -> ApiResult<Project> {
        let gone = v.status == VodState::Gone;
        let p = self
            .mutate(pid, |p| {
                p.vod = Some(v);
                Ok(())
            })
            .await?;
        let _ = self.with_ps(pid, |ps| {
            ps.vod_gone = gone;
            ps.dirty = true;
        });
        Ok(p)
    }

    /// Is the replay still online? Never blocks opening a project; the UI
    /// calls it in the background. `gone` makes video requests fail at once.
    pub async fn vod_check(&self, pid: &str) -> ApiResult<VodStatus> {
        let p = self.get(pid)?;
        let (status, code) = if is_local(&p) {
            let ok = p.source.as_ref().is_some_and(|s| Path::new(&s.url).is_file());
            (if ok { VodState::Alive } else { VodState::Gone }, (!ok).then(|| "NOT_FOUND".to_string()))
        } else {
            match self.resolve_fresh(&p.input).await {
                Ok(src) => {
                    self.sources.lock().await.insert(p.id.clone(), (src, Instant::now()));
                    (VodState::Alive, None)
                }
                Err(e) if e.code == "VOD_NOT_FOUND" => (VodState::Gone, Some(e.code)),
                Err(e) => (VodState::Unknown, Some(e.code)),
            }
        };
        let v = VodStatus { status, checked_at: project::now(), code };
        if status == VodState::Gone {
            // pending requests can't be served any more
            let reqs: Vec<String> = self.with_ps(pid, |ps| ps.idx.requests.iter().map(|r| r.id.clone()).collect()).unwrap_or_default();
            if !reqs.is_empty() {
                let _ = self.video_cancel(pid, None);
            }
        }
        self.set_vod(pid, v.clone()).await?;
        Ok(v)
    }

    // ── alignment ───────────────────────────────────────────────────────

    /// Media start times of raw segment `i` of rendition `v` (cached).
    pub(crate) fn probe_raw(&self, pid: &str, v: usize, i: usize) -> ApiResult<Probe> {
        let key = format!("v{v}:{i}");
        let (dir, container, cached) = self.with_ps(pid, |ps| (ps.dir.clone(), ps.container, ps.idx.probes.get(&key).copied()))?;
        if let Some(p) = cached {
            return Ok(p);
        }
        let pr = media::probe_segment(&dir, container, v, i)?;
        self.with_ps(pid, |ps| {
            ps.idx.probes.insert(key, pr);
            ps.idx_dirty = true;
        })?;
        Ok(pr)
    }

    /// `m` of rendition `v` (`vod = media − m`), measured once against the
    /// analysis audio on the longest stretch of that rendition we have.
    /// Blocking (decodes); call from a worker thread.
    pub(crate) fn align_raw(&self, pid: &str, v: usize) -> ApiResult<Align> {
        let key = format!("v{v}");
        let (cached, stretch, tl) = self.with_ps(pid, |ps| {
            let longest = ps
                .stretches()
                .into_iter()
                .flat_map(|(_, _, pieces)| pieces)
                .filter_map(|pc| match pc {
                    Piece::Raw { variant, first, last } if variant == v => Some((first, last)),
                    _ => None,
                })
                .max_by_key(|(a, b)| b - a);
            (ps.idx.align.get(&key).cloned(), longest, ps.tl.clone())
        })?;
        let p = self.get(pid)?;
        let audio_ok = p.audio.as_ref().is_some_and(|a| !a.dropped);
        if let Some(a) = &cached
            && (a.ok || !audio_ok)
        {
            return Ok(a.clone());
        }
        let (first, last) = stretch.ok_or_else(|| precondition(format!("no video of rendition {v} on disk")))?;
        let tl = tl.ok_or_else(|| precondition("fetch the audio first"))?;
        let pr = self.probe_raw(pid, v, first)?;
        let predicted = pr.audio_start - tl.start(first);
        let (dir, container) = self.with_ps(pid, |ps| (ps.dir.clone(), ps.container))?;
        let pieces = [Piece::Raw { variant: v, first, last: last.min(first + 40) }];
        let a = if audio_ok {
            let src = self.analysis_source(&p)?;
            measure_m(&dir, container, &pieces, pr.audio_start, predicted, first, |s, d| src.decode(s, d, 8000, 1))
        } else {
            Align { m: predicted, method: "predicted".into(), corr: 0.0, predicted, ok: false, at_segment: first }
        };
        tracing::info!("{pid}: rendition {v} alignment {} m={:.4} (predicted {:.4}) corr {:.2}", a.method, a.m, a.predicted, a.corr);
        if a.ok || cached.is_none() {
            self.with_ps(pid, |ps| {
                ps.idx.align.insert(key, a.clone());
                ps.idx_dirty = true;
            })?;
            self.save_index(pid);
        }
        Ok(a)
    }

    /// `m` of the local recording (`vod = file time − m`). Blocking.
    pub(crate) fn align_local(&self, p: &Project) -> ApiResult<Align> {
        if let Some(a) = self.with_ps(&p.id, |ps| ps.idx.align.get("local").cloned())? {
            return Ok(a);
        }
        let file = PathBuf::from(&p.source.as_ref().unwrap().url);
        let pr = media::probe_input(&file.to_string_lossy())?;
        let pieces = [Piece::Local { file, track: p.settings.audio_track }];
        let dir = self.store.dir(&p.id);
        let src = self.analysis_source(p)?;
        let a = measure_m(&dir, Container::Fmp4, &pieces, pr.audio_start, pr.audio_start, 0, |s, d| src.decode(s, d, 8000, 1));
        self.with_ps(&p.id, |ps| {
            ps.idx.align.insert("local".into(), a.clone());
            ps.idx_dirty = true;
        })?;
        self.save_index(&p.id);
        Ok(a)
    }

    /// VOD time of decoded sample 0 of a stretch starting with `piece`. Blocking.
    pub(crate) fn decode_origin(&self, p: &Project, piece: &Piece) -> ApiResult<f64> {
        Ok(match piece {
            Piece::Raw { variant, first, .. } => self.probe_raw(&p.id, *variant, *first)?.audio_start - self.align_raw(&p.id, *variant)?.m,
            Piece::Legacy { file, origin, .. } => {
                // the adopted stems put file t on the grid at round(origin·rate):
                // round the same way so the fresh decode lands on the same samples
                let pr = media::probe_input(&file.to_string_lossy())?;
                let r = crate::stems::STEM_RATE as f64;
                ((origin * r).round() + ((pr.audio_start - pr.format_start).max(0.0) * r).round()) / r
            }
            Piece::Local { file, .. } => media::probe_input(&file.to_string_lossy())?.audio_start - self.align_local(p)?.m,
        })
    }

    // ── views (S-E4) ────────────────────────────────────────────────────

    /// A playable MP4 with the picture at VOD time `t` (or covering
    /// `[start, end]` as far as one file can). Builds (remuxes) it if needed —
    /// a few seconds for a long range. None-like error `VIDEO_MISSING` when
    /// there is no video at `t`.
    pub async fn video_view(self: &Arc<Self>, pid: &str, t: f64, end: Option<f64>) -> ApiResult<VideoView> {
        let p = self.get(pid)?;
        let lock = self.video.view_locks.lk().entry(pid.to_string()).or_default().clone();
        let _g = lock.lock().await;
        let eng = self.clone();
        tokio::task::spawn_blocking(move || eng.view_blocking(&p, t, end)).await?
    }

    fn view_blocking(&self, p: &Project, t: f64, end: Option<f64>) -> ApiResult<VideoView> {
        let pid = &p.id;
        let missing = || ApiError::new("VIDEO_MISSING", format!("no video at {t:.1} s")).with_data(json!({ "ranges": [[t, end.unwrap_or(t)]] }));
        if is_local(p) {
            return self.local_view(p, t, end);
        }
        let (tl, piece, dir, container, heights) = self.with_ps(pid, |ps| {
            let tl = ps.tl.clone();
            let seg = tl.as_ref().and_then(|tl| tl.seg_at(t));
            let piece = seg.and_then(|s| ps.stretches().into_iter().flat_map(|(_, _, pcs)| pcs).find(|pc| pc.first() <= s && s < pc.last()));
            (tl, piece, ps.dir.clone(), ps.container, ps.heights.clone())
        })?;
        let tl = tl.ok_or_else(|| precondition("fetch the audio first"))?;
        let piece = piece.ok_or_else(missing)?;
        match piece {
            Piece::Legacy { file, variant, first, last, origin } => Ok(VideoView {
                file: rel(&dir, &file),
                path: file,
                origin,
                start: tl.start(first),
                end: tl.end(last - 1),
                height: heights.get(variant).copied(),
                kind: "legacy".into(),
            }),
            Piece::Raw { variant, first, last } => {
                // whole stretch, or a window around t
                let (a, b) = if tl.end(last - 1) - tl.start(first) <= VIEW_MAX_SEC {
                    (first, last)
                } else {
                    let want_end = end.unwrap_or(t).max(t + VIEW_MAX_SEC * 2.0 / 3.0);
                    let s = tl.segments_for(t - VIEW_MAX_SEC / 3.0, want_end, 0.0);
                    (s.start.max(first), s.end.min(last))
                };
                let key = format!("v{variant}_{a}_{b}");
                let file = format!("{}/views/{key}.mp4", store::DIR);
                let path = dir.join(&file);
                let m = self.align_raw(pid, variant)?.m;
                let origin = self.probe_raw(pid, variant, a)?.format_start - m;
                if !path.is_file() {
                    std::fs::create_dir_all(path.parent().unwrap())?;
                    media::remux(&dir, container, &[Piece::Raw { variant, first: a, last: b }], &path, &Default::default())?;
                }
                let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.touch_view(pid, ViewEntry { key, file: file.clone(), variant, first: a, last: b, origin, bytes, last_used: project::now() });
                Ok(VideoView { file, path, origin, start: tl.start(a), end: tl.end(b - 1), height: heights.get(variant).copied(), kind: "segments".into() })
            }
            Piece::Local { .. } => Err(missing()),
        }
    }

    fn local_view(&self, p: &Project, t: f64, end: Option<f64>) -> ApiResult<VideoView> {
        let src = p.source.as_ref().unwrap();
        let file = PathBuf::from(&src.url);
        let dur = src.duration.max(p.audio.as_ref().map(|a| a.duration).unwrap_or(0.0));
        let a = (t - VIEW_MAX_SEC / 3.0).max(0.0);
        let b = (end.unwrap_or(t).max(t + VIEW_MAX_SEC * 2.0 / 3.0)).min(if dur > 0.0 { dur } else { f64::MAX });
        let m = self.align_local(p)?.m;
        // media window [a + m, b + m] of the recording
        let key = format!("local_{}_{}", (a * 1000.0) as u64, (b * 1000.0) as u64);
        let rel_file = format!("{}/views/{key}.mp4", store::DIR);
        let dir = self.store.dir(&p.id);
        let path = dir.join(&rel_file);
        let origin_file = path.with_extension("origin");
        let kf = match std::fs::read_to_string(&origin_file).ok().and_then(|s| s.trim().parse::<f64>().ok()).filter(|_| path.is_file()) {
            Some(kf) => kf,
            None => {
                std::fs::create_dir_all(path.parent().unwrap())?;
                let v = crate::local::cut_clip(&file, (a + m).max(0.0), b + m, p.settings.audio_track, &path, &Default::default())?;
                std::fs::write(&origin_file, v.vod_start.to_string())?;
                v.vod_start
            }
        };
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        self.touch_view(&p.id, ViewEntry { key, file: rel_file.clone(), variant: 0, first: 0, last: 0, origin: kf - m, bytes, last_used: project::now() });
        Ok(VideoView { file: rel_file, path, origin: kf - m, start: kf - m, end: b, height: src.variants.first().map(|v| v.height), kind: "local".into() })
    }

    /// Record a view as most recently used; delete the oldest beyond [`VIEWS_KEPT`].
    fn touch_view(&self, pid: &str, e: ViewEntry) {
        let evict = self
            .with_ps(pid, |ps| {
                ps.idx.views.retain(|x| x.key != e.key);
                ps.idx.views.push(e);
                ps.idx.views.sort_by(|a, b| b.last_used.cmp(&a.last_used));
                let old: Vec<ViewEntry> = ps.idx.views.drain(VIEWS_KEPT.min(ps.idx.views.len())..).collect();
                ps.idx_dirty = true;
                (ps.dir.clone(), old)
            })
            .ok();
        if let Some((dir, old)) = evict {
            for v in old {
                let f = dir.join(&v.file);
                if std::fs::remove_file(&f).is_ok() || !f.exists() {
                    let _ = std::fs::remove_file(f.with_extension("origin"));
                } else {
                    // in use (Windows): keep tracking it, try again next time
                    let _ = self.with_ps(pid, |ps| ps.idx.views.push(v));
                }
            }
        }
        self.save_index(pid);
    }

    /// Delete all views (cleanup); returns bytes freed.
    pub(crate) fn drop_views(&self, pid: &str) -> u64 {
        let (dir, views) = match self.with_ps(pid, |ps| (ps.dir.clone(), std::mem::take(&mut ps.idx.views))) {
            Ok(x) => x,
            Err(_) => return 0,
        };
        let vdir = dir.join(store::DIR).join("views");
        let freed = store::dir_size(&vdir);
        let _ = std::fs::remove_dir_all(&vdir);
        let left = store::dir_size(&vdir);
        let _ = self.with_ps(pid, |ps| {
            if left > 0 {
                ps.idx.views = views.into_iter().filter(|v| dir.join(&v.file).is_file()).collect();
            }
            ps.idx_dirty = true;
        });
        self.save_index(pid);
        freed.saturating_sub(left)
    }

    /// Wait until the project's video changes (or `timeout`).
    pub(crate) async fn video_wait_change(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.video.changed.notified()).await;
    }

    /// VOD ranges of a project's video on disk.
    pub fn video_have(&self, pid: &str) -> ApiResult<Vec<Range>> {
        self.with_ps(pid, |ps| ps.have_ranges())
    }

}

/// Request order across all projects (and restarts): microseconds since 1970.
fn next_seq() -> u64 {
    use std::sync::atomic::AtomicU64;
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = chrono::Utc::now().timestamp_micros().max(1) as u64;
    // strictly increasing even within one microsecond
    let mut prev = LAST.load(Ordering::Relaxed);
    loop {
        let next = now.max(prev + 1);
        match LAST.compare_exchange(prev, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return next,
            Err(p) => prev = p,
        }
    }
}

fn rel(dir: &Path, f: &Path) -> String {
    f.strip_prefix(dir).unwrap_or(f).to_string_lossy().replace('\\', "/")
}

async fn backoff(attempt: u32, cancel: &tokio_util::sync::CancellationToken) -> Result<(), SegErr> {
    let ms = 1000u64 << (attempt.saturating_sub(1)).min(4);
    tokio::select! {
        _ = cancel.cancelled() => Err(SegErr::Cancelled),
        _ = tokio::time::sleep(Duration::from_millis(ms)) => Ok(()),
    }
}

async fn write_atomic_async(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("{}.part", path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default()));
    tokio::fs::write(&tmp, bytes).await?;
    tokio::fs::rename(&tmp, path).await
}

/// Missing / total seconds and size of fetching `rs`.
fn plan_of(ps: &PState, p: &Project, rs: &[Range], height: Option<u32>, request_id: Option<String>) -> ApiResult<FetchPlan> {
    let tl = ps.tl.as_ref().ok_or_else(|| precondition("fetch the audio first (it places the video segments)"))?;
    let segs = tl.segments_in(rs, EDGE_EPS);
    let missing: Vec<usize> = if ps.local { Vec::new() } else { segs.iter().copied().filter(|i| !ps.have.contains_key(i)).collect() };
    let missing_sec = tl.seconds(missing.iter().copied());
    let variants = p.source.as_ref().map(|s| s.variants.clone()).unwrap_or_default();
    let bw = variants.get(crate::source::pick_variant(&variants, height)).map(|v| v.bandwidth).unwrap_or(8_000_000);
    Ok(FetchPlan {
        request_id,
        total_sec: tl.seconds(segs.iter().copied()),
        missing_sec,
        estimated_bytes: (missing_sec * bw as f64 / 8.0) as u64,
        ranges: tl.ranges_of(segs),
    })
}

/// Measure `m` (`vod = media − m`) by cross-correlating a stretch's audio with
/// the analysis audio near the prediction (tries a few spots: a silent intro
/// can't be matched). `media0` = media time of the stretch's first audio
/// sample. Falls back to the prediction, flagged `ok: false`.
pub(crate) fn measure_m(
    dir: &Path,
    container: Container,
    pieces: &[Piece],
    media0: f64,
    predicted: f64,
    at_segment: usize,
    analysis: impl Fn(f64, f64) -> ApiResult<Vec<f32>>,
) -> Align {
    const RATE: u32 = 8000;
    const W: f64 = 2.0;
    let fallback = |corr: f32| Align { m: predicted, method: "predicted".into(), corr, predicted, ok: false, at_segment };
    let pcm = match media::decode(dir, container, pieces, RATE, 1).and_then(|mut d| {
        // up to ~3 min is plenty for the spots below
        let mut out = Vec::new();
        let mut buf = vec![0f32; 1 << 15];
        while out.len() < 180 * RATE as usize {
            let n = d.pcm.read(&mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    }) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!("alignment decode failed: {e}");
            return fallback(0.0);
        }
    };
    let dur = pcm.len() as f64 / RATE as f64;
    let mut best = 0f32;
    for t0 in [5.0, 60.0, 120.0, 0.0] {
        let len = (dur - t0).min(40.0);
        if len < 8.0 {
            continue;
        }
        let clip = &pcm[(t0 * RATE as f64) as usize..((t0 + len) * RATE as f64) as usize];
        // expected VOD time of clip sample 0
        let expect = media0 + t0 - predicted;
        let s0 = (expect - W).max(0.0);
        let Ok(ana) = analysis(s0, len + 2.0 * W) else { continue };
        if ana.is_empty() {
            continue;
        }
        let (lag, corr) = crate::audio::align::find_lag(&ana, clip, RATE, (expect - s0) + W);
        let m = media0 + t0 - (s0 + lag);
        tracing::debug!("alignment @{t0}s: m {m:.4} (predicted {predicted:.4}) corr {corr:.3}");
        if corr >= 0.3 && (m - predicted).abs() <= W {
            return Align { m, method: "measured".into(), corr, predicted, ok: true, at_segment };
        }
        best = best.max(corr);
    }
    fallback(best)
}

impl ApiError {
    /// Attach machine-readable details (sent as JSON-RPC `error.data`).
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

