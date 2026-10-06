//! PANNs sound-event tagger → frame-wise music / speech / sing probabilities.
//!
//! Port of song-split-research `dl_tagger.py`: 32 kHz mono is fed to the
//! `Cnn14_DecisionLevelMax` ONNX graph (log-mel inside) in 60 s chunks; per
//! frame, each label group takes the max over its AudioSet classes; the frame
//! sequence is then linearly resampled onto a 10 fps grid spanning the audio.
//! Streaming: chunks are inferred as they are decoded, so a 4 h VOD never sits
//! in memory as raw PCM.

use std::path::Path;

use ort::session::Session;
use ort::value::TensorRef;
use serde::{Deserialize, Serialize};

use super::runtime::{Device, ModelError, open_session};
use crate::progress::{Cancelled, CancellationToken, Progress, ProgressFn};

pub const SAMPLE_RATE: u32 = 32000;
pub const FPS: f64 = 10.0;
const CHUNK_SEC: usize = 60;

#[derive(Debug, Clone, Deserialize)]
pub struct Groups {
    pub music: Vec<usize>,
    pub speech: Vec<usize>,
    pub sing: Vec<usize>,
}

#[derive(Deserialize)]
struct LabelsFile {
    groups: Groups,
}

/// Frame-wise class probabilities on the `fps` grid.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Probs {
    pub fps: f64,
    pub duration: f64,
    pub music: Vec<f32>,
    pub speech: Vec<f32>,
    pub sing: Vec<f32>,
}

impl Probs {
    pub fn len(&self) -> usize {
        self.music.len()
    }
    pub fn is_empty(&self) -> bool {
        self.music.is_empty()
    }

    /// Compact little-endian binary: magic, fps, duration, n, then 3×n f32.
    pub fn to_bytes(&self) -> Vec<u8> {
        let n = self.len();
        let mut b = Vec::with_capacity(28 + n * 12);
        b.extend_from_slice(b"CLPROBS1");
        b.extend_from_slice(&self.fps.to_le_bytes());
        b.extend_from_slice(&self.duration.to_le_bytes());
        b.extend_from_slice(&(n as u32).to_le_bytes());
        for v in [&self.music, &self.speech, &self.sing] {
            for x in v.iter() {
                b.extend_from_slice(&x.to_le_bytes());
            }
        }
        b
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() < 28 || &b[..8] != b"CLPROBS1" {
            return None;
        }
        let fps = f64::from_le_bytes(b[8..16].try_into().ok()?);
        let duration = f64::from_le_bytes(b[16..24].try_into().ok()?);
        let n = u32::from_le_bytes(b[24..28].try_into().ok()?) as usize;
        if b.len() < 28 + n * 12 {
            return None;
        }
        let rd = |k: usize| -> Vec<f32> {
            b[28 + k * n * 4..28 + (k + 1) * n * 4].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect()
        };
        Some(Probs { fps, duration, music: rd(0), speech: rd(1), sing: rd(2) })
    }
}

pub struct Tagger {
    session: Session,
    groups: Groups,
    pub device: Device,
}

impl Tagger {
    /// `model` = `panns_sed.onnx`; label groups from `panns_labels.json` next to it.
    pub fn open(model: &Path, prefer_cpu: bool) -> Result<Self, ModelError> {
        let labels = model.with_file_name("panns_labels.json");
        let groups: Groups = std::fs::read(&labels)
            .ok()
            .and_then(|b| serde_json::from_slice::<LabelsFile>(&b).ok())
            .map(|l| l.groups)
            .ok_or_else(|| ModelError::Missing(labels.clone()))?;
        let (session, device) = open_session(model, prefer_cpu)?;
        Ok(Self { session, groups, device })
    }

