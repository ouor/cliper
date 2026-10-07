//! Clip → stems: `orig.wav` (the clip's own audio, 48 kHz stereo f32) and
//! `vocals.wav` (model output resampled back to 48 kHz, same length).
//! The accompaniment is never stored: it is `orig − vocals` by construction,
//! which keeps vocal-muting exact (gain 1 ⇒ untouched original).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audio::{ffmpeg, wav};
use crate::ml::separator::{ModelSpec, SeparateError, Separator};
use crate::progress::{CancellationToken, Progress, ProgressFn};

pub const STEM_RATE: u32 = 48000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Stems {
    pub orig: PathBuf,
    pub vocals: PathBuf,
    pub sample_rate: u32,
    pub frames: u64,
    pub model: String,
    pub device: crate::ml::runtime::Device,
    pub elapsed_sec: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum StemError {
    #[error(transparent)]
    Ffmpeg(#[from] ffmpeg::FfmpegError),
    #[error(transparent)]
    Separate(#[from] SeparateError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Blocking; run on a worker thread.
pub fn separate_clip(
    clip: &Path,
    out_dir: &Path,
    models_dir: &Path,
    spec: ModelSpec,
    prefer_cpu: bool,
    progress: &ProgressFn,
    cancel: &CancellationToken,
) -> Result<Stems, StemError> {
    let t0 = std::time::Instant::now();
    std::fs::create_dir_all(out_dir)?;
    progress(Progress::note("separate", "decode", "decoding"));
    let orig = ffmpeg::decode(clip, STEM_RATE, 2, None, None)?.read_all()?;
    let frames = (orig.len() / 2) as u64;
    let orig_path = out_dir.join("orig.wav");
    wav::write_f32(&orig_path, STEM_RATE, 2, &orig)?;

    let mut sep = Separator::open(models_dir, spec, prefer_cpu).map_err(SeparateError::from)?;
    let mix = ffmpeg::decode(clip, sep.spec.sample_rate, 2, None, None)?.read_all()?;
    let stem = sep.separate(&mix, progress, cancel)?;
    drop(mix);

    // back to the clip rate
    let tmp = out_dir.join("vocals.model.wav");
    wav::write_f32(&tmp, sep.spec.sample_rate, 2, &stem)?;
    let mut voc = ffmpeg::decode(&tmp, STEM_RATE, 2, None, None)?.read_all()?;
    let _ = std::fs::remove_file(&tmp);
    voc.resize(orig.len(), 0.0);
    let vocals_path = out_dir.join("vocals.wav");
    wav::write_f32(&vocals_path, STEM_RATE, 2, &voc)?;

    Ok(Stems {
        orig: orig_path,
        vocals: vocals_path,
        sample_rate: STEM_RATE,
        frames,
        model: sep.spec.name.clone(),
        device: sep.device,
        elapsed_sec: t0.elapsed().as_secs_f64(),
    })
}
