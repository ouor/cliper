//! Reading the segment store with ffmpeg: decode audio across any contiguous
//! stretch of video, remux a stretch into a playable MP4, probe timestamps.
//!
//! A contiguous stretch is a list of [`Piece`]s: raw CDN segments of one
//! rendition, a legacy clip MP4, or the local recording. Several pieces are
//! fed to ffmpeg through the concat demuxer, raw segments through the
//! `concat:` protocol (init + segments byte-concatenated, exactly what an HLS
//! player would see). Decoding a stretch this way is bit-identical to decoding
//! one file holding all of it (the AAC decoder carries over), so audio decoded
//! for different requests always agrees sample for sample (verified on SOOP
//! fMP4 and chzzk TS; see docs/product/cycle2-segment-video-core.md).
//!
//! Time model: every rendition has its own media clock (fMP4 tfdt / TS PTS).
//! `vod = media − m` with one measured `m` per rendition ([`super::store::Align`]).
//! Decoded sample 0 of a stretch is the media time of its first audio packet
//! ([`Probe::audio_start`]); t = 0 of a remux is the earliest packet
//! ([`Probe::format_start`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::store::{self, Probe};
use crate::audio::ffmpeg::{self, FfmpegError, PcmReader};
use crate::source::Container;

/// A contiguous part of a stretch with one physical source.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece {
    /// segments `[first, last)` of rendition `variant` (files in `video/v<variant>/`)
    Raw { variant: usize, first: usize, last: usize },
    /// an adopted clip MP4 holding segments `[first, last)`; file t ↔ VOD `t + origin`
    Legacy { file: PathBuf, variant: usize, first: usize, last: usize, origin: f64 },
    /// a local recording (the whole file; `track` = audio stream)
    Local { file: PathBuf, track: u32 },
}

impl Piece {
    pub fn variant(&self) -> usize {
        match self {
            Piece::Raw { variant, .. } | Piece::Legacy { variant, .. } => *variant,
            Piece::Local { .. } => 0,
        }
    }
    pub fn first(&self) -> usize {
        match self {
            Piece::Raw { first, .. } | Piece::Legacy { first, .. } => *first,
            Piece::Local { .. } => 0,
        }
    }
    pub fn last(&self) -> usize {
        match self {
            Piece::Raw { last, .. } | Piece::Legacy { last, .. } => *last,
            Piece::Local { .. } => 0,
        }
    }
}

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `concat:` URL of init + segments `[first, last)` (absolute paths).
pub fn raw_url(project_dir: &Path, container: Container, variant: usize, first: usize, last: usize) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(last - first + 1);
    if container == Container::Fmp4 {
        parts.push(fwd(&store::init_path(project_dir, variant)));
    }
    for i in first..last {
        parts.push(fwd(&store::segment_path(project_dir, variant, i, container)));
    }
    format!("concat:{}", parts.join("|"))
}

/// A concat-demuxer list file, deleted on drop.
pub struct TempList(PathBuf);

