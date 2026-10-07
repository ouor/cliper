//! Minimal fragmented-MP4 (CMAF / fMP4) box parsing — pure, no I/O.
//!
//! Port of `stream_audio/mp4.py`: box walking, `tfhd`/`trun` sample tables to
//! locate each track's bytes inside a media segment, `track_id -> handler` from
//! the init segment, and the audio `AudioSpecificConfig` for ADTS framing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoxRef {
    pub start: usize,
    pub header: usize,
    pub size: usize,
}

impl BoxRef {
    pub fn body(&self) -> usize {
        self.start + self.header
    }
    pub fn end(&self) -> usize {
        self.start + self.size
    }
}

fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}
fn be64(b: &[u8], o: usize) -> Option<u64> {
    b.get(o..o + 8).map(|s| u64::from_be_bytes(s.try_into().unwrap()))
}

/// Boxes in `[start, end)` as `(fourcc, BoxRef)`. Stops at a malformed header.
pub fn iter_boxes(buf: &[u8], start: usize, end: usize) -> Vec<([u8; 4], BoxRef)> {
    let end = end.min(buf.len());
    let mut out = Vec::new();
    let mut o = start;
    while o + 8 <= end {
        let mut size = be32(buf, o).unwrap() as usize;
        let typ: [u8; 4] = buf[o + 4..o + 8].try_into().unwrap();
        let mut hs = 8;
        if size == 1 {
            match be64(buf, o + 8) {
                Some(s) => {
                    size = s as usize;
                    hs = 16;
                }
                None => break,
            }
        } else if size == 0 {
            size = end - o;
        }
        if size < hs {
            break;
        }
        out.push((typ, BoxRef { start: o, header: hs, size }));
        o += size;
    }
    out
}

/// Depth-first search for a nested box path, e.g. `[b"moof", b"traf"]`.
pub fn find_box(buf: &[u8], path: &[&[u8; 4]], start: usize, end: usize) -> Option<BoxRef> {
    let (target, rest) = path.split_first()?;
    for (typ, b) in iter_boxes(buf, start, end) {
        if &typ == *target {
            if rest.is_empty() {
                return Some(b);
            }
            if let Some(sub) = find_box(buf, rest, b.body(), b.end()) {
                return Some(sub);
            }
        }
    }
    None
}

pub fn all_boxes(buf: &[u8], typ: &[u8; 4], start: usize, end: usize) -> Vec<BoxRef> {
    iter_boxes(buf, start, end)
        .into_iter()
        .filter(|(t, _)| t == typ)
        .map(|(_, b)| b)
        .collect()
}

/// One `trun` sample run: the byte region of one track inside a segment.
#[derive(Debug, Clone)]
pub struct Run {
    pub track_id: u32,
    /// absolute byte offset of the run's data
    pub start: u64,
    pub sizes: Vec<u32>,
}

impl Run {
    pub fn nbytes(&self) -> u64 {
        self.sizes.iter().map(|&s| s as u64).sum()
    }
    pub fn end(&self) -> u64 {
        self.start + self.nbytes()
    }
    /// Slice this run's samples out of `data` (which starts at absolute `data_base`).
    pub fn split(&self, data: &[u8], data_base: u64) -> Option<Vec<Vec<u8>>> {
        let mut out = Vec::with_capacity(self.sizes.len());
        let mut off = (self.start.checked_sub(data_base)?) as usize;
        for &sz in &self.sizes {
            out.push(data.get(off..off + sz as usize)?.to_vec());
            off += sz as usize;
        }
        Some(out)
    }
}

struct Tfhd {
    track_id: u32,
    base_data_offset: Option<u64>,
    default_sample_size: u32,
    default_base_is_moof: bool,
}

fn parse_tfhd(buf: &[u8], b: BoxRef) -> Option<Tfhd> {
    let mut p = b.body();
    let flags = be32(buf, p)? & 0x00FF_FFFF;
    p += 4;
    let track_id = be32(buf, p)?;
    p += 4;
    let mut base_data_offset = None;
    let mut default_sample_size = 0;
    if flags & 0x01 != 0 {
        base_data_offset = Some(be64(buf, p)?);
        p += 8;
    }
    if flags & 0x02 != 0 {
        p += 4;
    }
    if flags & 0x08 != 0 {
        p += 4;
    }
    if flags & 0x10 != 0 {
        default_sample_size = be32(buf, p)?;
    }
    Some(Tfhd {
        track_id,
        base_data_offset,
        default_sample_size,
        default_base_is_moof: flags & 0x020000 != 0,
    })
}

