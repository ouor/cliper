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

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RequestStatus {
    pub id: String,
    pub priority: Priority,
    pub reason: String,
    pub song_ids: Vec<String>,
    pub ranges: Vec<Range>,
    pub process: bool,
    /// seconds of video this request needs in total (whole segments)
    pub total_sec: f64,
    pub have_sec: f64,
    pub fetching_sec: f64,
    pub failed_sec: f64,
    /// still to download (queued + fetching)
    pub remaining_sec: f64,
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
    /// rendition index → height
    pub heights: Vec<u32>,
    /// the renditions (for picking one by height without a network call)
    pub variants: Vec<crate::hls::m3u8::Variant>,
    pub local: bool,
    pub vod_gone: bool,
    /// emit `video.changed`
    pub dirty: bool,
    /// write index.json
    pub idx_dirty: bool,
}

impl PState {
    pub fn height(&self, variant: usize) -> Option<u32> {
        self.heights.get(variant).copied()
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
    /// failed. Returns (priority, seq, segment).
    pub fn next_segment(&self) -> Option<(Priority, u64, usize)> {
        let mut reqs: Vec<&FetchRequest> = self.idx.requests.iter().collect();
        reqs.sort_by_key(|r| (r.priority, r.seq));
        for r in reqs {
            if let Some(i) = self.needed(r).into_iter().find(|i| !self.fetching.contains_key(i) && !self.idx.failed.contains_key(i)) {
                return Some((r.priority, r.seq, i));
            }
        }
        None
    }

    /// Rendition for segment `i`: continue the neighbouring video (so ranges
    /// stay one stream and join without a seam), else the request's height.
    pub fn choose_variant(&self, i: usize, preferred: usize) -> usize {
        let left = i.checked_sub(1).and_then(|j| self.variant_of(j));
        left.or_else(|| self.variant_of(i + 1)).filter(|&v| v < self.heights.len().max(1)).unwrap_or(preferred)
    }

    pub fn coverage(&self) -> Vec<CoverageRange> {
        let Some(tl) = &self.tl else { return Vec::new() };
        if self.local {
            return vec![CoverageRange { start: 0.0, end: tl.duration(), state: CovState::Have, height: self.heights.first().copied(), code: None }];
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
            song_ids: r.song_ids.clone(),
            ranges: r.ranges.clone(),
            process: r.process,
            total_sec,
            have_sec,
            fetching_sec,
            failed_sec,
            remaining_sec: (total_sec - have_sec - failed_sec).max(0.0),
            created_at: r.created_at.clone(),
        }
    }

    pub fn state(&self, project_id: &str, paused: bool, rate: Option<f64>) -> VideoState {
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
                    Some(media::Piece::Raw { last, .. }) => *last = i + 1,
                    Some(media::Piece::Legacy { .. }) => {}
                    _ => {}
                }
            } else {
                st.2.push(match h.kind {
                    HaveKind::Raw(v) => media::Piece::Raw { variant: v, first: i, last: i + 1 },
                    HaveKind::Legacy(n) => {
                        let l = &self.idx.legacy[n];
                        media::Piece::Legacy { file: self.dir.join(&l.file), variant: l.variant, first: l.first, last: l.last, origin: l.origin }
                    }
                });
            }
            prev = Some((i, h.kind));
        }
        out
    }
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

    fn ps() -> PState {
        PState {
            dir: PathBuf::new(),
            idx: VideoIndex::default(),
            have: BTreeMap::new(),
            fetching: HashMap::new(),
            tl: Some(Timeline::from_durations(0, &[4.0; 10])),
            container: Container::Ts,
            heights: vec![1080, 720, 144],
            variants: Vec::new(),
            local: false,
            vod_gone: false,
            dirty: false,
            idx_dirty: false,
        }
    }

    fn req(id: &str, ranges: Vec<Range>, priority: Priority, seq: u64) -> FetchRequest {
        FetchRequest { id: id.into(), ranges, priority, reason: "test".into(), song_ids: vec![], height: None, process: false, created_at: String::new(), seq }
    }

    #[test]
    fn coverage_merges_states_and_skips_what_we_have() {
        let mut p = ps();
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
        assert_eq!(p.next_segment(), Some((Priority::User, 2, 5)));
        let s = p.request_status(&p.idx.requests[1]);
        assert_eq!((s.total_sec, s.have_sec, s.failed_sec, s.remaining_sec), (16.0, 8.0, 4.0, 4.0));
    }

    #[test]
    fn priority_then_order_and_neighbour_variant() {
        let mut p = ps();
        p.idx.requests.push(req("batch", vec![(0.0, 3.0)], Priority::Batch, 1));
        p.idx.requests.push(req("auto", vec![(12.0, 13.0)], Priority::Auto, 2));
        p.idx.requests.push(req("now", vec![(30.0, 31.0)], Priority::Now, 3));
        assert_eq!(p.next_segment().map(|x| x.2), Some(7));
        p.fetching.insert(7, (2, Default::default()));
        // then the automatic top-up (12..13 widened by the edge margin starts in segment 2)
        assert_eq!(p.next_segment().map(|x| (x.0, x.2)), Some((Priority::Auto, 2)));
        // a segment next to one being fetched at 144p continues at 144p
        assert_eq!(p.choose_variant(8, 0), 2);
        assert_eq!(p.choose_variant(6, 0), 2);
        assert_eq!(p.choose_variant(0, 1), 1);
    }

    #[test]
    fn stretches_split_on_source_change() {
        let mut p = ps();
        p.idx.legacy.push(store::LegacyPiece { id: "c".into(), file: "clips/c/source.mp4".into(), variant: 1, first: 0, last: 2, origin: 0.0, resolution: String::new() });
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
        assert_eq!(s[0].2[1], media::Piece::Raw { variant: 1, first: 2, last: 4 });
        assert_eq!((s[1].0, s[1].1), (7, 8));
        assert_eq!(p.have_ranges(), vec![(0.0, 20.0), (28.0, 32.0)]);
    }
}
