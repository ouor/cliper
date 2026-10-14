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

/// Like [`run`], but kills ffmpeg when `cancel` fires.
pub fn run_cancellable(mut cmd: Command, cancel: &tokio_util::sync::CancellationToken) -> Result<(), FfmpegError> {
    let mut child = cmd.stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(map_spawn)?;
    let stderr = drain_stderr(&mut child);
    loop {
        if let Some(st) = child.try_wait()? {
            if !st.success() {
                let tail = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
                let tail: String = tail.chars().rev().take(800).collect::<Vec<_>>().into_iter().rev().collect();
                return Err(FfmpegError::Failed(tail));
            }
            return Ok(());
        }
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(FfmpegError::Failed("cancelled".into()));
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

fn map_spawn(e: std::io::Error) -> FfmpegError {
    if e.kind() == std::io::ErrorKind::NotFound { FfmpegError::Missing } else { FfmpegError::Io(e) }
}

/// Streaming f32le PCM decoder over an ffmpeg child process. Reaching the end
/// of the stream checks ffmpeg's exit status, so a decoder that dies mid-file
/// is an error rather than a silently short result.
pub struct PcmReader {
    child: Child,
    channels: usize,
    buf: Vec<u8>,
    stderr: Option<std::thread::JoinHandle<String>>,
    finished: bool,
}

/// Collect the last few KB of a child's stderr on a thread (never blocks the child).
fn drain_stderr(child: &mut Child) -> Option<std::thread::JoinHandle<String>> {
    let mut err = child.stderr.take()?;
    Some(std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        while let Ok(n) = err.read(&mut buf) {
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 8192 {
                tail.drain(..tail.len() - 4096);
            }
        }
        String::from_utf8_lossy(&tail).into_owned()
    }))
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
    spawn_pcm(c, channels)
}

/// Decode with custom input arguments (`-f concat … -i list`, `-map …`):
/// `input` goes before the output options.
pub fn decode_input(input: &[std::ffi::OsString], sample_rate: u32, channels: u32) -> Result<PcmReader, FfmpegError> {
    let mut c = command();
    c.args(input);
    c.args(["-vn", "-ac", &channels.to_string(), "-ar", &sample_rate.to_string(), "-f", "f32le", "-"]);
    spawn_pcm(c, channels)
}

fn spawn_pcm(mut c: Command, channels: u32) -> Result<PcmReader, FfmpegError> {
    let mut child = c.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(map_spawn)?;
    let stderr = drain_stderr(&mut child);
    Ok(PcmReader { child, channels: channels as usize, buf: Vec::new(), stderr, finished: false })
}

/// Decode an in-memory stream (e.g. a slice of ADTS frames, `input_format`
/// "aac") to interleaved f32, dropping the first `skip` seconds and keeping
/// `duration` seconds. Blocking.
pub fn decode_bytes(bytes: Vec<u8>, input_format: &str, sample_rate: u32, channels: u32, skip: f64, duration: f64) -> Result<Vec<f32>, FfmpegError> {
    use std::io::Write;
    let mut c = Command::new(ffmpeg_path());
    c.args(["-hide_banner", "-v", "error", "-f", input_format, "-i", "pipe:0"]);
    if skip > 0.0 {
        c.args(["-ss", &format!("{skip:.6}")]);
    }
    c.args(["-t", &format!("{duration:.6}"), "-vn", "-ac", &channels.to_string(), "-ar", &sample_rate.to_string(), "-f", "f32le", "pipe:1"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    let mut child = c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(map_spawn)?;
    let mut stdin = child.stdin.take().expect("piped");
    let feeder = std::thread::spawn(move || {
        let _ = stdin.write_all(&bytes);
    });
    let stderr = drain_stderr(&mut child);
    let mut out = Vec::new();
    child.stdout.take().expect("piped").read_to_end(&mut out)?;
    let _ = feeder.join();
    let st = child.wait()?;
    if !st.success() {
        let tail = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
        return Err(FfmpegError::Failed(format!("decoder exited with {st}: {}", tail.trim())));
    }
    Ok(out.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect())
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
        if n == 0 && !self.finished {
            // end of stream: did the decoder finish cleanly?
            self.finished = true;
            self.check_exit()?;
        }
        Ok(n)
    }

    fn check_exit(&mut self) -> Result<(), FfmpegError> {
        let st = self.child.wait()?;
        if !st.success() {
            let tail = self.stderr.take().and_then(|h| h.join().ok()).unwrap_or_default();
            return Err(FfmpegError::Failed(format!("decoder exited with {st}: {}", tail.trim())));
        }
        Ok(())
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
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.check_exit()
    }
}

impl Drop for PcmReader {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
