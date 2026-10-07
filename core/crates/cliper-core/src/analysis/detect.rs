//! Song-section detection — 1:1 port of song-split-research `segment.py`
//! (method `"pann"`, v6 defaults incl. the adaptive full-VOD fallback).
//!
//! margin = music − speech → core = box(3 s) > hi, loose = box(1 s) > lo →
//! hysteresis (loose runs containing a core frame) → close gaps → min length →
//! segment gate (p75 sing ≥ x, median speech ≤ y) → if any blob is far longer
//! than a song (full, unedited VOD with BGM between songs), re-split blobs at
//! talk spikes (smoothed speech) and apply a strict sing gate.

use serde::{Deserialize, Serialize};

use crate::ml::panns::Probs;

pub type Seg = (f64, f64);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(default)]
pub struct Params {
    pub pann_smooth_sec: f64,
    pub pann_edge_sec: f64,
    /// core threshold on the margin
    pub pann_hi: f64,
    /// expansion threshold on the margin
    pub pann_lo: f64,
    /// margin = music − speech (false: music only)
    pub pann_use_speech: bool,
    /// gaps shorter than this merge into one song
    pub close_sec: f64,
    pub min_song_sec: f64,
    /// segment gate: drop if sing statistic < this (BGM-only)
    pub seg_sing_min: f64,
    /// segment gate: drop if median speech > this (BGM + talk)
    pub seg_speech_max: f64,
    /// "p75" | "median"
    pub seg_sing_stat: String,
    pub adaptive_resplit: bool,
    /// a segment longer than this is a blob (re-split target and trigger)
    pub long_blob_sec: f64,
    pub long_blob_n: usize,
    /// speech-cut: smoothed speech above this is talk
    pub sc_thr: f64,
    pub sc_smooth_sec: f64,
    pub sc_close_sec: f64,
    /// strict sing gate (p75) in full-VOD mode
    pub full_sing_min: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            pann_smooth_sec: 3.0,
            pann_edge_sec: 1.0,
            pann_hi: 0.2,
            pann_lo: 0.0,
            pann_use_speech: true,
            close_sec: 10.0,
            min_song_sec: 45.0,
            seg_sing_min: 0.015,
            seg_speech_max: 0.1,
            seg_sing_stat: "p75".into(),
            adaptive_resplit: true,
            long_blob_sec: 450.0,
            long_blob_n: 1,
            sc_thr: 0.05,
            sc_smooth_sec: 5.0,
            sc_close_sec: 5.0,
            full_sing_min: 0.03,
        }
    }
}

/// `scipy.ndimage.uniform_filter1d(x, size)` with the default `mode="reflect"`
/// (half-sample symmetric: d c b a | a b c d | d c b a) and `origin=0`.
pub fn uniform_filter1d(x: &[f64], size: usize) -> Vec<f64> {
    let n = x.len();
    if n == 0 || size <= 1 {
        return x.to_vec();
    }
    let at = |i: i64| -> f64 {
        // reflect index into [0, n)
        let n = n as i64;
        let period = 2 * n;
        let mut k = i.rem_euclid(period);
        if k >= n {
            k = period - 1 - k;
        }
        x[k as usize]
    };
    let left = (size / 2) as i64;
    let right = size as i64 - 1 - left;
    // running sum (scipy also accumulates incrementally)
    let mut out = Vec::with_capacity(n);
    let mut acc: f64 = (-left..=right).map(at).sum();
    out.push(acc / size as f64);
    for i in 1..n as i64 {
        acc += at(i + right) - at(i - 1 - left);
        out.push(acc / size as f64);
    }
    out
}

fn mask_to_segs(mask: &[bool], fps: f64) -> Vec<Seg> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &m) in mask.iter().enumerate() {
        match (m, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push((s as f64 / fps, i as f64 / fps));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s as f64 / fps, mask.len() as f64 / fps));
    }
    out
}

fn close_gaps(segs: &[Seg], max_gap: f64) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    for &(s, e) in segs {
        match out.last_mut() {
            Some(last) if s - last.1 <= max_gap => last.1 = e,
            _ => out.push((s, e)),
        }
    }
    out
}

/// Keep runs of `loose` that contain at least one `core` frame.
fn hysteresis(core: &[bool], loose: &[bool]) -> Vec<bool> {
    let n = loose.len();
    let mut out = vec![false; n];
    let mut i = 0;
    while i < n {
        if !loose[i] {
            i += 1;
            continue;
        }
        let s = i;
        while i < n && loose[i] {
            i += 1;
        }
        if core[s..i].iter().any(|&c| c) {
            out[s..i].iter_mut().for_each(|v| *v = true);
        }
    }
    out
}

