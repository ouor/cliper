//! Segment-based video (cycle 2, GitHub #1): the video of a VOD is a set of
//! downloaded HLS segments on the VOD timeline, not a file per song.
//!
//! * [`store`]: files on disk + `index.json`
//! * [`timeline`]: segment ↔ VOD time
//! * [`media`]: ffmpeg over stretches of segments (decode, remux, probe)
//! * this module: the coverage model and the scheduler's bookkeeping (what is
//!   queued / fetching / failed), as plain data so it can be unit-tested; the
//!   async parts live in `engine_video.rs`.

pub mod media;
pub mod store;
pub mod timeline;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::Instant;

use serde::Serialize;

use crate::ranges::Range;
use crate::source::Container;
use store::{FetchRequest, Have, HaveKind, Priority, VideoIndex};
use timeline::{EDGE_EPS, Timeline};

/// Retries per segment after the first attempt (S-E3).
pub const SEGMENT_RETRIES: u32 = 3;
/// Concurrent segment downloads the scheduler starts (the HTTP layer caps
/// all traffic at 4 anyway, see `http::MAX_IN_FLIGHT`).
pub const WORKERS: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum CovState {
    /// on disk
    Have,
    /// downloading now
    Fetching,
    /// requested, waiting for a slot
    Queued,
    /// gave up after the retries (see `code`)
    Failed,
}

/// One stretch of the VOD with the same video state (absent = no video).
#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CoverageRange {
    pub start: f64,
    pub end: f64,
    pub state: CovState,
    /// rendition (have / fetching)
    #[cfg_attr(feature = "ts", ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// failed: engine error code (NETWORK, CDN_TRUNCATED, VOD_GONE, …)
    #[cfg_attr(feature = "ts", ts(optional))]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// Progress of one song inside a request (its range ± margins).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SongProgress {
    pub song_id: String,
    pub total_sec: f64,
    pub have_sec: f64,
    pub failed_sec: f64,
}

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RequestStatus {
    pub id: String,
    pub priority: Priority,
    pub reason: String,
    pub song_ids: Vec<String>,
    pub ranges: Vec<Range>,
    pub process: bool,
    /// runs even while video downloads are paused (an export or a job waits for it)
    pub force: bool,
    /// seconds of video this request needs in total (whole segments)
    pub total_sec: f64,
    pub have_sec: f64,
    pub fetching_sec: f64,
    pub failed_sec: f64,
    /// still to download (queued + fetching)
    pub remaining_sec: f64,
    /// per song of `song_ids` (whole segments of its range ± margins)
    pub songs: Vec<SongProgress>,
    pub created_at: String,
}

/// Everything the UI needs to draw the video lane (S-E1) and the jobs panel.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VideoState {
    pub project_id: String,
    /// sorted, non-overlapping; gaps = no video
    pub ranges: Vec<CoverageRange>,
    pub requests: Vec<RequestStatus>,
    /// app-wide pause (video.pause)
    pub paused: bool,
    pub have_sec: f64,
    pub fetching_sec: f64,
    pub queued_sec: f64,
    pub failed_sec: f64,
    /// rough time left for this project's queue at the current speed
    pub eta_sec: Option<f64>,
    /// bytes of video on disk (segments + adopted clips, not the view cache)
    pub bytes: u64,
    /// local recording: the whole file is "have", nothing is ever fetched
    pub local: bool,
    /// the network is down: downloads wait and retry by themselves (app-wide)
    pub offline: Option<OfflineInfo>,
    /// problems met loading this project's video index (a corrupt file set aside)
    pub warnings: Vec<String>,
}

/// "Offline, waiting" (R-3): nothing is marked failed while the network is down.
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct OfflineInfo {
    /// when the network was first seen down
    pub since: String,
    /// seconds until the next try
    pub retry_in_sec: f64,
    /// last error, for logs
    pub reason: String,
}

