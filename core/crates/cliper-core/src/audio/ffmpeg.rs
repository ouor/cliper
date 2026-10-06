//! ffmpeg subprocess helpers. ffmpeg is the one external tool the engine drives:
//! decoding (AAC/TS/MP4 → PCM) and video cut/mux. Location: `$CLIPER_FFMPEG`,
//! `<exe dir>/ffmpeg(.exe)`, then `PATH`.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum FfmpegError {
    #[error("ffmpeg not found (set CLIPER_FFMPEG or put ffmpeg on PATH)")]
    Missing,
    #[error("ffmpeg failed: {0}")]
    Failed(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn ffmpeg_path() -> PathBuf {
    if let Some(p) = std::env::var_os("CLIPER_FFMPEG") {
        return PathBuf::from(p);
    }
    let exe_name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        let p = dir.join(exe_name);
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("ffmpeg")
}

pub fn command() -> Command {
    let mut c = Command::new(ffmpeg_path());
    c.args(["-hide_banner", "-nostdin", "-v", "error"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// Run to completion; stderr tail on failure.
pub fn run(mut cmd: Command) -> Result<(), FfmpegError> {
    let out = cmd.stdout(Stdio::null()).stderr(Stdio::piped()).output().map_err(map_spawn)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: String = err.chars().rev().take(800).collect::<Vec<_>>().into_iter().rev().collect();
        return Err(FfmpegError::Failed(tail));
    }
    Ok(())
}

fn map_spawn(e: std::io::Error) -> FfmpegError {
    if e.kind() == std::io::ErrorKind::NotFound { FfmpegError::Missing } else { FfmpegError::Io(e) }
}

/// Streaming f32le PCM decoder over an ffmpeg child process.
pub struct PcmReader {
    child: Child,
    channels: usize,
    buf: Vec<u8>,
}

/// Decode `input` to interleaved f32 at `sample_rate`/`channels`, optionally
/// trimmed to `[start, start+duration)` seconds.
pub fn decode(input: &Path, sample_rate: u32, channels: u32, start: Option<f64>, duration: Option<f64>) -> Result<PcmReader, FfmpegError> {
    let mut c = command();
    if let Some(s) = start.filter(|s| *s > 0.0) {
        c.args(["-ss", &format!("{s:.6}")]);
    }
    c.arg("-i").arg(input);
    if let Some(d) = duration {
        c.args(["-t", &format!("{d:.6}")]);
    }
    c.args(["-vn", "-ac", &channels.to_string(), "-ar", &sample_rate.to_string(), "-f", "f32le", "-"]);
    let child = c.stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(map_spawn)?;
    Ok(PcmReader { child, channels: channels as usize, buf: Vec::new() })
}

impl PcmReader {
    /// Fill `out` with up to `out.len()` samples (interleaved); returns count read (0 = EOF).
    pub fn read(&mut self, out: &mut [f32]) -> Result<usize, FfmpegError> {
        let want = out.len() * 4;
        self.buf.resize(want, 0);
        let stdout = self.child.stdout.as_mut().expect("piped");
        let mut got = 0;
        while got < want {
            let n = stdout.read(&mut self.buf[got..want])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        let n = got / 4;
        for (i, ch) in self.buf[..n * 4].chunks_exact(4).enumerate() {
            out[i] = f32::from_le_bytes(ch.try_into().unwrap());
        }
        Ok(n)
    }

    /// Read everything (only for short inputs).
    pub fn read_all(mut self) -> Result<Vec<f32>, FfmpegError> {
        let mut bytes = Vec::new();
        self.child.stdout.as_mut().expect("piped").read_to_end(&mut bytes)?;
        self.finish()?;
        Ok(bytes.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
    }

    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn finish(mut self) -> Result<(), FfmpegError> {
        let st = self.child.wait()?;
        if !st.success() {
            return Err(FfmpegError::Failed(format!("decoder exited with {st}")));
        }
        Ok(())
    }
}

impl Drop for PcmReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