fn parse_trun(buf: &[u8], b: BoxRef, default_sample_size: u32) -> Option<(Option<i32>, Vec<u32>)> {
    let mut p = b.body();
    let flags = be32(buf, p)? & 0x00FF_FFFF;
    p += 4;
    let count = be32(buf, p)? as usize;
    p += 4;
    let mut data_offset = None;
    if flags & 0x01 != 0 {
        data_offset = Some(be32(buf, p)? as i32);
        p += 4;
    }
    if flags & 0x04 != 0 {
        p += 4;
    }
    if count > 1_000_000 {
        return None;
    }
    let mut sizes = Vec::with_capacity(count);
    for _ in 0..count {
        if flags & 0x100 != 0 {
            p += 4;
        }
        if flags & 0x200 != 0 {
            sizes.push(be32(buf, p)?);
            p += 4;
        } else {
            sizes.push(default_sample_size);
        }
        if flags & 0x400 != 0 {
            p += 4;
        }
        if flags & 0x800 != 0 {
            p += 4;
        }
    }
    Some((data_offset, sizes))
}

/// Parse the `moof` at `buf[moof_local..]` whose absolute offset is
/// `base_abs + moof_local`. Returns the runs and the moof box, or `None`.
pub fn runs_in_moof(buf: &[u8], moof_local: usize, base_abs: u64) -> Option<(Vec<Run>, BoxRef)> {
    let m = find_box(buf, &[b"moof"], moof_local, buf.len())?;
    if m.end() > buf.len() {
        return None;
    }
    let moof_abs = base_abs + m.start as u64;
    let mut runs = Vec::new();
    for traf in all_boxes(buf, b"traf", m.body(), m.end()) {
        let tf = find_box(buf, &[b"tfhd"], traf.body(), traf.end())?;
        let t = parse_tfhd(buf, tf)?;
        for tr in all_boxes(buf, b"trun", traf.body(), traf.end()) {
            let (doff, sizes) = parse_trun(buf, tr, t.default_sample_size)?;
            let anchor = if t.default_base_is_moof { moof_abs } else { t.base_data_offset.unwrap_or(0) };
            let start = (anchor as i64 + doff.unwrap_or(0) as i64).max(0) as u64;
            runs.push(Run { track_id: t.track_id, start, sizes });
        }
    }
    Some((runs, m))
}

/// All runs of a complete media segment, in file order (handles N fragments).
pub fn iter_segment_runs(seg: &[u8]) -> Vec<Run> {
    let mut runs = Vec::new();
    for (typ, b) in iter_boxes(seg, 0, seg.len()) {
        if &typ == b"moof"
            && let Some((r, _)) = runs_in_moof(seg, b.start, 0) {
                runs.extend(r);
            }
    }
    runs
}

/// `track_id -> handler` (`soun` audio, `vide` video) from the init `moov`.
pub fn parse_init_tracks(init: &[u8]) -> Vec<(u32, [u8; 4])> {
    let mut out = Vec::new();
    let Some(mo) = find_box(init, &[b"moov"], 0, init.len()) else { return out };
    for trak in all_boxes(init, b"trak", mo.body(), mo.end()) {
        let tid = find_box(init, &[b"tkhd"], trak.body(), trak.end()).and_then(|tk| {
            let ver = *init.get(tk.body())?;
            be32(init, tk.body() + if ver == 1 { 20 } else { 12 })
        });
        let handler = find_box(init, &[b"mdia", b"hdlr"], trak.body(), trak.end())
            .and_then(|h| init.get(h.body() + 8..h.body() + 12))
            .map(|s| <[u8; 4]>::try_from(s).unwrap());
        if let (Some(t), Some(h)) = (tid, handler) {
            out.push((t, h));
        }
    }
    out
}

pub fn audio_track_id(init: &[u8]) -> Option<u32> {
    parse_init_tracks(init).into_iter().find(|(_, h)| h == b"soun").map(|(t, _)| t)
}

/// AAC sampling-frequency table (AudioSpecificConfig index -> Hz).
pub const SAMPLE_RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

/// Parameters needed to wrap raw AAC access units in ADTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AudioConfig {
    /// AudioObjectType (AAC-LC = 2)
    pub aot: u8,
    pub freq_index: u8,
    pub channels: u8,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self { aot: 2, freq_index: 3, channels: 2 }
    }
}

impl AudioConfig {
    pub fn sample_rate(&self) -> u32 {
        SAMPLE_RATES.get(self.freq_index as usize).copied().unwrap_or(0)
    }

    /// 7-byte ADTS header (no CRC) for one AAC access unit of `payload_len` bytes.
    pub fn adts_header(&self, payload_len: usize) -> [u8; 7] {
        let flen = payload_len + 7;
        let profile = self.aot.saturating_sub(1);
        [
            0xFF,
            0xF1,
            (profile << 6) | (self.freq_index << 2) | (self.channels >> 2),
            ((self.channels & 3) << 6) | ((flen >> 11) & 3) as u8,
            ((flen >> 3) & 0xFF) as u8,
            (((flen & 7) << 5) | 0x1F) as u8,
            0xFC,
        ]
    }
}

