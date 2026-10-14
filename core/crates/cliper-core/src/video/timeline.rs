//! The VOD's HLS segments placed on the analysis timeline.
//!
//! Segment `i` (playlist index) spans `[bound(i), bound(i+1))` VOD seconds,
//! where the bounds come from the analysis audio's frame index (exact sample
//! counts per segment), so coverage is expressed in the same time base as
//! songs and mutes. All video variants share this segmentation (checked when
//! a variant is used).

use std::collections::BTreeSet;

use crate::hls::audio_extract::SegIndex;
use crate::ranges::Range;

/// Extra seconds a requested range is widened by before it is rounded out to
/// whole segments: a video variant's audio/video can sit a few ms off the
/// analysis audio's frame-count bounds, so an edge exactly on a bound must
/// still pull the neighbouring segment rather than come up a frame short.
pub const EDGE_EPS: f64 = 0.1;

#[derive(Debug, Clone, PartialEq)]
pub struct Timeline {
    /// playlist index of the first segment
    pub first: usize,
    /// `bounds[k]` = VOD start of segment `first + k`; `len = n + 1`
    pub bounds: Vec<f64>,
}

impl Timeline {
    pub fn from_audio(idx: &SegIndex) -> Self {
        let sr = idx.sample_rate.max(1) as f64;
        let spf = idx.samples_per_frame as f64;
        let mut bounds = Vec::with_capacity(idx.frames.len() + 1);
        let mut acc = 0u64;
        bounds.push(0.0);
        for &f in &idx.frames {
            acc += f as u64;
            bounds.push(acc as f64 * spf / sr);
        }
        Self { first: idx.first_segment, bounds }
    }

    /// A timeline from EXTINF durations (tests; sources without an index).
    pub fn from_durations(first: usize, durations: &[f64]) -> Self {
        let mut bounds = Vec::with_capacity(durations.len() + 1);
        let mut t = 0.0;
        bounds.push(0.0);
        for d in durations {
            t += d;
            bounds.push(t);
        }
        Self { first, bounds }
    }

    pub fn len(&self) -> usize {
        self.bounds.len().saturating_sub(1)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// One past the last segment index.
    pub fn end_index(&self) -> usize {
        self.first + self.len()
    }
    pub fn duration(&self) -> f64 {
        self.bounds.last().copied().unwrap_or(0.0)
    }
    pub fn contains(&self, i: usize) -> bool {
        i >= self.first && i < self.end_index()
    }
    pub fn start(&self, i: usize) -> f64 {
        self.bounds[(i - self.first).min(self.len())]
    }
    pub fn end(&self, i: usize) -> f64 {
        self.bounds[(i + 1 - self.first).min(self.len())]
    }
    pub fn dur(&self, i: usize) -> f64 {
        self.end(i) - self.start(i)
    }

    /// Segment holding VOD time `t`.
    pub fn seg_at(&self, t: f64) -> Option<usize> {
        if self.is_empty() || t < 0.0 || t >= self.duration() {
            return None;
        }
        // last bound <= t
        let k = self.bounds.partition_point(|&b| b <= t).saturating_sub(1);
        Some(self.first + k.min(self.len() - 1))
    }

    /// Segments needed to cover `[a, b)`, rounded outward (widened by `eps`).
    pub fn segments_for(&self, a: f64, b: f64, eps: f64) -> std::ops::Range<usize> {
        if self.is_empty() || b <= a {
            return self.first..self.first;
        }
        let (a, b) = ((a - eps).max(0.0), (b + eps).min(self.duration()));
        if b <= a {
            return self.first..self.first;
        }
        let lo = self.bounds.partition_point(|&x| x <= a).saturating_sub(1);
        let hi = self.bounds.partition_point(|&x| x < b).min(self.len());
        (self.first + lo)..(self.first + hi.max(lo + 1))
    }

    /// Every segment touching any of `ranges` (rounded outward).
    pub fn segments_in(&self, ranges: &[Range], eps: f64) -> BTreeSet<usize> {
        let mut out = BTreeSet::new();
        for &(a, b) in ranges {
            out.extend(self.segments_for(a, b, eps));
        }
        out
    }

    /// VOD ranges of a set of segments (consecutive ones merged).
    pub fn ranges_of<I: IntoIterator<Item = usize>>(&self, segs: I) -> Vec<Range> {
        let mut out: Vec<Range> = Vec::new();
        let mut prev: Option<usize> = None;
        for i in segs {
            if !self.contains(i) {
                continue;
            }
            match (prev, out.last_mut()) {
                (Some(p), Some(last)) if i == p + 1 => last.1 = self.end(i),
                _ => out.push((self.start(i), self.end(i))),
            }
            prev = Some(i);
        }
        out
    }

    /// Seconds covered by these segments.
    pub fn seconds<I: IntoIterator<Item = usize>>(&self, segs: I) -> f64 {
        segs.into_iter().filter(|&i| self.contains(i)).map(|i| self.dur(i)).fold(0.0, |a, b| a + b)
    }
}

/// Group sorted segment indices into runs of consecutive indices `[first, last)`.
pub fn runs(segs: &[usize]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    for &i in segs {
        match out.last_mut() {
            Some(r) if r.1 == i => r.1 = i + 1,
            _ => out.push((i, i + 1)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tl() -> Timeline {
        // 5 segments of 4 s starting at playlist index 10
        Timeline::from_durations(10, &[4.0; 5])
    }

    #[test]
    fn rounding_out_to_segments() {
        let t = tl();
        assert_eq!(t.duration(), 20.0);
        assert_eq!(t.segments_for(5.0, 9.0, 0.0), 11..13);
        // exactly on a bound: the segment ending there is not needed without eps …
        assert_eq!(t.segments_for(4.0, 8.0, 0.0), 11..12);
        // … but is with the safety margin (a few ms of alignment slack)
        assert_eq!(t.segments_for(4.0, 8.0, EDGE_EPS), 10..13);
        assert_eq!(t.segments_for(-5.0, 100.0, 0.0), 10..15);
        assert_eq!(t.segments_for(19.9, 30.0, 0.0), 14..15);
        assert!(t.segments_for(30.0, 40.0, 0.0).is_empty());
        assert_eq!(t.seg_at(0.0), Some(10));
        assert_eq!(t.seg_at(7.99), Some(11));
        assert_eq!(t.seg_at(8.0), Some(12));
        assert_eq!(t.seg_at(20.0), None);
    }

    #[test]
    fn ranges_and_runs() {
        let t = tl();
        assert_eq!(t.ranges_of([10, 11, 13]), vec![(0.0, 8.0), (12.0, 16.0)]);
        assert_eq!(runs(&[1, 2, 3, 7, 9, 10]), vec![(1, 4), (7, 8), (9, 11)]);
        let s = t.segments_in(&[(1.0, 2.0), (13.0, 14.0)], 0.0);
        assert_eq!(s.into_iter().collect::<Vec<_>>(), vec![10, 13]);
        assert_eq!(t.seconds([10, 11, 99]), 8.0);
    }

    #[test]
    fn from_audio_index_uses_frame_counts() {
        let idx = SegIndex { first_segment: 0, frames: vec![375, 376, 375], durations: vec![8.0; 3], samples_per_frame: 1024, sample_rate: 48000 };
        let t = Timeline::from_audio(&idx);
        let want = [0.0, 8.0, 751.0 * 1024.0 / 48000.0, 1126.0 * 1024.0 / 48000.0];
        assert!(t.bounds.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-9), "{:?}", t.bounds);
    }
}