/// Per-project scheduler state (in memory; persisted parts live in `idx`).
pub struct PState {
    pub dir: PathBuf,
    pub idx: VideoIndex,
    pub have: BTreeMap<usize, Have>,
    /// segment → (rendition, cancel)
    pub fetching: HashMap<usize, (usize, tokio_util::sync::CancellationToken)>,
    pub tl: Option<Timeline>,
    pub container: Container,
    /// rendition folder → height (from `idx.renditions`; local: the file's)
    pub heights: Vec<u32>,
    /// the renditions (for picking one by height without a network call)
    pub variants: Vec<crate::hls::m3u8::Variant>,
    pub local: bool,
    pub vod_gone: bool,
    /// emit `video.changed`
    pub dirty: bool,
    /// write index.json
    pub idx_dirty: bool,
    /// a cleanup (`media.drop`) is deleting files: nothing new starts (A-4)
    pub dropping: bool,
}

impl PState {
    pub fn height(&self, variant: usize) -> Option<u32> {
        self.heights.get(variant).copied().filter(|h| *h > 0)
    }

    /// Recompute `heights` from the rendition table.
    pub fn refresh_heights(&mut self) {
        if self.local {
            self.heights = self.variants.iter().map(|v| v.height).collect();
            return;
        }
        let n = self.idx.renditions.keys().next_back().map(|n| n + 1).unwrap_or(0);
        let mut h = vec![0u32; n];
        for (&k, r) in &self.idx.renditions {
            h[k] = r.height;
        }
        self.heights = h;
    }

    /// Is the rendition of this folder still in the current rendition list?
    pub fn offered(&self, folder: usize) -> bool {
        self.variants.is_empty() || self.idx.renditions.get(&folder).is_some_and(|r| self.variants.iter().any(|x| store::rendition_key(x) == r.key))
    }

    /// Folder of the rendition `video_height`-style `height` picks from the
    /// current list (a folder number is assigned on first use).
    pub fn preferred_folder(&mut self, height: Option<u32>) -> usize {
        if self.variants.is_empty() {
            return 0;
        }
        let v = &self.variants[crate::source::pick_variant(&self.variants, height)];
        let before = self.idx.renditions.len();
        let f = self.idx.folder_for(&store::Rendition::of(v));
        if self.idx.renditions.len() != before {
            self.idx_dirty = true;
            self.refresh_heights();
        }
        f
    }

    /// Rendition of segment `i` if it is on disk or being fetched.
    pub fn variant_of(&self, i: usize) -> Option<usize> {
        if let Some(h) = self.have.get(&i) {
            return Some(h.kind.variant(&self.idx));
        }
        self.fetching.get(&i).map(|(v, _)| *v)
    }

    /// Segments a request still needs (not on disk).
    pub fn needed(&self, r: &FetchRequest) -> Vec<usize> {
        let Some(tl) = &self.tl else { return Vec::new() };
        tl.segments_in(&r.ranges, EDGE_EPS).into_iter().filter(|i| !self.have.contains_key(i)).collect()
    }

    /// Is every segment of the request on disk or failed?
    pub fn finished(&self, r: &FetchRequest) -> bool {
        self.needed(r).iter().all(|i| self.idx.failed.contains_key(i))
    }

    /// Next segment to fetch for this project: best (priority, seq) request
    /// first, earliest segment first; skips what is on disk, being fetched or
    /// failed. Returns (priority, seq, segment, request id). While `paused`,
    /// only requests allowed to run anyway (`force`, an export waiting) count.
    pub fn next_segment(&self, paused: bool) -> Option<(Priority, u64, usize, String)> {
        let mut reqs: Vec<&FetchRequest> = self.idx.requests.iter().filter(|r| !paused || r.force).collect();
        reqs.sort_by_key(|r| (r.priority, r.seq));
        for r in reqs {
            if let Some(i) = self.needed(r).into_iter().find(|i| !self.fetching.contains_key(i) && !self.idx.failed.contains_key(i)) {
                return Some((r.priority, r.seq, i, r.id.clone()));
            }
        }
        None
    }

    /// Rendition folder of the video on disk / being fetched at segment `i`
    /// (an adopted clip: the folder of its rendition key); None when that
    /// rendition isn't offered any more.
    pub fn usable_variant_of(&self, i: usize) -> Option<usize> {
        let f = match self.have.get(&i) {
            Some(h) => h.kind.variant(&self.idx),
            None => self.fetching.get(&i).map(|(v, _)| *v)?,
        };
        Some(f).filter(|&f| self.offered(f))
    }