/// `np.percentile(x, q)` with linear interpolation.
pub fn percentile(x: &[f64], q: f64) -> f64 {
    if x.is_empty() {
        return f64::NAN;
    }
    let mut v = x.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let pos = q / 100.0 * (v.len() - 1) as f64;
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    v[lo] + (v[hi] - v[lo]) * (pos - lo as f64)
}

pub fn median(x: &[f64]) -> f64 {
    percentile(x, 50.0)
}

fn idx(t: f64, fps: f64) -> usize {
    (t * fps) as usize
}

fn slice(v: &[f64], s: f64, e: f64, fps: f64) -> &[f64] {
    let (a, b) = (idx(s, fps).min(v.len()), idx(e, fps).min(v.len()));
    &v[a..b.max(a)]
}

fn f64v(v: &[f32]) -> Vec<f64> {
    v.iter().map(|&x| x as f64).collect()
}

fn win(sec: f64, fps: f64) -> usize {
    ((sec * fps) as usize).max(1)
}

/// Detect song sections. `probs` on a uniform grid (`probs.fps`).
pub fn segment(probs: &Probs, p: &Params) -> Vec<Seg> {
    let fps = probs.fps;
    let music = f64v(&probs.music);
    let speech = f64v(&probs.speech);
    let sing = f64v(&probs.sing);

    let cur: Vec<f64> = if p.pann_use_speech { music.iter().zip(&speech).map(|(m, s)| m - s).collect() } else { music.clone() };
    let core_c = uniform_filter1d(&cur, win(p.pann_smooth_sec, fps));
    let edge_c = uniform_filter1d(&cur, win(p.pann_edge_sec, fps));
    let core: Vec<bool> = core_c.iter().map(|&v| v > p.pann_hi).collect();
    let loose: Vec<bool> = edge_c.iter().map(|&v| v > p.pann_lo).collect();
    let mask = hysteresis(&core, &loose);
    let mut segs = mask_to_segs(&mask, fps);
    segs = close_gaps(&segs, p.close_sec);
    segs.retain(|(s, e)| e - s >= p.min_song_sec);

    // segment gate: BGM (sing↓) / BGM+talk (speech↑)
    if p.seg_sing_min > 0.0 || p.seg_speech_max < 1.0 {
        segs.retain(|&(s, e)| {
            let sg = slice(&sing, s, e, fps);
            let sv = if p.seg_sing_stat == "p75" { percentile(sg, 75.0) } else { median(sg) };
            sv >= p.seg_sing_min && median(slice(&speech, s, e, fps)) <= p.seg_speech_max
        });
    }

    if p.adaptive_resplit {
        segs = adaptive_resplit(segs, &speech, &sing, fps, p);
    }
    segs
}

fn adaptive_resplit(segs: Vec<Seg>, speech: &[f64], sing: &[f64], fps: f64, p: &Params) -> Vec<Seg> {
    let n_long = segs.iter().filter(|(s, e)| e - s > p.long_blob_sec).count();
    if n_long < p.long_blob_n {
        return segs;
    }
    let talk: Vec<bool> = uniform_filter1d(speech, win(p.sc_smooth_sec, fps)).iter().map(|&v| v > p.sc_thr).collect();
    let mut refined = Vec::new();
    for &(s, e) in &segs {
        if e - s > p.long_blob_sec {
            let (i0, i1) = (idx(s, fps), idx(e, fps).min(talk.len()));
            let not_talk: Vec<bool> = talk[i0..i1].iter().map(|t| !t).collect();
            let frag: Vec<Seg> = mask_to_segs(&not_talk, fps).into_iter().map(|(a, b)| (s + a, s + b)).collect();
            refined.extend(close_gaps(&frag, p.sc_close_sec).into_iter().filter(|(a, b)| b - a >= p.min_song_sec));
        } else {
            refined.push((s, e));
        }
    }
    refined
        .into_iter()
        .filter(|&(s, e)| {
            percentile(slice(sing, s, e, fps), 75.0) >= p.full_sing_min && median(slice(speech, s, e, fps)) <= p.seg_speech_max
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_filter_matches_scipy() {
        // scipy.ndimage.uniform_filter1d([1,2,3,4,5], 2) -> [1. , 1.5, 2.5, 3.5, 4.5]
        let r = uniform_filter1d(&[1.0, 2.0, 3.0, 4.0, 5.0], 2);
        assert_eq!(r, vec![1.0, 1.5, 2.5, 3.5, 4.5]);
        // size 3 -> [1.3333, 2, 3, 4, 4.6667]
        let r = uniform_filter1d(&[1.0, 2.0, 3.0, 4.0, 5.0], 3);
        assert!((r[0] - 4.0 / 3.0).abs() < 1e-12 && (r[4] - 14.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn percentile_linear() {
        assert_eq!(percentile(&[1.0, 2.0, 3.0, 4.0], 75.0), 3.25);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
    }
}
