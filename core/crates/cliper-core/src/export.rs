//! Export one song: rendered audio + the clip's video, cut to the song.
//!
//! Default is a frame-accurate cut with re-encode (`h264_nvenc`, falling back
//! to `libx264`). `copy` mode snaps the in-point back to the previous keyframe
//! and stream-copies the video (fast, lossless video, slightly early start).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

use crate::audio::{ffmpeg, wav};
use crate::progress::{Cancelled, CancellationToken, Progress, ProgressFn};
use crate::render::{MuteSpan, render};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSpec {
    pub source_mp4: PathBuf,
    pub orig_wav: PathBuf,
    pub vocals_wav: PathBuf,
    /// song range in clip seconds
    pub in_t: f64,
    pub out_t: f64,
    /// mutes in clip seconds
    pub mutes: Vec<MuteSpan>,
    pub out: PathBuf,
    /// "h264_nvenc" | "libx264" | "copy"
    pub video_codec: String,
    pub video_quality: u32,
    pub audio_bitrate_k: u32,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ExportResult {
    pub path: PathBuf,
    pub duration: f64,
    pub video_codec: String,
    /// in-point actually used (differs from the request in copy mode)
    pub in_t: f64,
    pub elapsed_sec: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error(transparent)]
    Ffmpeg(#[from] ffmpeg::FfmpegError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Cancelled(#[from] Cancelled),
}

fn ffprobe_path() -> PathBuf {
    let ff = ffmpeg::ffmpeg_path();
    let name = if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" };
    match ff.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) if dir.join(name).is_file() => dir.join(name),
        _ => PathBuf::from("ffprobe"),
    }
}

/// Last video keyframe at or before `t` (seconds), via ffprobe.
pub fn keyframe_before(mp4: &Path, t: f64) -> Result<f64, ExportError> {
    let from = (t - 12.0).max(0.0);
    let out = std::process::Command::new(ffprobe_path())
        .args(["-v", "error", "-select_streams", "v:0", "-skip_frame", "nokey", "-show_entries", "frame=pts_time", "-of", "csv=p=0"])
        .args(["-read_intervals", &format!("{from:.3}%{:.3}", t + 0.001)])
        .arg(mp4)
        .output()?;
    let best = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
        .filter(|&k| k <= t + 1e-3)
        .fold(None, |acc: Option<f64>, k| Some(acc.map_or(k, |a| a.max(k))));
    Ok(best.unwrap_or(from))
}

/// Run ffmpeg with `-progress` and report `stage` progress over `dur` seconds.
fn run_with_progress(mut cmd: std::process::Command, dur: f64, progress: &ProgressFn, cancel: &CancellationToken) -> Result<(), ExportError> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(ffmpeg::FfmpegError::from)?;
    let stderr = child.stderr.take().unwrap();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stderr), &mut s);
        s
    });
    let total_ms = (dur * 1000.0).max(1.0) as u64;
    for line in BufReader::new(child.stdout.take().unwrap()).lines() {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Cancelled.into());
        }
        let line = line?;
        if let Some(v) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms="))
            && let Ok(us) = v.trim().parse::<u64>() {
                progress(Progress::new("export.encode", (us / 1000).min(total_ms), total_ms));
            }
    }
    let st = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    if !st.success() {
        let tail: String = err.chars().rev().take(800).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(ffmpeg::FfmpegError::Failed(tail).into());
    }
    Ok(())
}

/// Blocking; run on a worker thread.
pub fn export_song(spec: &ExportSpec, progress: &ProgressFn, cancel: &CancellationToken) -> Result<ExportResult, ExportError> {
    let t0 = std::time::Instant::now();
    if spec.out_t <= spec.in_t {
        return Err(ExportError::Invalid("empty song range".into()));
    }
    let copy = spec.video_codec == "copy";
    let in_t = if copy { keyframe_before(&spec.source_mp4, spec.in_t)? } else { spec.in_t };
    let dur = spec.out_t - in_t;

    progress(Progress::note("export.render", "rendering audio"));
    let orig = wav::read_f32(&spec.orig_wav)?;
    let voc = wav::read_f32(&spec.vocals_wav)?;
    if orig.sample_rate != voc.sample_rate || orig.channels != 2 || voc.channels != 2 {
        return Err(ExportError::Invalid("stems must be stereo at the same rate".into()));
    }
    let audio = render(&orig.samples, &voc.samples, orig.sample_rate, in_t, spec.out_t, &spec.mutes, 0.005);
    if let Some(dir) = spec.out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp_wav = spec.out.with_extension("render.wav");
    wav::write_f32(&tmp_wav, orig.sample_rate, 2, &audio)?;
    drop((orig, voc, audio));

    let build = |codec: &str| {
        let mut c = ffmpeg::command();
        c.arg("-y").args(["-progress", "pipe:1", "-nostats"]).args(["-ss", &format!("{in_t:.6}")]).arg("-i").arg(&spec.source_mp4).arg("-i").arg(&tmp_wav);
        c.args(["-t", &format!("{dur:.6}"), "-map", "0:v:0", "-map", "1:a:0"]);
        match codec {
            "copy" => {
                c.args(["-c:v", "copy"]);
            }
            "h264_nvenc" => {
                c.args(["-c:v", "h264_nvenc", "-preset", "p4", "-tune", "hq", "-rc", "vbr", "-cq", &spec.video_quality.to_string(), "-b:v", "0"]);
            }
            _ => {
                c.args(["-c:v", "libx264", "-preset", "medium", "-crf", &spec.video_quality.to_string()]);
            }
        }
        c.args(["-c:a", "aac", "-b:a", &format!("{}k", spec.audio_bitrate_k), "-movflags", "+faststart"]);
        if let Some(t) = &spec.title {
            c.args(["-metadata", &format!("title={t}")]);
        }
        c.arg(&spec.out);
        c
    };

    let mut used = spec.video_codec.clone();
    let mut res = run_with_progress(build(&used), dur, progress, cancel);
    if res.is_err() && used == "h264_nvenc" && !cancel.is_cancelled() {
        tracing::warn!("h264_nvenc failed, retrying with libx264: {:?}", res.as_ref().err());
        used = "libx264".into();
        res = run_with_progress(build(&used), dur, progress, cancel);
    }
    let _ = std::fs::remove_file(&tmp_wav);
    res?;
    Ok(ExportResult { path: spec.out.clone(), duration: dur, video_codec: used, in_t, elapsed_sec: t0.elapsed().as_secs_f64() })
}