    /// Rendition for segment `i`: continue the video next to the gap `i` is in
    /// (on disk, adopted clip, or being fetched; looking across segments that
    /// are queued too), so a range stays one stream and joins without a seam;
    /// else the request's height. The left side wins over the right.
    pub fn choose_variant(&self, i: usize, preferred: usize, queued: &std::collections::BTreeSet<usize>) -> usize {
        let walk = |mut j: usize, step: isize| -> Option<usize> {
            for _ in 0..100_000 {
                if self.have.contains_key(&j) || self.fetching.contains_key(&j) {
                    return self.usable_variant_of(j);
                }
                if !queued.contains(&j) {
                    return None;
                }
                j = j.checked_add_signed(step)?;
            }
            None
        };
        let left = i.checked_sub(1).and_then(|j| walk(j, -1));
        left.or_else(|| walk(i + 1, 1)).filter(|v| self.idx.renditions.contains_key(v)).unwrap_or(preferred)
    }

    /// [`PState::choose_variant`] for every segment of `segs` at once (one
    /// pass per gap instead of one walk per segment, R-12).
    pub fn choose_variants(&self, segs: &[usize], preferred: usize, queued: &std::collections::BTreeSet<usize>) -> HashMap<usize, usize> {
        let mut out = HashMap::new();
        for (a, b) in timeline::runs(segs) {
            let v = self.choose_variant(a, preferred, queued);
            for i in a..b {
                out.insert(i, v);
            }
        }
        out
    }

    /// Segments some request still needs.
    pub fn queued(&self) -> std::collections::BTreeSet<usize> {
        self.idx.requests.iter().flat_map(|r| self.needed(r)).collect()
    }

    pub fn coverage(&self) -> Vec<CoverageRange> {
        let Some(tl) = &self.tl else { return Vec::new() };
        if self.local {
            return vec![CoverageRange { start: 0.0, end: tl.duration(), state: CovState::Have, height: self.height(0), code: None }];
        }
        let queued: std::collections::BTreeSet<usize> = self.idx.requests.iter().flat_map(|r| self.needed(r)).collect();
        let mut keys: Vec<usize> = self.have.keys().chain(self.fetching.keys()).chain(self.idx.failed.keys()).chain(queued.iter()).copied().collect();
        keys.sort_unstable();
        keys.dedup();
        let mut out: Vec<(usize, CoverageRange)> = Vec::new();
        for i in keys {
            if !tl.contains(i) {
                continue;
            }
            let (state, height, code) = if let Some(h) = self.have.get(&i) {
                (CovState::Have, self.height(h.kind.variant(&self.idx)), None)
            } else if let Some((v, _)) = self.fetching.get(&i) {
                (CovState::Fetching, self.height(*v), None)
            } else if let Some(f) = self.idx.failed.get(&i) {
                (CovState::Failed, None, Some(f.code.clone()))
            } else {
                (CovState::Queued, None, None)
            };
            match out.last_mut() {
                Some((last, r)) if *last + 1 == i && r.state == state && r.height == height && r.code == code => {
                    r.end = tl.end(i);
                    *last = i;
                }
                _ => out.push((i, CoverageRange { start: tl.start(i), end: tl.end(i), state, height, code })),
            }
        }
        out.into_iter().map(|(_, r)| r).collect()
    }

