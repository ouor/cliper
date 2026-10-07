//! Local recordings (e.g. OBS) as a source instead of a platform VOD.
//!
//! Same pipeline, different I/O: the analysis audio is copied out of the file
//! (no network), and a clip is a stream-copied cut of the file. Recordings with
//! several audio tracks (OBS multitrack: mix / mic / music) can pick the track
//! used for everything via `audio_track`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::audio::ffmpeg;
use crate::export::keyframe_before;
use crate::hls::m3u8::{SegRange, Variant};
use crate::hls::video_fetch::VideoClip;
use crate::source::{AudioTrack, Catalog, Channel, Container, Platform, SourceInfo};

#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("file not found: {0}")]
    Missing(PathBuf),
    #[error("not a media file with audio: {0}")]
    NoAudio(PathBuf),
    #[error(transparent)]
    Ffmpeg(#[from] ffmpeg::FfmpegError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Probe {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub has_video: bool,
    /// audio streams in file order
    pub audio_tracks: Vec<AudioTrack>,
}

#[derive(Deserialize)]
struct FfStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    #[serde(default)]
    tags: std::collections::BTreeMap<String, String>,
}
#[derive(Deserialize)]
struct FfFormat {
    duration: Option<String>,
}
#[derive(Deserialize)]
struct FfOut {
    streams: Vec<FfStream>,
    format: FfFormat,
}

pub fn ffprobe_path() -> PathBuf {
    let ff = ffmpeg::ffmpeg_path();
    let name = if cfg!(windows) { "ffprobe.exe" } else { "ffprobe" };
    match ff.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) if dir.join(name).is_file() => dir.join(name),
        _ => PathBuf::from("ffprobe"),
    }
}

pub fn probe(path: &Path) -> Result<Probe, LocalError> {
    if !path.is_file() {
        return Err(LocalError::Missing(path.to_path_buf()));
    }
    let out = std::process::Command::new(ffprobe_path())
        .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type,codec_name,width,height,channels:stream_tags=title,language", "-of", "json"])
        .arg(path)
        .output()
        .map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { ffmpeg::FfmpegError::Missing } else { e.into() })?;
    let v: FfOut = serde_json::from_slice(&out.stdout).map_err(|_| LocalError::NoAudio(path.to_path_buf()))?;
    let video = v.streams.iter().find(|s| s.codec_type.as_deref() == Some("video"));
    let tag = |s: &FfStream, k: &str| s.tags.iter().find(|(t, _)| t.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone()).filter(|v| !v.is_empty());
    let audio_tracks: Vec<AudioTrack> = v
        .streams
        .iter()
        .filter(|s| s.codec_type.as_deref() == Some("audio"))
        .enumerate()
        .map(|(i, s)| AudioTrack {
            index: i as u32,
            codec: s.codec_name.clone().unwrap_or_default(),
            channels: s.channels.unwrap_or(0),
            title: tag(s, "title"),
            language: tag(s, "language").filter(|l| l != "und"),
        })
        .collect();
    if audio_tracks.is_empty() {
        return Err(LocalError::NoAudio(path.to_path_buf()));
    }
    Ok(Probe {
        duration: v.format.duration.and_then(|d| d.parse().ok()).unwrap_or(0.0),
        width: video.and_then(|s| s.width).unwrap_or(0),
        height: video.and_then(|s| s.height).unwrap_or(0),
        has_video: video.is_some(),
        audio_tracks,
    })
}

/// Is `input` an existing local file (rather than a VOD reference)?
pub fn is_local(input: &str) -> bool {
    let p = Path::new(input.trim().trim_matches('"'));
    p.is_absolute() && p.is_file()
}

pub fn normalize(input: &str) -> PathBuf {
    PathBuf::from(input.trim().trim_matches('"'))
}

