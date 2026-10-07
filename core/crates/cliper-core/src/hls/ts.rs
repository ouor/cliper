//! Minimal MPEG-TS demuxer — pull the AAC (ADTS) audio stream out of a segment.
//!
//! Port of `stream_audio/ts.py`. chzzk general VODs ship as MPEG-TS; audio is a
//! dedicated ADTS PID interleaved packet-by-packet with video, so the whole
//! segment is fetched and the audio PID's PES payload reassembled here.

use super::mp4::AudioConfig;

const TS_SIZE: usize = 188;
const SYNC: u8 = 0x47;
const PAT_PID: u16 = 0;
const STREAM_TYPE_ADTS_AAC: u8 = 0x0F;

struct Packet {
    off: usize,
    pid: u16,
    payload: usize,
    len: usize,
}

fn packets(seg: &[u8]) -> impl Iterator<Item = Packet> + '_ {
    let n = seg.len();
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + TS_SIZE <= n {
            if seg[i] != SYNC {
                i += 1;
                continue;
            }
            let off = i;
            i += TS_SIZE;
            let pid = (((seg[off + 1] & 0x1F) as u16) << 8) | seg[off + 2] as u16;
            let flags = seg[off + 3];
            let mut p = off + 4;
            if flags & 0x20 != 0 {
                p += 1 + seg[p] as usize;
            }
            if flags & 0x10 != 0 && p < off + TS_SIZE {
                return Some(Packet { off, pid, payload: p, len: off + TS_SIZE - p });
            }
        }
        None
    })
}

/// ADTS-AAC elementary PID via PAT → PMT.
pub fn audio_pid(seg: &[u8]) -> Option<u16> {
    let pat = packets(seg).find(|p| p.pid == PAT_PID)?;
    let q = pat.payload + 1 + seg[pat.payload] as usize;
    if q + 12 > pat.payload + pat.len || seg[q] != 0x00 {
        return None;
    }
    let pmt_pid = u16::from_be_bytes([seg[q + 10], seg[q + 11]]) & 0x1FFF;
    for p in packets(seg).filter(|p| p.pid == pmt_pid) {
        let q = p.payload + 1 + seg[p.payload] as usize;
        if q + 12 > p.payload + p.len || seg[q] != 0x02 {
            continue;
        }
        let prog_info_len = (((seg[q + 10] & 0x0F) as usize) << 8) | seg[q + 11] as usize;
        let sec_end = (q + 3 + ((((seg[q + 1] & 0x0F) as usize) << 8) | seg[q + 2] as usize)).min(seg.len());
        let mut r = q + 12 + prog_info_len;
        while r + 5 <= sec_end {
            let st = seg[r];
            let epid = (((seg[r + 1] & 0x1F) as u16) << 8) | seg[r + 2] as u16;
            let esl = (((seg[r + 3] & 0x0F) as usize) << 8) | seg[r + 4] as usize;
            if st == STREAM_TYPE_ADTS_AAC {
                return Some(epid);
            }
            r += 5 + esl;
        }
        break;
    }
    None
}

/// Pure ADTS bytes of the audio PID, with PES headers stripped.
pub fn audio_payload(seg: &[u8], pid: u16) -> Vec<u8> {
    let mut out = Vec::new();
    for p in packets(seg).filter(|p| p.pid == pid) {
        let pl = &seg[p.payload..p.payload + p.len];
        let l = pl.len();
        if seg[p.off + 1] & 0x40 != 0 {
            if pl.len() >= 4 && pl[..3] == [0, 0, 1] {
                let sid = pl[3];
                let mut q = 4;
                if q + 2 <= l {
                    q += 2;
                    if ![0xBC, 0xBE, 0xBF, 0xF0, 0xF1, 0xF2, 0xF8, 0xFF].contains(&sid)
                        && q + 3 <= l
                        && pl[q] & 0xC0 == 0x80
                    {
                        q += 3 + pl[q + 2] as usize;
                    }
                }
                if q < l {
                    out.extend_from_slice(&pl[q..]);
                }
            } else {
                out.extend_from_slice(pl);
            }
        } else {
            out.extend_from_slice(pl);
        }
    }
    out
}

/// Fallback without PAT/PMT: the PID whose payload is densest in ADTS syncs.
fn sniff_audio_pid(seg: &[u8]) -> Option<u16> {
    let mut seen = std::collections::HashSet::new();
    let (mut best, mut best_hits) = (None, 0);
    for p in packets(seg) {
        if !seen.insert(p.pid) {
            continue;
        }
        let pl = &seg[p.payload..p.payload + p.len];
        let hits = pl.windows(2).filter(|w| w[0] == 0xFF && w[1] & 0xF0 == 0xF0).count();
        if hits > best_hits {
            best = Some(p.pid);
            best_hits = hits;
        }
    }
    if best_hits >= 2 { best } else { None }
}

/// Split a buffer into complete, plausibility-checked ADTS frames.
pub fn split_adts(buf: &[u8]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let (mut i, n) = (0usize, buf.len());
    while i + 7 <= n {
        if !(buf[i] == 0xFF && buf[i + 1] & 0xF6 == 0xF0) {
            i += 1;
            continue;
        }
        let hdr = if buf[i + 1] & 0x01 == 0 { 9 } else { 7 };
        let freq_index = (buf[i + 2] >> 2) & 0x0F;
        let channels = ((buf[i + 2] & 0x01) << 2) | ((buf[i + 3] >> 6) & 0x03);
        if freq_index >= 13 || channels > 7 {
            i += 1;
            continue;
        }
        let flen = (((buf[i + 3] & 0x03) as usize) << 11) | ((buf[i + 4] as usize) << 3) | ((buf[i + 5] >> 5) as usize);
        if flen < hdr || flen > 8191 {
            i += 1;
            continue;
        }
        if i + flen > n {
            break;
        }
        frames.push(buf[i..i + flen].to_vec());
        i += flen;
    }
    frames
}

/// All complete ADTS frames of a segment's audio PID (empty if none found).
pub fn extract_audio(seg: &[u8]) -> Vec<Vec<u8>> {
    match audio_pid(seg).or_else(|| sniff_audio_pid(seg)) {
        Some(pid) => split_adts(&audio_payload(seg, pid)),
        None => Vec::new(),
    }
}

/// AOT / sample-rate index / channels from an ADTS header.
pub fn adts_config(frame: &[u8]) -> AudioConfig {
    AudioConfig {
        aot: ((frame[2] >> 6) & 0x03) + 1,
        freq_index: (frame[2] >> 2) & 0x0F,
        channels: ((frame[2] & 0x01) << 2) | ((frame[3] >> 6) & 0x03),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_adts_rejects_garbage() {
        let cfg = AudioConfig::default();
        let mut buf = vec![0x00, 0xFF, 0x12];
        for n in [10usize, 20, 30] {
            buf.extend(cfg.adts_header(n));
            buf.extend(std::iter::repeat_n(0x55, n));
        }
        let f = split_adts(&buf);
        assert_eq!(f.len(), 3);
        assert_eq!(f[1].len(), 27);
        assert_eq!(adts_config(&f[0]), cfg);
    }
}
