//! Export one song: rendered audio (+ the clip's video), cut to the song.
//!
//! Video: frame-accurate cut with re-encode (`h264_nvenc`, falling back to
//! `libx264`), or `copy` — snap the in-point back to the previous keyframe and
//! stream-copy the video (fast, lossless video, slightly early start).
//! Audio-only formats (WAV / FLAC / MP3) skip the video entirely.
//! [`export_stems`] hands the aligned pieces to an editor / DAW instead.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::{Deserialize, Serialize};

use crate::audio::{ffmpeg, wav};
use crate::progress::{Cancelled, CancellationToken, Progress, ProgressFn};
use crate::project::Loudness;
use crate::render::{MuteSpan, render, render_full};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// video + audio
    #[default]
    Mp4,
    /// audio only, 24-bit PCM
    Wav,
    /// audio only, lossless
    Flac,
    /// audio only, 320 kbps
    Mp3,
}

impl OutputFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Mp4 => "mp4",
            OutputFormat::Wav => "wav",
            OutputFormat::Flac => "flac",
            OutputFormat::Mp3 => "mp3",
        }
    }
    pub fn has_video(&self) -> bool {
        *self == OutputFormat::Mp4
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSpec {
    pub source_mp4: PathBuf,
    pub orig_wav: PathBuf,
    pub vocals_wav: PathBuf,
    /// Replace the rendered audio with this file (aligned to clip t = 0, e.g. a
    /// processed stem edited in a DAW and brought back). Mutes are not applied.
    pub audio_override: Option<PathBuf>,
    /// song range in clip seconds
    pub in_t: f64,
    pub out_t: f64,
    /// mutes in clip seconds
    pub mutes: Vec<MuteSpan>,
    /// output file (extension should match `format`)
    pub out: PathBuf,
    pub format: OutputFormat,
    /// "h264_nvenc" | "libx264" | "copy" (mp4 only)
    pub video_codec: String,
    pub video_quality: u32,
    pub audio_bitrate_k: u32,
    pub fade_in: f64,
    pub fade_out: f64,
    pub loudness: Option<Loudness>,
    /// container tags (title, artist, comment, date, …)
    pub metadata: Vec<(String, String)>,
}

/// EBU R128 measurement of the exported audio (before normalisation).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct LoudnessReport {
    /// integrated loudness, LUFS
    pub integrated: f64,
    /// true peak, dBTP
    pub true_peak: f64,
    /// loudness range, LU
    pub range: f64,
    /// target applied, if normalisation was on
    pub normalized_to: Option<f64>,
    /// how loudnorm will treat this song: "linear" (one static gain) or
    /// "dynamic" (its gain would push the true peak over the ceiling, so it
    /// compresses — can pump on quiet songs with loud choruses). None when
    /// normalisation is off.
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ExportResult {
    pub path: PathBuf,
    pub format: OutputFormat,
    pub duration: f64,
    /// encoder actually used ("" for audio-only)
    pub video_codec: String,
    /// in-point actually used (differs from the request in copy mode)
    pub in_t: f64,
    pub loudness: Option<LoudnessReport>,
    /// gain (dB, negative) applied because the rendered audio would clip
    /// (orig − vocals can exceed full scale); None = untouched. Only without
    /// loudness normalisation, which has its own true-peak ceiling.
    pub clip_guard_db: Option<f64>,
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

/// Run ffmpeg with `-progress pipe:1` (already in `cmd`) reporting over `dur` s.
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
            && let Ok(us) = v.trim().parse::<u64>()
        {
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

/// Measured loudness values from ffmpeg's `loudnorm` analysis pass.
struct Measured {
    i: f64,
    tp: f64,
    lra: f64,
    thresh: f64,
    offset: f64,
}

fn measure(file: &Path, target: Option<Loudness>) -> Result<Measured, ExportError> {
    let (i, tp) = target.map(|l| (l.target_lufs, l.true_peak)).unwrap_or((-14.0, -1.0));
    let mut c = std::process::Command::new(ffmpeg::ffmpeg_path());
    c.args(["-hide_banner", "-nostdin", "-i"]).arg(file);
    c.args(["-af", &format!("loudnorm=I={i}:TP={tp}:LRA=20:print_format=json"), "-f", "null", "-"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let out = c.output()?;
    let err = String::from_utf8_lossy(&out.stderr);
    let json = err.rfind('{').and_then(|a| err[a..].find('}').map(|b| &err[a..=a + b]));
    let v: serde_json::Value = json
        .and_then(|j| serde_json::from_str(j).ok())
        .ok_or_else(|| ExportError::Invalid("loudness analysis failed".into()))?;
    let f = |k: &str| v[k].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(f64::NAN);
    Ok(Measured { i: f("input_i"), tp: f("input_tp"), lra: f("input_lra"), thresh: f("input_thresh"), offset: f("target_offset") })
}

fn sanitize_meta(v: &str) -> String {
    v.replace(['\r', '\n'], " ")
}

/// Blocking; run on a worker thread.
pub fn export_song(spec: &ExportSpec, progress: &ProgressFn, cancel: &CancellationToken) -> Result<ExportResult, ExportError> {
    let t0 = std::time::Instant::now();
    if spec.out_t <= spec.in_t {
        return Err(ExportError::Invalid("empty song range".into()));
    }
    let video = spec.format.has_video();
    let copy = video && spec.video_codec == "copy";
    let in_t = if copy { keyframe_before(&spec.source_mp4, spec.in_t)? } else { spec.in_t };
    let dur = spec.out_t - in_t;

    progress(Progress::note("export.render", "render_audio", "rendering audio"));
    let (rate, audio) = match &spec.audio_override {
        Some(path) => {
            let pcm = ffmpeg::decode(path, crate::stems::STEM_RATE, 2, None, None)?.read_all()?;
            let silence = vec![0f32; pcm.len()];
            (crate::stems::STEM_RATE, render(&pcm, &silence, crate::stems::STEM_RATE, in_t, spec.out_t, &[], spec.fade_in, spec.fade_out))
        }
        None => {
            // read just the song's frames; render on that window's time base
            let (rate, _, _) = wav::info(&spec.orig_wav)?;
            let a = (in_t.max(0.0) * rate as f64).round() as u64;
            let n = ((spec.out_t - in_t).max(0.0) * rate as f64).round() as u64 + 1;
            let orig = wav::read_f32_range(&spec.orig_wav, a, n)?;
            let voc = wav::read_f32_range(&spec.vocals_wav, a, n)?;
            if orig.sample_rate != voc.sample_rate || orig.channels != 2 || voc.channels != 2 {
                return Err(ExportError::Invalid("stems must be stereo at the same rate".into()));
            }
            let t0 = a as f64 / rate as f64;
            let mutes = shift(&spec.mutes, t0);
            let out = render(&orig.samples, &voc.samples, rate, in_t - t0, spec.out_t - t0, &mutes, spec.fade_in, spec.fade_out);
            (rate, out)
        }
    };
    // over full scale (accompaniment = orig − vocals can be): scale the song
    // down just enough instead of letting the encoder hard-clip
    let mut audio = audio;
    let peak = audio.iter().fold(0f32, |m, x| m.max(x.abs()));
    let clip_guard_db = if spec.loudness.is_none() && peak > 0.999 {
        let g = 0.999 / peak;
        audio.iter_mut().for_each(|x| *x *= g);
        Some(20.0 * (g as f64).log10())
    } else {
        None
    };
    if let Some(dir) = spec.out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp_wav = spec.out.with_extension("render.wav");
    wav::write_f32(&tmp_wav, rate, 2, &audio)?;
    drop(audio);

    // loudness: always measured (shown to the user), applied only when requested
    progress(Progress::note("export.render", "measure_loudness", "measuring loudness"));
    let m = measure(&tmp_wav, spec.loudness).ok();
    // loudnorm stays linear only if the gain keeps the true peak under the
    // ceiling and the target LRA isn't below the measured one (so the LRA
    // target is set to at least the measured range)
    let mode = |l: &Loudness, m: &Measured| if m.tp + (l.target_lufs - m.i) <= l.true_peak { "linear" } else { "dynamic" };
    let report = m.as_ref().map(|m| LoudnessReport {
        integrated: m.i,
        true_peak: m.tp,
        range: m.lra,
        normalized_to: spec.loudness.map(|l| l.target_lufs),
        mode: spec.loudness.map(|l| mode(&l, m).to_string()),
    });
    let audio_filter = match (spec.loudness, &m) {
        (Some(l), Some(m)) if m.i.is_finite() && m.i > -70.0 => Some(format!(
            "loudnorm=I={}:TP={}:LRA={}:measured_I={}:measured_TP={}:measured_LRA={}:measured_thresh={}:offset={}:linear=true,aresample={rate}",
            l.target_lufs,
            l.true_peak,
            m.lra.ceil().clamp(20.0, 50.0),
            m.i,
            m.tp,
            m.lra,
            m.thresh,
            m.offset
        )),
        _ => None,
    };

    let build = |codec: &str| {
        let mut c = ffmpeg::command();
        c.arg("-y").args(["-progress", "pipe:1", "-nostats"]);
        if video {
            c.args(["-ss", &format!("{in_t:.6}")]).arg("-i").arg(&spec.source_mp4);
        }
        c.arg("-i").arg(&tmp_wav);
        c.args(["-t", &format!("{dur:.6}")]);
        if video {
            c.args(["-map", "0:v:0", "-map", "1:a:0"]);
        } else {
            c.args(["-map", "0:a:0"]);
        }
        if let Some(f) = &audio_filter {
            c.args(["-af", f]);
        }
        match (spec.format, codec) {
            (OutputFormat::Mp4, "copy") => {
                c.args(["-c:v", "copy"]);
            }
            (OutputFormat::Mp4, "h264_nvenc") => {
                c.args(["-c:v", "h264_nvenc", "-preset", "p4", "-tune", "hq", "-rc", "vbr", "-cq", &spec.video_quality.to_string(), "-b:v", "0"]);
            }
            (OutputFormat::Mp4, _) => {
                c.args(["-c:v", "libx264", "-preset", "medium", "-crf", &spec.video_quality.to_string()]);
            }
            _ => {}
        }
        match spec.format {
            OutputFormat::Mp4 => {
                c.args(["-c:a", "aac", "-b:a", &format!("{}k", spec.audio_bitrate_k), "-movflags", "+faststart"]);
            }
            OutputFormat::Wav => {
                c.args(["-c:a", "pcm_s24le"]);
            }
            OutputFormat::Flac => {
                c.args(["-c:a", "flac", "-sample_fmt", "s32"]);
            }
            OutputFormat::Mp3 => {
                c.args(["-c:a", "libmp3lame", "-b:a", "320k"]);
            }
        }
        for (k, v) in &spec.metadata {
            if !v.is_empty() {
                c.args(["-metadata", &format!("{k}={}", sanitize_meta(v))]);
            }
        }
        c.arg(&spec.out);
        c
    };

    let mut used = if video { spec.video_codec.clone() } else { String::new() };
    let mut res = run_with_progress(build(&used), dur, progress, cancel);
    if res.is_err() && used == "h264_nvenc" && !cancel.is_cancelled() {
        tracing::warn!("h264_nvenc failed, retrying with libx264: {:?}", res.as_ref().err());
        used = "libx264".into();
        res = run_with_progress(build(&used), dur, progress, cancel);
    }
    let _ = std::fs::remove_file(&tmp_wav);
    if res.is_err() {
        let _ = std::fs::remove_file(&spec.out);
    }
    res?;
    Ok(ExportResult {
        path: spec.out.clone(),
        format: spec.format,
        duration: dur,
        video_codec: used,
        in_t,
        loudness: report,
        clip_guard_db,
        elapsed_sec: t0.elapsed().as_secs_f64(),
    })
}

// ── editor / DAW hand-off ────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct StemsSpec {
    pub source_mp4: PathBuf,
    pub orig_wav: PathBuf,
    pub vocals_wav: PathBuf,
    /// song range in clip seconds; files cover `[in − handles, out + handles]`
    pub in_t: f64,
    pub out_t: f64,
    pub handles: f64,
    pub mutes: Vec<MuteSpan>,
    /// output folder (created)
    pub dir: PathBuf,
    /// base name for the files
    pub name: String,
    /// VOD time of clip t = 0 (for the marker sheet)
    pub vod_offset: f64,
    /// also write a light 720p reference video
    pub reference_video: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct StemsResult {
    pub dir: PathBuf,
    /// original mix, processed mix, vocals, accompaniment — same start, same length
    pub files: Vec<PathBuf>,
    pub markers_csv: PathBuf,
    pub reaper_project: PathBuf,
    pub reference_video: Option<PathBuf>,
    /// clip seconds where the files start
    pub start: f64,
    pub duration: f64,
}

/// Mutes moved onto a time base that starts at `t0`.
fn shift(mutes: &[MuteSpan], t0: f64) -> Vec<MuteSpan> {
    mutes.iter().map(|m| MuteSpan { start: m.start - t0, end: m.end - t0, ..*m }).collect()
}

fn hms_ms(t: f64) -> String {
    let ms = (t.max(0.0) * 1000.0).round() as u64;
    format!("{}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Write sample-aligned WAV stems (32-bit float, clip rate) + a marker CSV and
/// a Reaper project laying them out, for finishing in an editor / DAW.
pub fn export_stems(spec: &StemsSpec, progress: &ProgressFn) -> Result<StemsResult, ExportError> {
    std::fs::create_dir_all(&spec.dir)?;
    progress(Progress::note("export.stems", "write_stems", "writing stems"));
    let (rate, _, frames_o) = wav::info(&spec.orig_wav)?;
    let (_, _, frames_v) = wav::info(&spec.vocals_wav)?;
    let a = (spec.in_t - spec.handles).max(0.0);
    let total = frames_o.min(frames_v) as f64 / rate as f64;
    let b = (spec.out_t + spec.handles).min(total);
    // only [a, b] is read; times inside it are relative to a
    let fa = (a * rate as f64).round() as u64;
    let fb = ((b * rate as f64).round() as u64).max(fa);
    let orig = wav::read_f32_range(&spec.orig_wav, fa, fb - fa)?;
    let voc = wav::read_f32_range(&spec.vocals_wav, fa, fb - fa)?;
    let t0 = fa as f64 / rate as f64;
    let processed = render_full(&orig.samples, &voc.samples, rate, &shift(&spec.mutes, t0));
    let inst: Vec<f32> = orig.samples.iter().zip(&voc.samples).map(|(o, v)| o - v).collect();
    let mut files = Vec::new();
    for (suffix, data) in [("original", &orig.samples), ("processed", &processed), ("vocals", &voc.samples), ("accompaniment", &inst)] {
        let p = spec.dir.join(format!("{}_{suffix}.wav", spec.name));
        wav::write_f32(&p, rate, 2, data)?;
        files.push(p);
    }

    // markers: times relative to the files' start, plus VOD timecode
    let mut csv = String::from("\u{feff}name,start_sec,end_sec,vod_start,vod_end\n");
    let mut marks: Vec<(String, f64, f64)> = vec![("song".into(), spec.in_t - a, spec.out_t - a)];
    for (k, m) in spec.mutes.iter().enumerate().filter(|(_, m)| m.end > a && m.start < b) {
        marks.push((format!("mute {}", k + 1), (m.start - a).max(0.0), (m.end - a).min(b - a)));
    }
    for (n, s, e) in &marks {
        csv.push_str(&format!("{n},{s:.3},{e:.3},{},{}\n", hms_ms(s + a + spec.vod_offset), hms_ms(e + a + spec.vod_offset)));
    }
    let markers_csv = spec.dir.join(format!("{}_markers.csv", spec.name));
    std::fs::write(&markers_csv, csv)?;

    // Reaper project: four tracks at 0, song as a region, mutes as markers
    let len = b - a;
    let mut rpp = String::from("<REAPER_PROJECT 0.1 \"7.0\"\n  SAMPLERATE ");
    rpp.push_str(&format!("{rate} 0 0\n"));
    let mut idx = 1;
    for (n, s, e) in &marks {
        rpp.push_str(&format!("  MARKER {idx} {s:.6} \"{n}\" 1\n  MARKER {idx} {e:.6} \"\" 1\n"));
        idx += 1;
    }
    for f in &files {
        let name = f.file_stem().unwrap().to_string_lossy().replace('"', "'");
        let file = f.file_name().unwrap().to_string_lossy().replace('"', "'");
        rpp.push_str(&format!(
            "  <TRACK\n    NAME \"{name}\"\n    <ITEM\n      POSITION 0\n      LENGTH {len:.6}\n      NAME \"{name}\"\n      <SOURCE WAVE\n        FILE \"{file}\"\n      >\n    >\n  >\n"
        ));
    }
    rpp.push_str(">\n");
    let reaper_project = spec.dir.join(format!("{}.rpp", spec.name));
    std::fs::write(&reaper_project, rpp)?;

    let reference_video = if spec.reference_video {
        progress(Progress::note("export.stems", "write_reference_video", "writing reference video"));
        let p = spec.dir.join(format!("{}_reference.mp4", spec.name));
        let mut c = ffmpeg::command();
        c.arg("-y").args(["-ss", &format!("{a:.6}")]).arg("-i").arg(&spec.source_mp4);
        c.args(["-t", &format!("{len:.6}"), "-vf", "scale=-2:720", "-c:v", "libx264", "-preset", "veryfast", "-crf", "28", "-c:a", "aac", "-b:a", "128k"]);
        c.arg(&p);
        ffmpeg::run(c)?;
        Some(p)
    } else {
        None
    };
    Ok(StemsResult { dir: spec.dir.clone(), files, markers_csv, reaper_project, reference_video, start: a, duration: len })
}
