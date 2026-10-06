//! Minimal WAV I/O: 32-bit float interleaved (IEEE_FLOAT) — the stem format the
//! engine writes and the UI loads.

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::Path;

pub fn write_f32(path: &Path, sample_rate: u32, channels: u16, interleaved: &[f32]) -> std::io::Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, File::create(path)?);
    let data_len = (interleaved.len() * 4) as u32;
    w.write_all(b"RIFF")?;
    w.write_all(&(36 + data_len).to_le_bytes())?;
    w.write_all(b"WAVEfmt ")?;
    w.write_all(&16u32.to_le_bytes())?;
    w.write_all(&3u16.to_le_bytes())?; // IEEE float
    w.write_all(&channels.to_le_bytes())?;
    w.write_all(&sample_rate.to_le_bytes())?;
    w.write_all(&(sample_rate * channels as u32 * 4).to_le_bytes())?;
    w.write_all(&(channels * 4).to_le_bytes())?;
    w.write_all(&32u16.to_le_bytes())?;
    w.write_all(b"data")?;
    w.write_all(&data_len.to_le_bytes())?;
    for s in interleaved {
        w.write_all(&s.to_le_bytes())?;
    }
    w.flush()
}

pub struct Wav {
    pub sample_rate: u32,
    pub channels: u16,
    pub samples: Vec<f32>,
}

/// Read a float32 WAV written by [`write_f32`] (or any IEEE_FLOAT 32-bit WAV).
pub fn read_f32(path: &Path) -> std::io::Result<Wav> {
    let mut b = Vec::new();
    BufReader::new(File::open(path)?).read_to_end(&mut b)?;
    let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not a float32 WAV");
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WAVE" {
        return Err(bad());
    }
    let (mut p, mut fmt) = (12usize, None);
    while p + 8 <= b.len() {
        let id = &b[p..p + 4];
        let sz = u32::from_le_bytes(b[p + 4..p + 8].try_into().unwrap()) as usize;
        let body = p + 8;
        if id == b"fmt " {
            let tag = u16::from_le_bytes([b[body], b[body + 1]]);
            let ch = u16::from_le_bytes([b[body + 2], b[body + 3]]);
            let sr = u32::from_le_bytes(b[body + 4..body + 8].try_into().unwrap());
            let bits = u16::from_le_bytes([b[body + 14], b[body + 15]]);
            if !(tag == 3 || tag == 0xFFFE) || bits != 32 {
                return Err(bad());
            }
            fmt = Some((sr, ch));
        } else if id == b"data" {
            let (sr, ch) = fmt.ok_or_else(bad)?;
            let end = (body + sz).min(b.len());
            let samples = b[body..end].chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap())).collect();
            return Ok(Wav { sample_rate: sr, channels: ch, samples });
        }
        p = body + sz + (sz & 1);
    }
    Err(bad())
}
