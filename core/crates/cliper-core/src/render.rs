//! Audio render: the clip's own audio with selected vocal regions removed.
//!
//! `out(t) = orig(t) − (1 − g(t)) · vocals(t)`, where `g` is 1 everywhere except
//! inside mute regions (raised-cosine fades, then 0). With `g = 1` the output is
//! bit-identical to the original; with `g = 0` it is the accompaniment.

use serde::{Deserialize, Serialize};

/// A mute region in clip time.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct MuteSpan {
    pub start: f64,
    pub end: f64,
    pub fade_in: f64,
    pub fade_out: f64,
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
        let v = if fi > 0.0 && t < m.start + fi {
            0.5 * (1.0 + (std::f64::consts::PI * (t - m.start) / fi).cos())
        } else if fo > 0.0 && t > m.end - fo {
            0.5 * (1.0 - (std::f64::consts::PI * (t - (m.end - fo)) / fo).cos())
        } else {
            0.0
        };
        g = g.min(v);
    }
    g as f32
}

/// Render interleaved stereo `[in_t, out_t)` (clip seconds) at `rate`.
/// `edge_fade` (s) fades the very start/end to avoid clicks at cut points.
pub fn render(orig: &[f32], vocals: &[f32], rate: u32, in_t: f64, out_t: f64, mutes: &[MuteSpan], edge_fade: f64) -> Vec<f32> {
    let frames = orig.len().min(vocals.len()) / 2;
    let a = ((in_t.max(0.0) * rate as f64).round() as usize).min(frames);
    let b = ((out_t * rate as f64).round() as usize).clamp(a, frames);
    let n = b - a;
    let ef = (edge_fade * rate as f64) as usize;
    let mut out = Vec::with_capacity(n * 2);
    // only mutes touching the range matter
    let mutes: Vec<MuteSpan> = mutes.iter().copied().filter(|m| m.end > in_t && m.start < out_t).collect();
    for k in 0..n {
        let i = a + k;
        let g = if mutes.is_empty() { 1.0 } else { gain_at(i as f64 / rate as f64, &mutes) };
        let mut e = 1.0f32;
        if ef > 0 {
            if k < ef {
                e = k as f32 / ef as f32;
            } else if n - k <= ef {
                e = (n - k) as f32 / ef as f32;
            }
        }
        for c in 0..2 {
            let o = orig[2 * i + c];
            let v = if g >= 1.0 { o } else { o - (1.0 - g) * vocals[2 * i + c] };
            out.push(v * e);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope() {
        let m = [MuteSpan { start: 1.0, end: 3.0, fade_in: 0.5, fade_out: 0.5 }];
        assert_eq!(gain_at(0.5, &m), 1.0);
        assert!((gain_at(1.0, &m) - 1.0).abs() < 1e-6);
        assert!((gain_at(1.25, &m) - 0.5).abs() < 1e-6);
        assert_eq!(gain_at(2.0, &m), 0.0);
        assert!((gain_at(2.75, &m) - 0.5).abs() < 1e-6);
        assert!((gain_at(3.0, &m) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn untouched_is_identity() {
        let orig: Vec<f32> = (0..2000).map(|i| (i as f32 * 0.01).sin()).collect();
        let voc = vec![0.3f32; 2000];
        let out = render(&orig, &voc, 100, 0.0, 10.0, &[], 0.0);
        assert_eq!(out, orig);
    }
}
