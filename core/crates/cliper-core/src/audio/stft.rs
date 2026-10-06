//! STFT / iSTFT matching `torch.stft(..., center=True, pad_mode="reflect",
//! window=hann_window(n_fft, periodic=True), onesided=True)` and `torch.istft`
//! (window-squared normalised overlap-add, centre trimmed).

use std::sync::Arc;

use realfft::num_complex::Complex32;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

pub struct Stft {
    pub n_fft: usize,
    pub hop: usize,
    window: Vec<f32>,
    fwd: Arc<dyn RealToComplex<f32>>,
    inv: Arc<dyn ComplexToReal<f32>>,
}

impl Stft {
    pub fn new(n_fft: usize, hop: usize) -> Self {
        let window = (0..n_fft)
            .map(|i| {
                let x = std::f64::consts::PI * i as f64 / n_fft as f64;
                (x.sin() * x.sin()) as f32 // 0.5 - 0.5 cos(2πi/N), periodic
            })
            .collect();
        let mut planner = RealFftPlanner::<f32>::new();
        Self { n_fft, hop, window, fwd: planner.plan_fft_forward(n_fft), inv: planner.plan_fft_inverse(n_fft) }
    }

    pub fn bins(&self) -> usize {
        self.n_fft / 2 + 1
    }

    /// Number of frames for a signal of `len` samples (center=True).
    pub fn frames(&self, len: usize) -> usize {
        1 + len / self.hop
    }

    /// Spectrum as `[frame][bin]` complex values.
    pub fn forward(&self, x: &[f32]) -> Vec<Vec<Complex32>> {
        let pad = self.n_fft / 2;
        let n = x.len();
        // reflect padding (torch "reflect" = numpy "reflect": excludes the edge sample)
        let at = |i: isize| -> f32 {
            let n = n as isize;
            let mut k = i;
            if n == 1 {
                return x[0];
            }
            let period = 2 * (n - 1);
            k = k.rem_euclid(period);
            if k >= n {
                k = period - k;
            }
            x[k as usize]
        };
        let frames = self.frames(n);
        let mut scratch = self.fwd.make_scratch_vec();
        let mut buf = self.fwd.make_input_vec();
        let mut out = Vec::with_capacity(frames);
        for f in 0..frames {
            let start = (f * self.hop) as isize - pad as isize;
            for (i, b) in buf.iter_mut().enumerate() {
                *b = at(start + i as isize) * self.window[i];
            }
            let mut spec = self.fwd.make_output_vec();
            self.fwd.process_with_scratch(&mut buf, &mut spec, &mut scratch).expect("fft");
            out.push(spec);
        }
        out
    }

    /// Inverse of [`forward`] for an output of `len` samples.
    pub fn inverse(&self, spec: &[Vec<Complex32>], len: usize) -> Vec<f32> {
        let pad = self.n_fft / 2;
        let total = self.n_fft + self.hop * (spec.len().saturating_sub(1));
        let mut y = vec![0f32; total];
        let mut env = vec![0f32; total];
        let mut scratch = self.inv.make_scratch_vec();
        let mut frame = self.inv.make_output_vec();
        let scale = 1.0 / self.n_fft as f32;
        for (f, s) in spec.iter().enumerate() {
            let mut s = s.clone();
            // DC / Nyquist must be real for a real inverse
            s[0].im = 0.0;
            let last = s.len() - 1;
            s[last].im = 0.0;
            self.inv.process_with_scratch(&mut s, &mut frame, &mut scratch).expect("ifft");
            let off = f * self.hop;
            for i in 0..self.n_fft {
                let w = self.window[i];
                y[off + i] += frame[i] * scale * w;
                env[off + i] += w * w;
            }
        }
        (0..len)
            .map(|i| {
                let k = i + pad;
                if k < total && env[k] > 1e-11 { y[k] / env[k] } else { 0.0 }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let st = Stft::new(1024, 256);
        let x: Vec<f32> = (0..8192).map(|i| ((i as f32) * 0.013).sin() * 0.5 + ((i * 7919 % 101) as f32 / 101.0 - 0.5) * 0.1).collect();
        let y = st.inverse(&st.forward(&x), x.len());
        let err = x.iter().zip(&y).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        assert!(err < 1e-5, "max err {err}");
    }
}