fn read_desc_len(buf: &[u8], mut p: usize) -> Option<(usize, usize)> {
    let mut len = 0usize;
    for _ in 0..4 {
        let b = *buf.get(p)?;
        p += 1;
        len = (len << 7) | (b & 0x7F) as usize;
        if b & 0x80 == 0 {
            break;
        }
    }
    Some((len, p))
}

/// Audio `esds` AudioSpecificConfig from the init segment; AAC-LC/48k/stereo fallback.
pub fn parse_audio_config(init: &[u8]) -> AudioConfig {
    fn inner(init: &[u8]) -> Option<AudioConfig> {
        let idx = init.windows(4).position(|w| w == b"esds")?;
        let mut p = idx + 8;
        if *init.get(p)? == 0x03 {
            p += 1;
            p = read_desc_len(init, p)?.1;
            p += 2;
            let f = *init.get(p)?;
            p += 1;
            if f & 0x80 != 0 {
                p += 2;
            }
            if f & 0x40 != 0 {
                p += 1 + *init.get(p)? as usize;
            }
            if f & 0x20 != 0 {
                p += 2;
            }
        }
        if *init.get(p)? == 0x04 {
            p += 1;
            p = read_desc_len(init, p)?.1;
            p += 13;
        }
        if *init.get(p)? == 0x05 {
            p += 1;
            let (_, q) = read_desc_len(init, p)?;
            let bits = u16::from_be_bytes([*init.get(q)?, *init.get(q + 1)?]);
            let aot = ((bits >> 11) & 0x1F) as u8;
            let fi = ((bits >> 7) & 0x0F) as u8;
            let ch = ((bits >> 3) & 0x0F) as u8;
            if aot != 0 && (fi as usize) < SAMPLE_RATES.len() && ch != 0 {
                return Some(AudioConfig { aot, freq_index: fi, channels: ch });
            }
        }
        None
    }
    inner(init).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(body);
        v
    }

    /// Synthetic 2-track fragment: moof (default-base-is-moof) + mdat [audio | video].
    fn fragment(audio: &[&[u8]], video: &[u8]) -> Vec<u8> {
        let mk_traf = |tid: u32, sizes: &[u32], doff: i32| {
            let mut tfhd = vec![0, 0x02, 0, 0];
            tfhd.extend(tid.to_be_bytes());
            let mut trun = vec![0, 0, 0x02, 0x01];
            trun.extend((sizes.len() as u32).to_be_bytes());
            trun.extend(doff.to_be_bytes());
            for s in sizes {
                trun.extend(s.to_be_bytes());
            }
            let mut b = bx(b"tfhd", &tfhd);
            b.extend(bx(b"trun", &trun));
            bx(b"traf", &b)
        };
        let asz: Vec<u32> = audio.iter().map(|a| a.len() as u32).collect();
        let atotal: u32 = asz.iter().sum();
        // moof size is fixed for given sample counts; compute with placeholder offsets
        let probe = {
            let mut t = mk_traf(1, &asz, 0);
            t.extend(mk_traf(2, &[video.len() as u32], 0));
            bx(b"moof", &t)
        };
        let data_start = probe.len() as i32 + 8;
        let mut t = mk_traf(1, &asz, data_start);
        t.extend(mk_traf(2, &[video.len() as u32], data_start + atotal as i32));
        let mut out = bx(b"moof", &t);
        let mut mdat = Vec::new();
        for a in audio {
            mdat.extend_from_slice(a);
        }
        mdat.extend_from_slice(video);
        out.extend(bx(b"mdat", &mdat));
        out
    }

    #[test]
    fn runs_locate_audio_bytes() {
        let a1 = [1u8; 10];
        let a2 = [2u8; 7];
        let mut seg = fragment(&[&a1, &a2], &[9u8; 100]);
        seg.extend(fragment(&[&[3u8; 5]], &[8u8; 50]));
        let runs = iter_segment_runs(&seg);
        let audio: Vec<Vec<u8>> = runs.iter().filter(|r| r.track_id == 1).flat_map(|r| r.split(&seg, 0).unwrap()).collect();
        assert_eq!(audio, vec![a1.to_vec(), a2.to_vec(), vec![3u8; 5]]);
    }

    #[test]
    fn adts_header_roundtrip() {
        let cfg = AudioConfig { aot: 2, freq_index: 3, channels: 2 };
        let h = cfg.adts_header(100);
        let flen = (((h[3] & 3) as usize) << 11) | ((h[4] as usize) << 3) | ((h[5] >> 5) as usize);
        assert_eq!(flen, 107);
        assert_eq!((h[2] >> 2) & 0xF, 3);
    }
}
