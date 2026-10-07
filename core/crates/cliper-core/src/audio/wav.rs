//! Minimal WAV I/O: 32-bit float interleaved (IEEE_FLOAT) — the stem format the
//! engine writes and the UI loads. Writing streams (constant memory); reading
//! can take just a range of frames. Files over 4 GiB (the RIFF size limit) are
//! refused rather than written with a wrapped header.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

fn too_big() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "WAV data larger than 4 GiB (clip too long)")
}

fn header(sample_rate: u32, channels: u16, data_len: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    h[..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(36u32.saturating_add(data_len)).to_le_bytes());
    h[8..16].copy_from_slice(b"WAVEfmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&3u16.to_le_bytes()); // IEEE float
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    h[28..32].copy_from_slice(&(sample_rate * channels as u32 * 4).to_le_bytes());
    h[32..34].copy_from_slice(&(channels * 4).to_le_bytes());
    h[34..36].copy_from_slice(&32u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data_len.to_le_bytes());
    h
}

/// Streaming float32 WAV writer; sizes are patched in by [`WavWriter::finish`].
pub struct WavWriter {
    w: BufWriter<File>,
    sample_rate: u32,
    channels: u16,
    bytes: u64,
}

impl WavWriter {
    pub fn create(path: &Path, sample_rate: u32, channels: u16) -> std::io::Result<Self> {
        let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
        w.write_all(&header(sample_rate, channels, 0))?;
        Ok(Self { w, sample_rate, channels, bytes: 0 })
    }

    pub fn write(&mut self, interleaved: &[f32]) -> std::io::Result<()> {
        self.bytes += interleaved.len() as u64 * 4;
        if self.bytes > (u32::MAX - 36) as u64 {
            return Err(too_big());
        }
        // one copy into a byte buffer is much faster than per-sample writes
        let mut b = Vec::with_capacity(interleaved.len() * 4);
        for s in interleaved {
            b.extend_from_slice(&s.to_le_bytes());
        }
        self.w.write_all(&b)
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.bytes / 4 / self.channels as u64
    }

    /// Patch the header; returns the frame count.
    pub fn finish(mut self) -> std::io::Result<u64> {
        let frames = self.frames();
        self.w.flush()?;
        let mut f = self.w.into_inner().map_err(|e| e.into_error())?;
        f.seek(SeekFrom::Start(0))?;
        f.write_all(&header(self.sample_rate, self.channels, self.bytes as u32))?;
        f.sync_all()?;
        Ok(frames)
    }
}

pub fn write_f32(path: &Path, sample_rate: u32, channels: u16, interleaved: &[f32]) -> std::io::Result<()> {
    let mut w = WavWriter::create(path, sample_rate, channels)?;
    w.write(interleaved)?;
    w.finish().map(|_| ())
}

pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

/// Format and data location of a float32 WAV.
struct Layout {
    sample_rate: u32,
    channels: u16,
    data_start: u64,
    data_len: u64,
}

fn layout(f: &mut File) -> std::io::Result<Layout> {
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a float32 WAV");
    let file_len = f.metadata()?.len();
    let mut r = BufReader::new(&mut *f);
    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|_| bad())?;
    if &head[..4] != b"RIFF" || &head[8..12] != b"WAVE" {
        return Err(bad());
    }
    let mut pos = 12u64;
    let mut fmt = None;
    loop {
        let mut ch = [0u8; 8];
        if r.read_exact(&mut ch).is_err() {
            return Err(bad());
        }
        let sz = u32::from_le_bytes(ch[4..8].try_into().unwrap()) as u64;
        let body = pos + 8;
        if &ch[..4] == b"fmt " {
            if sz < 16 {
                return Err(bad());
            }
            let mut b = vec![0u8; sz as usize];
            r.read_exact(&mut b).map_err(|_| bad())?;
            let tag = u16::from_le_bytes([b[0], b[1]]);
            let channels = u16::from_le_bytes([b[2], b[3]]);
            let sample_rate = u32::from_le_bytes(b[4..8].try_into().unwrap());
            let bits = u16::from_le_bytes([b[14], b[15]]);
            if !(tag == 3 || tag == 0xFFFE) || bits != 32 || channels == 0 {
                return Err(bad());
            }
            fmt = Some((sample_rate, channels));
            if sz & 1 == 1 {
                r.seek_relative(1)?;
            }
        } else if &ch[..4] == b"data" {
            let (sample_rate, channels) = fmt.ok_or_else(bad)?;
            let data_len = sz.min(file_len.saturating_sub(body));
            return Ok(Layout { sample_rate, channels, data_start: body, data_len });
        } else {
            r.seek_relative((sz + (sz & 1)) as i64)?;
        }
        pos = body + sz + (sz & 1);
    }
}

/// Read a whole float32 WAV.
pub fn read_f32(path: &Path) -> std::io::Result<Wav> {
    read_f32_range(path, 0, u64::MAX)
}

/// Read up to `frames` frames starting at frame `start` (clamped to the file).
pub fn read_f32_range(path: &Path, start: u64, frames: u64) -> std::io::Result<Wav> {
    let mut f = File::open(path)?;
    let l = layout(&mut f)?;
    let frame_bytes = l.channels as u64 * 4;
    let total = l.data_len / frame_bytes;
    let a = start.min(total);
    let n = frames.min(total - a);
    f.seek(SeekFrom::Start(l.data_start + a * frame_bytes))?;
    let mut b = vec![0u8; (n * frame_bytes) as usize];
    f.read_exact(&mut b)?;
    let samples = b.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
    Ok(Wav { sample_rate: l.sample_rate, channels: l.channels, samples })
}

/// Sample rate, channels and frame count without reading the data.
pub fn info(path: &Path) -> std::io::Result<(u32, u16, u64)> {
    let mut f = File::open(path)?;
    let l = layout(&mut f)?;
    Ok((l.sample_rate, l.channels, l.data_len / (l.channels as u64 * 4)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_range() {
        let dir = std::env::temp_dir().join(format!("cliper-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.wav");
        let x: Vec<f32> = (0..2000).map(|i| i as f32 / 2000.0).collect();
        let mut w = WavWriter::create(&p, 48000, 2).unwrap();
        w.write(&x[..600]).unwrap();
        w.write(&x[600..]).unwrap();
        assert_eq!(w.finish().unwrap(), 1000);
        assert_eq!(read_f32(&p).unwrap().samples, x);
        let r = read_f32_range(&p, 100, 10).unwrap();
        assert_eq!(r.samples, x[200..220]);
        assert_eq!(read_f32_range(&p, 995, 100).unwrap().samples.len(), 10);
        assert_eq!(info(&p).unwrap(), (48000, 2, 1000));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
