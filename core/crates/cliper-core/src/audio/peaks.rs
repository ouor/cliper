//! Waveform peaks for the UI.
//!
//! File format `CLPEAKS1` (little endian):
//! ```text
//! magic    [u8; 8]  "CLPEAKS1"
//! rate     f64      peaks per second
//! count    u32      number of peaks
//! data     count × (min i16, max i16)   amplitude × 32767, mono (channel mean)
//! ```

use std::path::Path;

pub struct PeakBuilder {
    per_peak: usize,
    rate: f64,
    acc_min: f32,
    acc_max: f32,
    acc_n: usize,
    out: Vec<(i16, i16)>,
}

impl PeakBuilder {
    /// `sample_rate` of the mono samples fed in, `peaks_per_sec` output rate.
    pub fn new(sample_rate: u32, peaks_per_sec: u32) -> Self {
        let per_peak = (sample_rate / peaks_per_sec).max(1) as usize;
        Self { per_peak, rate: sample_rate as f64 / per_peak as f64, acc_min: f32::MAX, acc_max: f32::MIN, acc_n: 0, out: Vec::new() }
    }

    pub fn push(&mut self, mono: &[f32]) {
        for &x in mono {
            self.acc_min = self.acc_min.min(x);
            self.acc_max = self.acc_max.max(x);
            self.acc_n += 1;
            if self.acc_n == self.per_peak {
                self.flush();
            }
        }
    }

    /// Push interleaved stereo (channel mean).
    pub fn push_stereo(&mut self, inter: &[f32]) {
        let mono: Vec<f32> = inter.chunks_exact(2).map(|c| 0.5 * (c[0] + c[1])).collect();
        self.push(&mono);
    }

    fn flush(&mut self) {
        let q = |v: f32| (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        self.out.push((q(self.acc_min), q(self.acc_max)));
        self.acc_min = f32::MAX;
        self.acc_max = f32::MIN;
        self.acc_n = 0;
    }

    /// Peaks so far (the partial peak in progress is left out).
    pub fn snapshot(&self) -> Peaks {
        Peaks { rate: self.rate, data: self.out.clone() }
    }

    pub fn finish(mut self) -> Peaks {
        if self.acc_n > 0 {
            self.flush();
        }
        Peaks { rate: self.rate, data: self.out }
    }
}

pub struct Peaks {
    pub rate: f64,
    pub data: Vec<(i16, i16)>,
}

impl Peaks {
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let mut b = Vec::with_capacity(20 + self.data.len() * 4);
        b.extend_from_slice(b"CLPEAKS1");
        b.extend_from_slice(&self.rate.to_le_bytes());
        b.extend_from_slice(&(self.data.len() as u32).to_le_bytes());
        for (mn, mx) in &self.data {
            b.extend_from_slice(&mn.to_le_bytes());
            b.extend_from_slice(&mx.to_le_bytes());
        }
        crate::project::store::write_atomic(path, &b)
    }
}

/// Peaks of interleaved stereo samples.
pub fn of_stereo(samples: &[f32], sample_rate: u32, peaks_per_sec: u32) -> Peaks {
    let mut b = PeakBuilder::new(sample_rate, peaks_per_sec);
    b.push_stereo(samples);
    b.finish()
}