    /// Per-frame (music, speech, sing) group maxima for one chunk.
    fn infer_chunk(&mut self, chunk: &[f32]) -> Result<Vec<[f32; 3]>, ModelError> {
        let input = TensorRef::from_array_view(([1usize, chunk.len()], chunk))?;
        let outputs = self.session.run(ort::inputs![input])?;
        let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
        let (t, c) = (shape[1] as usize, shape[2] as usize);
        let gmax = |row: &[f32], idx: &[usize]| idx.iter().map(|&i| row[i]).fold(f32::NEG_INFINITY, f32::max);
        Ok((0..t)
            .map(|k| {
                let row = &data[k * c..(k + 1) * c];
                [gmax(row, &self.groups.music), gmax(row, &self.groups.speech), gmax(row, &self.groups.sing)]
            })
            .collect())
    }

    /// Run over a mono 32 kHz sample stream. `read` fills a buffer and returns
    /// the count (0 = end). `total_samples` (if known) drives progress.
    pub fn probs_streaming(
        &mut self,
        mut read: impl FnMut(&mut [f32]) -> std::io::Result<usize>,
        total_samples: Option<u64>,
        progress: &ProgressFn,
        cancel: &CancellationToken,
    ) -> Result<Probs, PannsError> {
        let chunk_len = CHUNK_SEC * SAMPLE_RATE as usize;
        let mut buf = vec![0f32; chunk_len];
        let mut frames: Vec<[f32; 3]> = Vec::new();
        let mut total = 0u64;
        loop {
            if cancel.is_cancelled() {
                return Err(Cancelled.into());
            }
            // fill a whole chunk (reads may be short)
            let mut n = 0;
            while n < chunk_len {
                let k = read(&mut buf[n..])?;
                if k == 0 {
                    break;
                }
                n += k;
            }
            if n == 0 {
                break;
            }
            total += n as u64;
            if n < SAMPLE_RATE as usize / 2 {
                // tail shorter than 0.5 s: repeat the last frame (dl_tagger.py)
                if let Some(&last) = frames.last() {
                    frames.push(last);
                }
            } else {
                frames.extend(self.infer_chunk(&buf[..n])?);
            }
            if let Some(t) = total_samples {
                progress(Progress::new("detect.tag", total.min(t), t));
            }
            if n < chunk_len {
                break;
            }
        }
        Ok(resample_grid(&frames, total as f64 / SAMPLE_RATE as f64))
    }
}

/// `np.interp(arange(n)/fps, linspace(0, n/fps, len(fw)), fw)` per group, with
/// `n = int(duration * fps)`.
pub fn resample_grid(frames: &[[f32; 3]], duration: f64) -> Probs {
    let n = (duration * FPS) as usize;
    let mut p = Probs { fps: FPS, duration, music: vec![0.0; n], speech: vec![0.0; n], sing: vec![0.0; n] };
    let m = frames.len();
    if m == 0 || n == 0 {
        return p;
    }
    let span = n as f64 / FPS;
    let step = if m > 1 { span / (m - 1) as f64 } else { 0.0 };
    for i in 0..n {
        let t = i as f64 / FPS;
        let v = if m == 1 || step == 0.0 {
            frames[0]
        } else {
            // np.interp: locate j with x[j] <= t < x[j+1] on x[j] = j*step
            let pos = t / step;
            let j = (pos.floor() as usize).min(m - 1);
            if j >= m - 1 {
                frames[m - 1]
            } else {
                let x0 = j as f64 * step;
                let x1 = (j + 1) as f64 * step;
                let w = ((t - x0) / (x1 - x0)) as f32;
                let (a, b) = (frames[j], frames[j + 1]);
                [a[0] + (b[0] - a[0]) * w, a[1] + (b[1] - a[1]) * w, a[2] + (b[2] - a[2]) * w]
            }
        };
        p.music[i] = v[0];
        p.speech[i] = v[1];
        p.sing[i] = v[2];
    }
    p
}

#[derive(Debug, thiserror::Error)]
pub enum PannsError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Cancelled(#[from] Cancelled),
}

impl From<ort::Error> for PannsError {
    fn from(e: ort::Error) -> Self {
        PannsError::Model(e.into())
    }
}