    pub fn request_status(&self, r: &FetchRequest) -> RequestStatus {
        let tl = self.tl.as_ref();
        let all: Vec<usize> = tl.map(|t| t.segments_in(&r.ranges, EDGE_EPS).into_iter().collect()).unwrap_or_default();
        let sec = |f: &dyn Fn(usize) -> bool| -> f64 { tl.map(|t| t.seconds(all.iter().copied().filter(|&i| f(i)))).unwrap_or(0.0) };
        let total_sec = sec(&|_| true);
        let have_sec = sec(&|i| self.have.contains_key(&i));
        let fetching_sec = sec(&|i| !self.have.contains_key(&i) && self.fetching.contains_key(&i));
        let failed_sec = sec(&|i| !self.have.contains_key(&i) && !self.fetching.contains_key(&i) && self.idx.failed.contains_key(&i));
        RequestStatus {
            id: r.id.clone(),
            priority: r.priority,
            reason: r.reason.clone(),
            force: r.force,
            song_ids: r.song_ids.clone(),
            ranges: r.ranges.clone(),
            process: r.process,
            total_sec,
            have_sec,
            fetching_sec,
            failed_sec,
            remaining_sec: (total_sec - have_sec - failed_sec).max(0.0),
            songs: r
                .song_ids
                .iter()
                .zip(&r.song_ranges)
                .map(|(id, &rg)| {
                    let segs: Vec<usize> = tl.map(|t| t.segments_in(&[rg], EDGE_EPS).into_iter().collect()).unwrap_or_default();
                    let sec = |f: &dyn Fn(usize) -> bool| -> f64 { tl.map(|t| t.seconds(segs.iter().copied().filter(|&i| f(i)))).unwrap_or(0.0) };
                    SongProgress {
                        song_id: id.clone(),
                        total_sec: sec(&|_| true),
                        have_sec: sec(&|i| self.have.contains_key(&i)),
                        failed_sec: sec(&|i| !self.have.contains_key(&i) && self.idx.failed.contains_key(&i)),
                    }
                })
                .collect(),
            created_at: r.created_at.clone(),
        }
    }

    pub fn state(&self, project_id: &str, paused: bool, rate: Option<f64>, offline: Option<OfflineInfo>) -> VideoState {
        let ranges = self.coverage();
        let sum = |s: CovState| ranges.iter().filter(|r| r.state == s).map(|r| r.end - r.start).fold(0.0, |a, b| a + b);
        let queued_sec = sum(CovState::Queued);
        let fetching_sec = sum(CovState::Fetching);
        let bytes = self.have.values().filter(|h| matches!(h.kind, HaveKind::Raw(_))).map(|h| h.bytes).sum::<u64>()
            + self.idx.legacy.iter().map(|l| std::fs::metadata(self.dir.join(&l.file)).map(|m| m.len()).unwrap_or(0)).sum::<u64>();
        VideoState {
            project_id: project_id.to_string(),
            requests: self.idx.requests.iter().map(|r| self.request_status(r)).collect(),
            paused,
            have_sec: sum(CovState::Have),
            fetching_sec,
            queued_sec,
            failed_sec: sum(CovState::Failed),
            eta_sec: rate.filter(|r| *r > 0.0).map(|r| ((queued_sec + fetching_sec) / r).round()).filter(|_| queued_sec + fetching_sec > 0.0),
            bytes,
            local: self.local,
            offline,
            warnings: self.idx.warnings.clone(),
            ranges,
        }
    }

    /// VOD ranges on disk.
    pub fn have_ranges(&self) -> Vec<Range> {
        match &self.tl {
            Some(tl) if self.local => vec![(0.0, tl.duration())],
            Some(tl) => tl.ranges_of(self.have.keys().copied()),
            None => Vec::new(),
        }
    }

    /// Contiguous stretches on disk as pieces (split where the source changes:
    /// rendition, legacy clip). Returns `(first, last, pieces)` per stretch.
    pub fn stretches(&self) -> Vec<(usize, usize, Vec<media::Piece>)> {
        let mut out: Vec<(usize, usize, Vec<media::Piece>)> = Vec::new();
        let mut prev: Option<(usize, HaveKind)> = None;
        for (&i, h) in &self.have {
            let contiguous = prev.is_some_and(|(p, _)| p + 1 == i);
            if !contiguous {
                out.push((i, i, Vec::new()));
            }
            let st = out.last_mut().unwrap();
            st.1 = i + 1;
            let same = contiguous && prev.is_some_and(|(_, k)| k == h.kind);
            if same {
                match st.2.last_mut() {
                    Some(media::Piece::Raw { last, .. }) | Some(media::Piece::Legacy { last, .. }) => *last = i + 1,
                    _ => {}
                }
            } else {
                st.2.push(match h.kind {
                    HaveKind::Raw(v) => media::Piece::Raw { variant: v, first: i, last: i + 1 },
                    HaveKind::Legacy(n) => {
                        let l = &self.idx.legacy[n];
                        media::Piece::Legacy { file: self.dir.join(&l.file), variant: l.variant, first: i, last: i + 1, origin: l.origin, file_first: l.first, file_last: l.last }
                    }
                });
            }
            prev = Some((i, h.kind));
        }
        out
    }
}