/// Stable project id for a file: `local-<stem>-<FNV-1a of the lower-cased path>`
/// (stable across toolchains; a growing recording keeps its id).
pub fn project_id(path: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.to_string_lossy().to_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let stem: String = path
        .file_stem()
        .map(|s| s.to_string_lossy().chars().filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_').take(32).collect())
        .unwrap_or_default();
    format!("local-{}-{:08x}", if stem.is_empty() { "file".into() } else { stem }, h as u32)
}

pub fn source_info(path: &Path) -> Result<SourceInfo, LocalError> {
    let pr = probe(path)?;
    let modified = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .map(|t| chrono::DateTime::<chrono::Local>::from(t).format("%Y-%m-%d %H:%M:%S").to_string());
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut notes = Vec::new();
    let mut notices = Vec::new();
    if pr.audio_tracks.len() > 1 {
        notes.push(format!(
            "{} audio tracks; track 1 is used unless settings.audio_track is set (e.g. an OBS mix track)",
            pr.audio_tracks.len()
        ));
    }
    if pr.audio_tracks.len() > 1 {
        notices.push(crate::progress::Note::new("multitrack").arg("tracks", pr.audio_tracks.len()));
    }
    if !pr.has_video {
        notes.push("audio-only file: songs can be exported as audio only".into());
        notices.push(crate::progress::Note::new("audio_only"));
    }
    Ok(SourceInfo {
        platform: Platform::Local,
        video_id: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
        url: path.to_string_lossy().into_owned(),
        title: name,
        channel: String::new(),
        duration: pr.duration,
        container: Container::Fmp4,
        variants: vec![Variant { width: pr.width, height: pr.height, bandwidth: 0, codecs: None, url: path.to_string_lossy().into_owned() }],
        audio_variant: 0,
        catalog: Catalog { published_at: modified, channel: Channel::default(), ..Default::default() },
        notes,
        notices,
        audio_tracks: pr.audio_tracks,
    })
}

/// Copy one audio track out (no re-encode) into a Matroska audio file.
pub fn extract_audio(path: &Path, track: u32, out: &Path) -> Result<(), LocalError> {
    if !path.is_file() {
        return Err(LocalError::Missing(path.to_path_buf()));
    }
    let mut c = ffmpeg::command();
    c.arg("-y").arg("-i").arg(path).args(["-map", &format!("0:a:{track}"), "-vn", "-c:a", "copy"]).arg(out);
    ffmpeg::run(c)?;
    Ok(())
}

/// Cut `[start, end)` of the recording into an MP4 (video stream-copied from the
/// keyframe at/before `start`; audio copied, or AAC if the codec doesn't fit MP4).
pub fn cut_clip(path: &Path, start: f64, end: f64, track: u32, out: &Path, cancel: &tokio_util::sync::CancellationToken) -> Result<VideoClip, LocalError> {
    let pr = probe(path)?;
    let kf = if pr.has_video { keyframe_before(path, start).unwrap_or(start) } else { start };
    let end = if pr.duration > 0.0 { end.min(pr.duration) } else { end };
    let dur = (end - kf).max(0.1);
    let run = |audio_codec: &[&str]| {
        let mut c = ffmpeg::command();
        c.arg("-y").args(["-ss", &format!("{kf:.6}")]).arg("-i").arg(path).args(["-t", &format!("{dur:.6}")]);
        if pr.has_video {
            c.args(["-map", "0:v:0", "-c:v", "copy"]);
        }
        c.args(["-map", &format!("0:a:{track}")]).args(audio_codec).args(["-movflags", "+faststart"]).arg(out);
        ffmpeg::run_cancellable(c, cancel)
    };
    if run(&["-c:a", "copy"]).is_err() && !cancel.is_cancelled() {
        run(&["-c:a", "aac", "-b:a", "320k"])?;
    }
    Ok(VideoClip {
        path: out.to_path_buf(),
        variant: 0,
        resolution: format!("{}x{}", pr.width, pr.height),
        segments: SegRange { first: 0, last: 0, seg_start: kf, trim: start - kf },
        vod_start: kf,
        duration: dur,
        bytes: std::fs::metadata(out).map(|m| m.len()).unwrap_or(0),
        skipped_variants: Vec::new(),
    })
}
