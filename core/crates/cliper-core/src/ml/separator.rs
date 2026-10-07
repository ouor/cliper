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
    /// the GPU ran out of memory even at batch size 1
    #[error("GPU out of memory")]
    OutOfMemory,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<ort::Error> for SeparateError {
    fn from(e: ort::Error) -> Self {
        SeparateError::Model(e.into())
    }
}

/// Does this ONNX Runtime error mean the GPU ran out of memory?
pub fn is_oom(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    ["out of memory", "failed to allocate", "alloc_failed", "cudaerrormemoryallocation", "bfcarena"].iter().any(|k| m.contains(k))
}

/// Free GPU memory (MiB) on the first GPU, from nvidia-smi.
fn free_vram_mb() -> Option<u64> {
    let mut c = std::process::Command::new("nvidia-smi");
    c.args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let out = c.output().ok().filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

impl Separator {
    /// Open the model (CUDA unless `prefer_cpu`). On the GPU the batch size
    /// follows the free memory (batch 4 peaks near 10 GB, 1 near 2.5 GB); an
    /// out-of-memory run halves it automatically.
    pub fn open(models_dir: &Path, spec: ModelSpec, prefer_cpu: bool) -> Result<Self, ModelError> {
        let (session, device) = open_session(&models_dir.join(&spec.file), prefer_cpu)?;
        let stft = Stft::new(spec.n_fft, spec.hop);
        let batch = match device {
            Device::Cpu => 1,
            Device::Cuda => match free_vram_mb() {
                Some(mb) if mb >= 11_000 => 4,
                Some(mb) if mb >= 6_000 => 2,
                Some(_) => 1,
                None => 2,
            },
        };
        tracing::info!("separator on {device:?}, batch {batch}");
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

    /// Run the model on `b` input plane sets. On GPU out-of-memory the batch is
    /// split in halves (and the separator's batch size lowered for the rest of
    /// the run); at batch 1 it reports [`SeparateError::OutOfMemory`].
    fn run_group(&mut self, input: Vec<f32>, b: usize) -> Result<Vec<f32>, SeparateError> {
        let (df, dt) = (self.spec.dim_f, self.spec.dim_t);
        let plane = 4 * df * dt;
        let arr = Array4::from_shape_vec((b, 4, df, dt), input).expect("shape");
        let oom = {
            let res = self.session.run(ort::inputs![TensorRef::from_array_view(&arr)?]);
            match res {
                Ok(outputs) => {
                    let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
                    let want = [b as i64, 4, df as i64, dt as i64];
                    if shape[..] != want[..] {
                        return Err(ModelError::Ort(format!("unexpected model output shape {:?} (expected {want:?})", &shape[..])).into());
                    }
                    return Ok(data.to_vec());
                }
                Err(e) if is_oom(&e.to_string()) => e.to_string(),
                Err(e) => return Err(e.into()),
            }
        };
        if b == 1 {
            tracing::warn!("separation out of GPU memory at batch 1: {oom}");
            return Err(SeparateError::OutOfMemory);
        }
        let half = b / 2;
        self.batch = half.max(1);
        tracing::warn!("separation out of GPU memory at batch {b}; retrying with {half}");
        let input = arr.into_raw_vec_and_offset().0;
        let (x, y) = input.split_at(half * plane);
        let mut out = self.run_group(x.to_vec(), half)?;
        out.extend(self.run_group(y.to_vec(), b - half)?);
        Ok(out)
    }

    /// Separate the primary stem of a stereo stream at `spec.sample_rate`,
    /// in constant memory: `read` fills interleaved stereo samples (0 = end),
    /// `write` receives the interleaved stem, exactly as many frames as were
    /// read. `total_frames` (if known) drives the progress total.
    pub fn separate_stream(
        &mut self,
        read: &mut dyn FnMut(&mut [f32]) -> std::io::Result<usize>,
        write: &mut dyn FnMut(&[f32]) -> std::io::Result<()>,
        total_frames: Option<u64>,
        progress: &ProgressFn,
        cancel: &CancellationToken,
    ) -> Result<u64, SeparateError> {
        let (chunk, trim, gen_len) = (self.spec.chunk_size(), self.spec.trim(), self.spec.gen_size());
        let plane = 4 * self.spec.dim_f * self.spec.dim_t;
        let k = self.spec.compensate;
        // padded signal per channel, from the current window start on:
        // [trim zeros | mix | pad zeros | trim zeros] (UVR classic demix)
        let mut l: Vec<f32> = vec![0.0; trim];
        let mut r: Vec<f32> = vec![0.0; trim];
        let mut ibuf = vec![0f32; 1 << 16];
        let (mut read_frames, mut written, mut eof) = (0u64, 0u64, false);
        let total_windows = total_frames.map(|n| n / gen_len as u64 + 1).unwrap_or(0);
        let mut windows = 0u64;
        loop {
            // buffer enough for a full batch (or everything that's left)
            let need = (self.batch.max(1) - 1) * gen_len + chunk;
            while !eof && l.len() < need {
                let n = read(&mut ibuf)?;
                if n == 0 {
                    eof = true;
                    let pad = gen_len - (read_frames as usize) % gen_len;
                    l.resize(l.len() + pad + trim, 0.0);
                    r.resize(r.len() + pad + trim, 0.0);
                } else {
                    for fr in ibuf[..n].chunks_exact(2) {
                        l.push(fr[0]);
                        r.push(fr[1]);
                    }
                    read_frames += (n / 2) as u64;
                }
            }
            let avail = if l.len() >= chunk { (l.len() - chunk) / gen_len + 1 } else { 0 };
            let b = avail.min(self.batch.max(1));
            if b == 0 {
                break;
            }
            if cancel.is_cancelled() {
                return Err(Cancelled.into());
            }
            let mut input = vec![0f32; b * plane];
            input.par_chunks_mut(plane).enumerate().for_each(|(w, dst)| {
                let s = w * gen_len;
                self.analyse(&l[s..s + chunk], &r[s..s + chunk], dst);
            });
            let data = self.run_group(input, b)?;
            let waves: Vec<[Vec<f32>; 2]> = data.par_chunks(plane).map(|p| self.synthesise(p)).collect();
            for w in waves {
                // before the end every window lies inside the real signal; at the
                // end, stop at the input length (the rest is padding)
                let take = if eof { (read_frames - written).min(gen_len as u64) as usize } else { gen_len };
                let mut out = Vec::with_capacity(take * 2);
                for i in 0..take {
                    out.push(w[0][trim + i] * k);
                    out.push(w[1][trim + i] * k);
                }
                write(&out)?;
                written += take as u64;
            }
            l.drain(..b * gen_len);
            r.drain(..b * gen_len);
            windows += b as u64;
            progress(Progress::new("separate", windows, total_windows.max(windows)));
            if eof && written >= read_frames {
                break;
            }
        }
        Ok(written)
    }

    /// Separate the primary stem from an interleaved stereo mix at `spec.sample_rate`.
    /// Returns the stem, interleaved, same length as the input.
    pub fn separate(&mut self, mix: &[f32], progress: &ProgressFn, cancel: &CancellationToken) -> Result<Vec<f32>, SeparateError> {
        let mut pos = 0usize;
        let mut out = Vec::with_capacity(mix.len());
        self.separate_stream(
            &mut |b: &mut [f32]| {
                let n = b.len().min(mix.len() - pos);
                b[..n].copy_from_slice(&mix[pos..pos + n]);
                pos += n;
                Ok(n)
            },
            &mut |x: &[f32]| {
                out.extend_from_slice(x);
                Ok(())
            },
            Some((mix.len() / 2) as u64),
            progress,
            cancel,
        )?;
        Ok(out)
    }
}