/// Fill the rendition table of an index written before it existed (A-1):
/// folder `v<N>` holds what was rendition N of the project's stored list
/// (the list the old engine indexed), adopted clips get the key of their
/// recorded rendition (found by resolution if the position changed) and are
/// renumbered to a folder of that key. Returns whether anything changed.
pub fn adopt_renditions(idx: &mut VideoIndex, raw_folders: &[usize], stored: &[crate::hls::m3u8::Variant]) -> bool {
    let mut changed = false;
    if idx.version < 2 {
        let mut folders: Vec<usize> = raw_folders.to_vec();
        folders.extend(idx.align.keys().filter_map(|k| k.strip_prefix('v')).filter_map(|n| n.parse::<usize>().ok()));
        folders.sort_unstable();
        folders.dedup();
        for n in folders {
            if let (false, Some(v)) = (idx.renditions.contains_key(&n), stored.get(n)) {
                idx.renditions.insert(n, store::Rendition::of(v));
                changed = true;
            }
        }
    }
    for k in 0..idx.legacy.len() {
        if idx.legacy[k].key.is_some() {
            continue;
        }
        let l = idx.legacy[k].clone();
        let var = stored.get(l.variant).filter(|v| l.resolution.is_empty() || v.resolution() == l.resolution).or_else(|| stored.iter().find(|v| v.resolution() == l.resolution));
        let r = match var {
            Some(v) => store::Rendition::of(v),
            None => {
                let (w, h) = l.resolution.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or((0, 0));
                store::Rendition { key: format!("legacy:{}", l.resolution), width: w, height: h, bandwidth: 0, codecs: None }
            }
        };
        let f = idx.folder_for(&r);
        idx.legacy[k].key = Some(r.key);
        idx.legacy[k].variant = f;
        changed = true;
    }
    if idx.version < 2 {
        idx.version = 2;
        changed = true;
    }
    changed
}

/// Seconds of video fetched per wall-clock second over the last minute (ETA).
#[derive(Default)]
pub struct RateMeter {
    samples: std::collections::VecDeque<(Instant, f64)>,
}

