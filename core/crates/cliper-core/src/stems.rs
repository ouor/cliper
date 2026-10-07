//! Clip → stems: `orig.wav` (the clip's own audio, 48 kHz stereo f32) and
//! `vocals.wav` (model output resampled back to 48 kHz, same length), plus
//! their waveform peaks. Everything streams, so memory stays constant however
//! long the clip is.
//!
//! The accompaniment is never stored: it is `orig − vocals` by construction,
//! which keeps vocal-muting exact (gain 1 ⇒ untouched original).
//!
//! Stem t = 0 is the clip file's t = 0 (what `-ss` and the video use): if the
//! audio stream starts later than the file (e.g. video first after a remux),
//! the stems are padded with that much silence.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audio::{ffmpeg, peaks, wav};
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

/// Seconds between the file's start and its first audio sample (≥ 0).
pub fn audio_start_delay(clip: &Path) -> f64 {
    #[derive(Deserialize)]
    struct S {
        codec_type: Option<String>,
        start_time: Option<String>,
    }
    #[derive(Deserialize)]
    struct F {
        start_time: Option<String>,
    }
    #[derive(Deserialize)]
    struct O {
        streams: Vec<S>,
        format: F,
    }
    let mut c = std::process::Command::new(crate::local::ffprobe_path());
    c.args(["-v", "error", "-show_entries", "stream=codec_type,start_time:format=start_time", "-of", "json"]).arg(clip);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let Some(o) = c.output().ok().and_then(|o| serde_json::from_slice::<O>(&o.stdout).ok()) else { return 0.0 };
    let num = |s: &Option<String>| s.as_deref().and_then(|x| x.parse::<f64>().ok());
    let file = num(&o.format.start_time).unwrap_or(0.0);
    let audio = o.streams.iter().find(|s| s.codec_type.as_deref() == Some("audio")).and_then(|s| num(&s.start_time)).unwrap_or(file);
    (audio - file).clamp(0.0, 10.0)
}

/// Stream `input` decoded at `rate` stereo into `sink`, after `pad` frames of silence.
fn pump(input: &Path, rate: u32, pad: usize, sink: &mut dyn FnMut(&[f32]) -> std::io::Result<()>) -> Result<(), StemError> {
    if pad > 0 {
        sink(&vec![0f32; pad * 2])?;
    }
    let mut rd = ffmpeg::decode(input, rate, 2, None, None)?;
    let mut buf = vec![0f32; 1 << 16];
    loop {
        let n = rd.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        sink(&buf[..n])?;
    }
}

/// Separate a clip with an already-open separator. Blocking.
pub fn separate_clip(clip: &Path, out_dir: &Path, sep: &mut Separator, progress: &ProgressFn, cancel: &CancellationToken) -> Result<Stems, StemError> {
    let t0 = std::time::Instant::now();
    std::fs::create_dir_all(out_dir)?;
    progress(Progress::note("separate", "decode", "decoding"));
    let delay = audio_start_delay(clip);

    // 1) original audio at the stem rate (+ peaks)
    let orig_path = out_dir.join("orig.wav");
    let mut w = wav::WavWriter::create(&orig_path, STEM_RATE, 2)?;
    let mut pk = peaks::PeakBuilder::new(STEM_RATE, 100);
    pump(clip, STEM_RATE, (delay * STEM_RATE as f64).round() as usize, &mut |x| {
        pk.push_stereo(x);
        w.write(x)
    })?;
    let frames = w.finish()?;
    pk.finish().write(&out_dir.join("orig.peaks"))?;

    // 2) model at its own rate, streamed decode → model → WAV
    let model_rate = sep.spec.sample_rate;
    let tmp = out_dir.join("vocals.model.wav");
    {
        let mut vw = wav::WavWriter::create(&tmp, model_rate, 2)?;
        let pad = (delay * model_rate as f64).round() as usize;
        let mut rd = ffmpeg::decode(clip, model_rate, 2, None, None)?;
        let mut pad_left = pad * 2;
        let total = (frames as f64 * model_rate as f64 / STEM_RATE as f64) as u64;
        sep.separate_stream(
            &mut |b: &mut [f32]| {
                if pad_left > 0 {
                    let n = pad_left.min(b.len());
                    b[..n].fill(0.0);
                    pad_left -= n;
                    return Ok(n);
                }
                rd.read(b).map_err(std::io::Error::other)
            },
            &mut |x: &[f32]| vw.write(x),
            Some(total),
            progress,
            cancel,
        )?;
        vw.finish()?;
    }

    // 3) back to the stem rate, exactly as long as orig (+ peaks)
    let vocals_path = out_dir.join("vocals.wav");
    let mut vw = wav::WavWriter::create(&vocals_path, STEM_RATE, 2)?;
    let mut pk = peaks::PeakBuilder::new(STEM_RATE, 100);
    let want = frames * 2;
    let mut got = 0u64;
    pump(&tmp, STEM_RATE, 0, &mut |x| {
        let n = ((want - got) as usize).min(x.len());
        got += n as u64;
        pk.push_stereo(&x[..n]);
        vw.write(&x[..n])
    })?;
    if got < want {
        let pad = vec![0f32; (want - got) as usize];
        pk.push_stereo(&pad);
        vw.write(&pad)?;
    }
    vw.finish()?;
    pk.finish().write(&out_dir.join("vocals.peaks"))?;
    let _ = std::fs::remove_file(&tmp);

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

/// Open a separator and separate (CLI use): on GPU out-of-memory at batch 1
/// the clip is redone on the CPU.
pub fn separate_clip_auto(
    clip: &Path,
    out_dir: &Path,
    models_dir: &Path,
    spec: ModelSpec,
    prefer_cpu: bool,
    progress: &ProgressFn,
    cancel: &CancellationToken,
) -> Result<Stems, StemError> {
    let mut sep = Separator::open(models_dir, spec.clone(), prefer_cpu).map_err(SeparateError::from)?;
    match separate_clip(clip, out_dir, &mut sep, progress, cancel) {
        Err(StemError::Separate(SeparateError::OutOfMemory)) => {
            drop(sep);
            progress(Progress::note("separate", "gpu_out_of_memory", "GPU out of memory; separating on the CPU"));
            let mut cpu = Separator::open(models_dir, spec, true).map_err(SeparateError::from)?;
            separate_clip(clip, out_dir, &mut cpu, progress, cancel)
        }
        r => r,
    }
}