impl Drop for TempList {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// ffmpeg input arguments for a stretch (+ the list file to keep alive while
/// ffmpeg runs). Local recordings select `track` with `-map` by the caller.
pub fn input_args(project_dir: &Path, container: Container, pieces: &[Piece]) -> std::io::Result<(Vec<OsString>, Option<TempList>)> {
    match pieces {
        [Piece::Local { file, .. }] | [Piece::Legacy { file, .. }] => Ok((vec!["-i".into(), file.clone().into_os_string()], None)),
        _ => {
            let mut text = String::from("ffconcat version 1.0\n");
            for p in pieces {
                let url = match p {
                    Piece::Raw { variant, first, last } => raw_url(project_dir, container, *variant, *first, *last),
                    Piece::Legacy { file, .. } | Piece::Local { file, .. } => fwd(file),
                };
                text.push_str(&format!("file {}\n", quote(&url)));
            }
            let dir = project_dir.join(store::DIR);
            std::fs::create_dir_all(&dir)?;
            let list = dir.join(format!("list_{}.ffconcat", crate::project::new_id("")));
            std::fs::write(&list, text)?;
            let args: Vec<OsString> = ["-f", "concat", "-safe", "0", "-protocol_whitelist", "file,concat,pipe", "-i"]
                .into_iter()
                .map(OsString::from)
                .chain(std::iter::once(list.clone().into_os_string()))
                .collect();
            Ok((args, Some(TempList(list))))
        }
    }
}

/// A PCM decoder over a stretch (keeps the list file alive).
pub struct StretchDecoder {
    pub pcm: PcmReader,
    _list: Option<TempList>,
}

/// Decode the audio of a stretch from its first packet on.
pub fn decode(project_dir: &Path, container: Container, pieces: &[Piece], rate: u32, channels: u32) -> Result<StretchDecoder, FfmpegError> {
    let (mut args, list) = input_args(project_dir, container, pieces)?;
    let track = match pieces.first() {
        Some(Piece::Local { track, .. }) => *track,
        _ => 0,
    };
    args.extend(["-map".into(), format!("0:a:{track}").into()]);
    Ok(StretchDecoder { pcm: ffmpeg::decode_input(&args, rate, channels)?, _list: list })
}

/// Remux a single-rendition stretch (video + first audio) into a playable MP4.
pub fn remux(project_dir: &Path, container: Container, pieces: &[Piece], out: &Path, cancel: &tokio_util::sync::CancellationToken) -> Result<(), FfmpegError> {
    let (args, _list) = input_args(project_dir, container, pieces)?;
    let tmp = out.with_extension("part.mp4");
    let mut c = ffmpeg::command();
    c.arg("-y").args(&args).args(["-map", "0:v:0", "-map", "0:a:0?", "-c", "copy", "-movflags", "+faststart"]).arg(&tmp);
    let r = ffmpeg::run_cancellable(c, cancel);
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return r;
    }
    std::fs::rename(&tmp, out)?;
    Ok(())
}

#[derive(serde::Deserialize)]
struct PStream {
    codec_type: Option<String>,
    start_time: Option<String>,
}
#[derive(serde::Deserialize)]
struct PFormat {
    start_time: Option<String>,
}
#[derive(serde::Deserialize)]
struct POut {
    #[serde(default)]
    streams: Vec<PStream>,
    format: Option<PFormat>,
}

/// Start times of an input (a file path or a `concat:` URL).
pub fn probe_input(input: &str) -> Result<Probe, FfmpegError> {
    let mut c = std::process::Command::new(crate::local::ffprobe_path());
    c.args(["-v", "error", "-show_entries", "stream=codec_type,start_time:format=start_time", "-of", "json", "-i", input]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let out = c.output().map_err(|e| if e.kind() == std::io::ErrorKind::NotFound { FfmpegError::Missing } else { e.into() })?;
    let o: POut = serde_json::from_slice(&out.stdout).map_err(|_| FfmpegError::Failed(format!("ffprobe: {}", String::from_utf8_lossy(&out.stderr).trim())))?;
    let num = |s: &Option<String>| s.as_deref().and_then(|x| x.parse::<f64>().ok());
    let starts: Vec<f64> = o.streams.iter().filter_map(|s| num(&s.start_time)).collect();
    let format_start = o.format.as_ref().and_then(|f| num(&f.start_time)).or_else(|| starts.iter().copied().reduce(f64::min)).unwrap_or(0.0);
    let audio_start = o.streams.iter().find(|s| s.codec_type.as_deref() == Some("audio")).and_then(|s| num(&s.start_time)).unwrap_or(format_start);
    Ok(Probe { format_start, audio_start })
}

/// Height of a file's first video stream.
pub fn video_height(file: &Path) -> Option<u32> {
    let mut c = std::process::Command::new(crate::local::ffprobe_path());
    c.args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=height", "-of", "csv=p=0"]).arg(file);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    String::from_utf8_lossy(&c.output().ok()?.stdout).trim().trim_end_matches(',').parse().ok()
}

/// Probe raw segment `index` of `variant` (init + that segment).
pub fn probe_segment(project_dir: &Path, container: Container, variant: usize, index: usize) -> Result<Probe, FfmpegError> {
    probe_input(&raw_url(project_dir, container, variant, index, index + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_quoting() {
        assert_eq!(quote("C:/a'b/c.mp4"), r"'C:/a'\''b/c.mp4'");
        let u = raw_url(Path::new("C:\\p"), Container::Fmp4, 3, 7, 9);
        assert_eq!(u, "concat:C:/p/video/v3/init.mp4|C:/p/video/v3/000007.m4s|C:/p/video/v3/000008.m4s");
        let u = raw_url(Path::new("C:\\p"), Container::Ts, 0, 7, 8);
        assert_eq!(u, "concat:C:/p/video/v0/000007.ts");
    }
}