impl RateMeter {
    pub fn add(&mut self, sec: f64) {
        let now = Instant::now();
        self.samples.push_back((now, sec));
        while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t).as_secs() > 60) {
            self.samples.pop_front();
        }
    }
    pub fn rate(&self) -> Option<f64> {
        let (first, _) = self.samples.front()?;
        let el = first.elapsed().as_secs_f64().max(5.0);
        Some(self.samples.iter().map(|(_, s)| s).sum::<f64>() / el)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use store::{Have, HaveKind, SegFailure};

    fn var(w: u32, h: u32, bw: u64) -> crate::hls::m3u8::Variant {
        crate::hls::m3u8::Variant { width: w, height: h, bandwidth: bw, codecs: None, url: String::new() }
    }

    /// folders 0, 1, 2 = 1080p, 720p, 144p; no current list (everything offered)
    fn ps() -> PState {
        let mut idx = VideoIndex { version: store::INDEX_VERSION, ..Default::default() };
        for v in [var(1920, 1080, 8_000_000), var(1280, 720, 4_000_000), var(256, 144, 200_000)] {
            idx.folder_for(&store::Rendition::of(&v));
        }
        let mut p = PState {
            dir: PathBuf::new(),
            idx,
            have: BTreeMap::new(),
            fetching: HashMap::new(),
            tl: Some(Timeline::from_durations(0, &[4.0; 10])),
            container: Container::Ts,
            heights: Vec::new(),
            variants: Vec::new(),
            local: false,
            vod_gone: false,
            dirty: false,
            idx_dirty: false,
            dropping: false,
        };
        p.refresh_heights();
        p
    }

    fn req(id: &str, ranges: Vec<Range>, priority: Priority, seq: u64) -> FetchRequest {
        FetchRequest {
            id: id.into(),
            ranges,
            priority,
            reason: "test".into(),
            song_ids: vec![],
            song_ranges: vec![],
            height: None,
            process: false,
            created_at: String::new(),
            seq,
            force: false,
        }
    }

    fn legacy(id: &str, variant: usize, first: usize, last: usize, resolution: &str) -> store::LegacyPiece {
        store::LegacyPiece { id: id.into(), file: format!("clips/{id}/source.mp4"), variant, first, last, origin: 0.0, resolution: resolution.into(), key: None }
    }

    #[test]
    fn coverage_merges_states_and_skips_what_we_have() {
        let mut p = ps();
        assert_eq!(p.heights, vec![1080, 720, 144]);
        for i in [2, 3] {
            p.have.insert(i, Have { kind: HaveKind::Raw(1), bytes: 10 });
        }
        p.have.insert(4, Have { kind: HaveKind::Raw(0), bytes: 10 });
        // overlapping songs: one request, the second only adds what's missing
        p.idx.requests.push(req("a", vec![(9.0, 15.0)], Priority::User, 1));
        p.idx.requests.push(req("b", vec![(13.0, 25.0)], Priority::User, 2));
        p.idx.failed.insert(6, SegFailure { code: "NETWORK".into(), message: String::new(), attempts: 4, at: String::new() });
        assert_eq!(p.needed(&p.idx.requests[0]), Vec::<usize>::new());
        assert_eq!(p.needed(&p.idx.requests[1]), vec![5, 6]);
        assert!(p.finished(&p.idx.requests[0]));
        assert!(!p.finished(&p.idx.requests[1]));
        let c = p.coverage();
        let st: Vec<(f64, f64, CovState, Option<u32>)> = c.iter().map(|r| (r.start, r.end, r.state, r.height)).collect();
        assert_eq!(
            st,
            vec![
                (8.0, 16.0, CovState::Have, Some(720)),
                (16.0, 20.0, CovState::Have, Some(1080)),
                (20.0, 24.0, CovState::Queued, None),
                (24.0, 28.0, CovState::Failed, None),
            ]
        );
        // the failed segment is not retried by the picker
        assert_eq!(p.next_segment(false), Some((Priority::User, 2, 5, "b".to_string())));
        let s = p.request_status(&p.idx.requests[1]);
        assert_eq!((s.total_sec, s.have_sec, s.failed_sec, s.remaining_sec), (16.0, 8.0, 4.0, 4.0));
    }

    #[test]
    fn paused_only_forced_requests_run() {
        let mut p = ps();
        p.idx.requests.push(req("user", vec![(0.0, 3.0)], Priority::Now, 1));
        assert!(p.next_segment(true).is_none());
        let mut r = req("export", vec![(12.0, 13.0)], Priority::User, 2);
        r.force = true;
        p.idx.requests.push(r);
        assert_eq!(p.next_segment(true).map(|x| x.3), Some("export".to_string()));
        assert_eq!(p.next_segment(false).map(|x| x.3), Some("user".to_string()));
    }

    #[test]
    fn priority_then_order_and_neighbour_variant() {
        let mut p = ps();
        p.idx.requests.push(req("batch", vec![(0.0, 3.0)], Priority::Batch, 1));
        p.idx.requests.push(req("auto", vec![(12.0, 13.0)], Priority::Auto, 2));
        p.idx.requests.push(req("now", vec![(30.0, 31.0)], Priority::Now, 3));
        assert_eq!(p.next_segment(false).map(|x| x.2), Some(7));
        p.fetching.insert(7, (2, Default::default()));
        // then the automatic top-up (12..13 widened by the edge margin starts in segment 2)
        assert_eq!(p.next_segment(false).map(|x| (x.0, x.2)), Some((Priority::Auto, 2)));
        // a segment next to one being fetched at 144p continues at 144p
        let q = p.queued();
        assert_eq!(p.choose_variant(8, 0, &q), 2);
        assert_eq!(p.choose_variant(6, 0, &q), 2);
        assert_eq!(p.choose_variant(0, 1, &q), 1);
        // all at once gives the same answers (R-12)
        let all = p.choose_variants(&[0, 6, 8], 1, &q);
        assert_eq!((all[&0], all[&6], all[&8]), (1, 2, 2));
    }

    #[test]
    fn stretches_split_on_source_change() {
        let mut p = ps();
        p.idx.legacy.push(legacy("c", 1, 0, 2, ""));
        for i in 0..2 {
            p.have.insert(i, Have { kind: HaveKind::Legacy(0), bytes: 1 });
        }
        for i in 2..4 {
            p.have.insert(i, Have { kind: HaveKind::Raw(1), bytes: 1 });
        }
        p.have.insert(4, Have { kind: HaveKind::Raw(0), bytes: 1 });
        p.have.insert(7, Have { kind: HaveKind::Raw(0), bytes: 1 });
        let s = p.stretches();
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].0, s[0].1), (0, 5));
        assert_eq!(s[0].2.len(), 3);
        assert!(!s[0].2[0].partial());
        assert_eq!(s[0].2[1], media::Piece::Raw { variant: 1, first: 2, last: 4 });
        assert_eq!((s[1].0, s[1].1), (7, 8));
        assert_eq!(p.have_ranges(), vec![(0.0, 20.0), (28.0, 32.0)]);
    }

    #[test]
    fn overlapping_adopted_clips_are_used_in_part() {
        // R-1: clips A = 0..5 and B = 3..9 from @1 overlap on 3..5; scan
        // gives the overlap to B (the later one). A is used for 0..3 only and
        // must not be concatenated whole (that repeated 3..5 and shifted B)
        let mut p = ps();
        p.idx.legacy.push(legacy("a", 0, 0, 5, ""));
        p.idx.legacy.push(legacy("b", 0, 3, 9, ""));
        for i in 0..3 {
            p.have.insert(i, Have { kind: HaveKind::Legacy(0), bytes: 1 });
        }
        for i in 3..9 {
            p.have.insert(i, Have { kind: HaveKind::Legacy(1), bytes: 1 });
        }
        let s = p.stretches();
        assert_eq!(s.len(), 1);
        match &s[0].2[..] {
            [a @ media::Piece::Legacy { first: 0, last: 3, file_first: 0, file_last: 5, .. }, b @ media::Piece::Legacy { first: 3, last: 9, file_first: 3, file_last: 9, .. }] => {
                assert!(a.partial());
                assert!(!b.partial());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rendition_folders_follow_the_rendition_not_its_position() {
        // A-1: an index written before the rendition table: v0 / v1 are
        // positions 0 / 1 of the list the project stored then
        let old_list = vec![var(1920, 1080, 8_000_000), var(1280, 720, 4_000_000), var(2560, 1440, 12_000_000)];
        let mut idx = VideoIndex { version: 1, ..Default::default() };
        // an adopted 1440p clip recorded as rendition 3 of an even older list
        idx.legacy.push(legacy("c", 3, 6, 9, "2560x1440"));
        assert!(adopt_renditions(&mut idx, &[0, 1], &old_list));
        assert_eq!(idx.version, store::INDEX_VERSION);
        assert_eq!(idx.renditions[&0].height, 1080);
        assert_eq!(idx.renditions[&1].height, 720);
        // found by resolution, given its own folder number
        let f1440 = idx.legacy[0].variant;
        assert_eq!(idx.renditions[&f1440].height, 1440);
        assert_eq!(idx.legacy[0].key.as_deref(), Some("2560x1440@12000000"));
        assert!(!adopt_renditions(&mut idx, &[0, 1], &old_list), "only once");
        // the platform now lists them in another order, with a new one
        let mut p = ps();
        p.idx = idx;
        p.refresh_heights();
        p.variants = vec![var(854, 480, 1_500_000), var(1280, 720, 4_000_000), var(1920, 1080, 8_000_000), var(2560, 1440, 12_000_000)];
        assert_eq!(p.preferred_folder(Some(1080)), 0, "1080p stays in v0");
        assert_eq!(p.preferred_folder(Some(720)), 1);
        let f480 = p.preferred_folder(Some(480));
        assert!(f480 > f1440, "a new rendition gets a new folder, never a used one");
        assert_eq!(p.height(f480), Some(480));
        // a top-up next to the adopted clip continues its rendition
        for i in 6..9 {
            p.have.insert(i, Have { kind: HaveKind::Legacy(0), bytes: 1 });
        }
        p.idx.requests.push(req("topup", vec![(5.0, 24.0)], Priority::Auto, 1));
        let q = p.queued();
        assert_eq!(q.iter().copied().collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
        assert_eq!(p.choose_variant(1, 0, &q), f1440);
        // 1440p no longer offered: the request's own rendition
        p.variants.pop();
        assert_eq!(p.choose_variant(1, 0, &q), 0);
    }
}
