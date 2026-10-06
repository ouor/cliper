//! Vocal / accompaniment separation with UVR MDX-Net ONNX models.
//!
//! Port of UVR's classic `demix` for MDX-Net: the stereo mix is cut into chunks
//! of `hop·(dim_t−1)` samples that overlap by `n_fft/2` on each side (`trim`),
//! each chunk's STFT (`[2ch × re/im, dim_f, dim_t]`, upper bins dropped) is
//! mapped by the model to the target stem's spectrum, inverted, and the trimmed
//! centres are concatenated; the result is scaled by the model's `compensate`.
//! Models run at 44.1 kHz.

use std::path::Path;

use ndarray::Array4;
use ort::session::Session;
use ort::value::TensorRef;
use rayon::prelude::*;
use realfft::num_complex::Complex32;
use serde::{Deserialize, Serialize};

use super::runtime::{Device, ModelError, open_session};
use crate::audio::stft::Stft;
use crate::progress::{Cancelled, CancellationToken, Progress, ProgressFn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSpec {
    pub name: String,
    pub file: String,
    pub n_fft: usize,
    pub hop: usize,
    pub dim_f: usize,
    pub dim_t: usize,
    pub compensate: f32,
    pub sample_rate: u32,
    /// stem the model outputs ("vocals" | "instrumental")
    pub primary: String,
}

impl ModelSpec {
    /// UVR-MDX-NET-Voc_FT (UVR model_data: dim_f 3072, dim_t 2^8, n_fft 7680, compensate 1.021).
    pub fn voc_ft() -> Self {
        Self {
            name: "UVR-MDX-NET-Voc_FT".into(),
            file: "UVR-MDX-NET-Voc_FT.onnx".into(),
            n_fft: 7680,
            hop: 1024,
            dim_f: 3072,
            dim_t: 256,
            compensate: 1.021,
            sample_rate: 44100,
            primary: "vocals".into(),
        }
    }

    pub fn chunk_size(&self) -> usize {
        self.hop * (self.dim_t - 1)
    }
    pub fn trim(&self) -> usize {
        self.n_fft / 2
    }
    pub fn gen_size(&self) -> usize {
        self.chunk_size() - 2 * self.trim()
    }
}

pub struct Separator {
    session: Session,
    pub spec: ModelSpec,
    stft: Stft,
    pub device: Device,
    pub batch: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum SeparateError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Cancelled(#[from] Cancelled),
}

impl From<ort::Error> for SeparateError {
    fn from(e: ort::Error) -> Self {
        SeparateError::Model(e.into())
    }
}

impl Separator {
    pub fn open(models_dir: &Path, spec: ModelSpec, prefer_cpu: bool) -> Result<Self, ModelError> {
        let (session, device) = open_session(&models_dir.join(&spec.file), prefer_cpu)?;
        let stft = Stft::new(spec.n_fft, spec.hop);
        let batch = if device == Device::Cuda { 4 } else { 1 };
        Ok(Self { session, spec, stft, device, batch })
    }

    /// Chunk → model input planes `[4][dim_f][dim_t]` (ch0 re, ch0 im, ch1 re, ch1 im).
    fn analyse(&self, left: &[f32], right: &[f32], out: &mut [f32]) {
        let (df, dt) = (self.spec.dim_f, self.spec.dim_t);
        for (ch, x) in [left, right].into_iter().enumerate() {
            let spec = self.stft.forward(x);
            debug_assert_eq!(spec.len(), dt);
            for (t, frame) in spec.iter().enumerate() {
                for f in 0..df {
                    let c = frame[f];
                    out[((ch * 2) * df + f) * dt + t] = c.re;
                    out[((ch * 2 + 1) * df + f) * dt + t] = c.im;
                }
            }
        }
    }

    /// Model output planes → stereo chunk (`chunk_size` samples per channel).
    fn synthesise(&self, planes: &[f32]) -> [Vec<f32>; 2] {
        let (df, dt) = (self.spec.dim_f, self.spec.dim_t);
        let bins = self.stft.bins();
        let n = self.spec.chunk_size();
        let ch = |c: usize| {
            let spec: Vec<Vec<Complex32>> = (0..dt)
                .map(|t| {
                    let mut fr = vec![Complex32::new(0.0, 0.0); bins];
                    for (f, slot) in fr.iter_mut().enumerate().take(df) {
                        *slot = Complex32::new(planes[((c * 2) * df + f) * dt + t], planes[((c * 2 + 1) * df + f) * dt + t]);
                    }
                    fr
                })
                .collect();
            self.stft.inverse(&spec, n)
        };
        [ch(0), ch(1)]
    }

    /// Separate the primary stem from an interleaved stereo mix at `spec.sample_rate`.
    /// Returns the stem, interleaved, same length as the input.
    pub fn separate(&mut self, mix: &[f32], progress: &ProgressFn, cancel: &CancellationToken) -> Result<Vec<f32>, SeparateError> {
        let n = mix.len() / 2;
        let (chunk, trim, gen_len) = (self.spec.chunk_size(), self.spec.trim(), self.spec.gen_size());
        let pad = gen_len - n % gen_len;
        // [trim zeros | mix | pad zeros | trim zeros], per channel
        let total = trim + n + pad + trim;
        let mut l = vec![0f32; total];
        let mut r = vec![0f32; total];
        for i in 0..n {
            l[trim + i] = mix[2 * i];
            r[trim + i] = mix[2 * i + 1];
        }
        let starts: Vec<usize> = (0..n + pad).step_by(gen_len).collect();
        let plane = 4 * self.spec.dim_f * self.spec.dim_t;
        let mut out_l = Vec::with_capacity(n + pad);
        let mut out_r = Vec::with_capacity(n + pad);

        for (bi, group) in starts.chunks(self.batch).enumerate() {
            if cancel.is_cancelled() {
                return Err(Cancelled.into());
            }
            let b = group.len();
            let mut input = vec![0f32; b * plane];
            input.par_chunks_mut(plane).zip(group.par_iter()).for_each(|(dst, &s)| {
                self.analyse(&l[s..s + chunk], &r[s..s + chunk], dst);
            });
            let arr = Array4::from_shape_vec((b, 4, self.spec.dim_f, self.spec.dim_t), input).expect("shape");
            let outputs = self.session.run(ort::inputs![TensorRef::from_array_view(&arr)?])?;
            let data = outputs[0].try_extract_tensor::<f32>()?.1.to_vec();
            drop(outputs);
            let waves: Vec<[Vec<f32>; 2]> = data.par_chunks(plane).map(|p| self.synthesise(p)).collect();
            for w in waves {
                out_l.extend_from_slice(&w[0][trim..chunk - trim]);
                out_r.extend_from_slice(&w[1][trim..chunk - trim]);
            }
            let done = ((bi + 1) * self.batch).min(starts.len());
            progress(Progress::new("separate", done as u64, starts.len() as u64));
        }
        let k = self.spec.compensate;
        let mut stem = Vec::with_capacity(n * 2);
        for i in 0..n {
            stem.push(out_l[i] * k);
            stem.push(out_r[i] * k);
        }
        Ok(stem)
    }
}
