//! Time alignment between two recordings of the same audio (e.g. a fetched
//! clip vs. the analysis audio, which may come from a different rendition /
//! encoding) via FFT cross-correlation.

use realfft::RealFftPlanner;
use realfft::num_complex::Complex32;

/// Lag (seconds) such that `b[t] ≈ a[t + lag]`, searched within ±`max_lag` s.
/// Returns `(lag, normalised peak correlation)`.
pub fn find_lag(a: &[f32], b: &[f32], rate: u32, max_lag: f64) -> (f64, f32) {
    let n = (a.len() + b.len()).next_power_of_two();
    let mut planner = RealFftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n);
    let inv = planner.plan_fft_inverse(n);
    let spec = |x: &[f32]| {
        let mut buf = vec![0f32; n];
        buf[..x.len()].copy_from_slice(x);
        let mut s = fwd.make_output_vec();
        fwd.process(&mut buf, &mut s).expect("fft");
        s
    };
    let (sa, sb) = (spec(a), spec(b));
    let mut prod: Vec<Complex32> = sa.iter().zip(&sb).map(|(x, y)| x * y.conj()).collect();
    prod[0].im = 0.0;
    let last = prod.len() - 1;
    prod[last].im = 0.0;
    let mut cc = vec![0f32; n];
    inv.process(&mut prod, &mut cc).expect("ifft");
    // cc[k] = Σ a[t+k]·b[t]  (k ≥ 0 at index k, k < 0 wrapped at n+k)
    let max_k = (max_lag * rate as f64) as isize;
    let (mut best_k, mut best) = (0isize, f32::MIN);
    for k in -max_k..=max_k {
        let idx = if k >= 0 { k as usize } else { (n as isize + k) as usize };
        if idx < n && cc[idx] > best {
            best = cc[idx];
            best_k = k;
        }
    }
    let ea: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let eb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    (best_k as f64 / rate as f64, best / n as f32 / (ea * eb).max(1e-12))
}

#[cfg(test)]
mod tests {
    #[test]
    fn finds_shift() {
        let a: Vec<f32> = (0..20000).map(|i| ((i * 7919) % 1009) as f32 / 1009.0 - 0.5).collect();
        let b: Vec<f32> = a[123..123 + 8000].to_vec();
        let (lag, c) = super::find_lag(&a, &b, 1000, 1.0);
        assert!((lag - 0.123).abs() < 1e-9, "{lag}");
        assert!(c > 0.5);
    }
}
