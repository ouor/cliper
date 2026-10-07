//! Audio render: the clip's own audio with selected vocal regions removed.
//!
//! `out(t) = orig(t) − (1 − g(t)) · vocals(t)`, where `g` is 1 everywhere except
//! inside mute regions: raised-cosine fades down to the region's floor gain
//! (0 = voice fully removed, or `10^(gain_db/20)` when only attenuating), then
//! back up. With `g = 1` the output is bit-identical to the original; with
//! `g = 0` it is the accompaniment.

use serde::{Deserialize, Serialize};

/// A mute region in clip time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct MuteSpan {
    pub start: f64,
    pub end: f64,
    pub fade_in: f64,
    pub fade_out: f64,
    /// None = remove completely; Some(db) = attenuate the voice by `db` (≤ 0)
    #[serde(default)]
    pub gain_db: Option<f64>,
}

impl MuteSpan {
    pub fn floor(&self) -> f64 {
        match self.gain_db {
            Some(db) if db < 0.0 => 10f64.powf(db / 20.0),
            Some(_) => 1.0,
            None => 0.0,
        }
    }
}

/// Vocal gain at time `t` (min over overlapping mutes).
pub fn gain_at(t: f64, mutes: &[MuteSpan]) -> f32 {
    let mut g = 1.0f64;
    for m in mutes {
        if t < m.start || t > m.end {
            continue;
        }
        let len = m.end - m.start;
        let (fi, fo) = (m.fade_in.clamp(0.0, len / 2.0), m.fade_out.clamp(0.0, len / 2.0));
        // shape: 1 → 0 → 1 (raised cosine fades inside the region)
        let shape = if fi > 0.0 && t < m.start + fi {
            0.5 * (1.0 + (std::f64::consts::PI * (t - m.start) / fi).cos())
        } else if fo > 0.0 && t > m.end - fo {
            0.5 * (1.0 - (std::f64::consts::PI * (t - (m.end - fo)) / fo).cos())
        } else {
            0.0
        };
        let floor = m.floor();
        g = g.min(floor + (1.0 - floor) * shape);
    }
    g as f32
}

/// Raised-cosine ramp 0→1 over `n` samples, value at `k`.
fn ramp(k: usize, n: usize) -> f32 {
    if n == 0 || k >= n {
        return 1.0;
    }
    (0.5 - 0.5 * (std::f64::consts::PI * k as f64 / n as f64).cos()) as f32
}

/// Render interleaved stereo `[in_t, out_t)` (clip seconds) at `rate`.
/// `fade_in` / `fade_out` (s) shape the very start / end (min 5 ms against clicks).
pub fn render(orig: &[f32], vocals: &[f32], rate: u32, in_t: f64, out_t: f64, mutes: &[MuteSpan], fade_in: f64, fade_out: f64) -> Vec<f32> {
    let frames = orig.len().min(vocals.len()) / 2;
    let a = ((in_t.max(0.0) * rate as f64).round() as usize).min(frames);
    let b = ((out_t * rate as f64).round() as usize).clamp(a, frames);
    let n = b - a;
    let fi = ((fade_in.max(0.005) * rate as f64) as usize).min(n / 2);
    let fo = ((fade_out.max(0.005) * rate as f64) as usize).min(n / 2);
    let mut out = Vec::with_capacity(n * 2);
    // only mutes touching the range matter
    let mutes: Vec<MuteSpan> = mutes.iter().copied().filter(|m| m.end > in_t && m.start < out_t && m.floor() < 1.0).collect();
    for k in 0..n {
        let i = a + k;
        let g = if mutes.is_empty() { 1.0 } else { gain_at(i as f64 / rate as f64, &mutes) };
        let e = ramp(k, fi) * ramp(n - 1 - k, fo);
        for c in 0..2 {
            let o = orig[2 * i + c];
            let v = if g >= 1.0 { o } else { o - (1.0 - g) * vocals[2 * i + c] };
            out.push(v * e);
        }
    }
    out
}

/// The clip audio with mutes applied over its whole length (no fades) — for
/// "processed" stems handed to an editor / DAW.
pub fn render_full(orig: &[f32], vocals: &[f32], rate: u32, mutes: &[MuteSpan]) -> Vec<f32> {
    let frames = orig.len().min(vocals.len()) / 2;
    let mut out = orig[..frames * 2].to_vec();
    let mutes: Vec<MuteSpan> = mutes.iter().copied().filter(|m| m.floor() < 1.0).collect();
    for m in &mutes {
        let a = ((m.start.max(0.0) * rate as f64) as usize).min(frames);
        let b = ((m.end * rate as f64).ceil() as usize).min(frames);
        for i in a..b {
            let g = gain_at(i as f64 / rate as f64, &mutes);
            for c in 0..2 {
                out[2 * i + c] = orig[2 * i + c] - (1.0 - g) * vocals[2 * i + c];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope() {
        let m = [MuteSpan { start: 1.0, end: 3.0, fade_in: 0.5, fade_out: 0.5, gain_db: None }];
        assert_eq!(gain_at(0.5, &m), 1.0);
        assert!((gain_at(1.0, &m) - 1.0).abs() < 1e-6);
        assert!((gain_at(1.25, &m) - 0.5).abs() < 1e-6);
        assert_eq!(gain_at(2.0, &m), 0.0);
        assert!((gain_at(2.75, &m) - 0.5).abs() < 1e-6);
        assert!((gain_at(3.0, &m) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn attenuation_floor() {
        let m = [MuteSpan { start: 1.0, end: 3.0, fade_in: 0.0, fade_out: 0.0, gain_db: Some(-20.0) }];
        assert!((gain_at(2.0, &m) - 0.1).abs() < 1e-6);
        assert_eq!(gain_at(4.0, &m), 1.0);
    }

    #[test]
    fn untouched_is_identity_inside_fades() {
        let orig: Vec<f32> = (0..2000).map(|i| (i as f32 * 0.01).sin()).collect();
        let voc = vec![0.3f32; 2000];
        let out = render(&orig, &voc, 100, 0.0, 10.0, &[], 0.0, 0.0);
        // with minimal (5 ms → 0 samples at 100 Hz) fades the samples are untouched
        assert_eq!(out, orig);
        let full = render_full(&orig, &voc, 100, &[]);
        assert_eq!(full, orig);
    }
}
