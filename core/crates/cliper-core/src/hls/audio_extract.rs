//! Audio-only extraction from a resolved VOD — port of `extractor.py`.
//!
//! fMP4: per fragment, read the `moof` (small) and range-fetch only the audio
//! runs listed in its `trun`, skipping video `mdat`; raw AAC is re-wrapped in
//! ADTS. Handles any number of fragments (chzzk: two bands, SOOP: tail block).
//! MPEG-TS: whole segment, demux the audio PID.
//!
//! Output is a lossless ADTS stream plus a per-segment frame index, which pins
//! every segment to an exact sample offset on the audio timeline.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::mp4::{self, AudioConfig};
use super::ts;
use crate::http::{Http, NetError};
use crate::progress::{Cancelled, CancellationToken, Progress, ProgressFn};
use crate::source::{Container, Source};

#[derive(Debug, thiserror::Error)]
pub enum ExtractError {
    #[error(transparent)]
    Net(#[from] NetError),
    #[error("{0}")]
    Format(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Cancelled(#[from] Cancelled),
}

/// Per-segment placement on the audio timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SegIndex {
    /// first segment index (in the audio variant's playlist) that was extracted
    pub first_segment: usize,
    /// AAC frames contributed by each extracted segment
    pub frames: Vec<u32>,
    /// EXTINF duration of each extracted segment
    pub durations: Vec<f64>,
    pub samples_per_frame: u32,
    pub sample_rate: u32,
}

impl SegIndex {
    /// Sample offset (on the extracted audio) where segment `first_segment + k` begins.
    pub fn sample_at_segment(&self, k: usize) -> u64 {
        self.frames[..k.min(self.frames.len())].iter().map(|&f| f as u64).sum::<u64>() * self.samples_per_frame as u64
    }
    pub fn total_samples(&self) -> u64 {
        self.sample_at_segment(self.frames.len())
    }
    pub fn duration(&self) -> f64 {
        self.total_samples() as f64 / self.sample_rate as f64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioResult {
    pub config: AudioConfig,
    pub index: SegIndex,
    pub audio_bytes: u64,
    pub fetched_bytes: u64,
    pub full_source_bytes: u64,
    pub fallback_segments: u32,
    pub elapsed_sec: f64,
}

struct SegOut {
    frames: Vec<Vec<u8>>,
    fetched: u64,
    seg_size: u64,
    tier: usize,
    cfg: AudioConfig,
}

fn fmt_err(e: impl std::fmt::Display) -> ExtractError {
    ExtractError::Format(e.to_string())
}

/// Fetch only the audio runs of one fMP4 segment. Parse anomalies fall back to a
/// full-segment read; network errors propagate so the caller can escalate.
async fn extract_ranged(http: &Http, url: &str, tid: u32, head: u64, retries: u32) -> Result<(Vec<Vec<u8>>, u64, u64), NetError> {
    match extract_ranged_inner(http, url, tid, head, retries).await {
        Ok(Some(r)) => Ok(r),
        Ok(None) => extract_full(http, url, tid, retries).await,
        Err(e) => Err(e),
    }
}

async fn extract_ranged_inner(http: &Http, url: &str, tid: u32, head: u64, retries: u32) -> Result<Option<(Vec<Vec<u8>>, u64, u64)>, NetError> {
    let mut frames = Vec::new();
    let mut fetched = 0u64;
    // HEAD first: warms the CDN edge with the full object (see `Http::warm`) and
    // gives the segment size up front.
    let mut seg_size: Option<u64> = http.warm(url, retries).await?;
    let mut base = 0u64;
    for _guard in 0..256 {
        let want_end = match seg_size {
            Some(s) if s > 0 => (base + head).min(s) - 1,
            _ => base + head - 1,
        };
        let (mut win, total) = http.get_range(url, base, want_end, retries).await?;
        fetched += win.len() as u64;
        if total.is_some() {
            seg_size = total;
        }
        let Some(m) = mp4::find_box(&win, &[b"moof"], 0, win.len()) else { break };
        if m.end() > win.len() {
            // moof truncated → fetch the rest of it
            let (extra, _) = http
                .get_range(url, base + win.len() as u64, base + m.end() as u64 - 1, retries)
                .await?;
            fetched += extra.len() as u64;
            win.extend_from_slice(&extra);
        }
        let Some((runs, mbox)) = mp4::runs_in_moof(&win, m.start, base) else { return Ok(None) };
        if runs.is_empty() {
            return Ok(None);
        }
        if let Some(size) = seg_size
            && let Some(need) = runs.iter().map(|r| r.end()).max().filter(|&e| e > size) {
                return Err(NetError::Truncated { have: size, need, url: crate::http::redact(url) });
            }
        let win_end = base + win.len() as u64;
        for r in runs.iter().filter(|r| r.track_id == tid) {
            let got = if r.start >= base && r.end() <= win_end {
                r.split(&win, base)
            } else if r.nbytes() == 0 {
                Some(Vec::new())
            } else {
                let (ab, _) = http.get_range(url, r.start, r.end() - 1, retries).await?;
                fetched += ab.len() as u64;
                r.split(&ab, r.start)
            };
            match got {
                Some(f) => frames.extend(f),
                None => return Ok(None),
            }
        }
        let frag_end = runs.iter().map(|r| r.end()).max().unwrap_or(base + mbox.end() as u64);
        if seg_size.is_some_and(|s| frag_end >= s) || frag_end <= base {
            break;
        }
        base = frag_end;
    }
    Ok(Some((frames, fetched, seg_size.unwrap_or(fetched))))
}

async fn extract_full(http: &Http, url: &str, tid: u32, retries: u32) -> Result<(Vec<Vec<u8>>, u64, u64), NetError> {
    let (seg, total) = http.get_full(url, retries).await?;
    let frames = mp4::iter_segment_runs(&seg)
        .iter()
        .filter(|r| r.track_id == tid)
        .flat_map(|r| r.split(&seg, 0).unwrap_or_default())
        .collect();
    Ok((frames, seg.len() as u64, total.unwrap_or(seg.len() as u64)))
}

async fn extract_ts(http: &Http, url: &str, retries: u32) -> Result<(Vec<Vec<u8>>, u64, u64), NetError> {
    let (seg, total) = http.get_full(url, retries).await?;
    let frames = ts::extract_audio(&seg);
    Ok((frames, seg.len() as u64, total.unwrap_or(seg.len() as u64)))
}

/// One silent AAC access unit (raw, no ADTS header) for `cfg`, encoded by ffmpeg.
fn silent_frame(cfg: &AudioConfig) -> Result<Vec<u8>, ExtractError> {
    let mut c = crate::audio::ffmpeg::command();
    c.args(["-f", "lavfi", "-i", &format!("anullsrc=r={}:cl=mono", cfg.sample_rate())])
        .args(["-t", "0.5", "-ac", &cfg.channels.max(1).to_string(), "-c:a", "aac", "-f", "adts", "pipe:1"]);
    let out = c.stderr(std::process::Stdio::null()).output().map_err(|e| fmt_err(format!("ffmpeg: {e}")))?;
    // take a frame from the middle (the first frames carry encoder priming)
    let mut frames = Vec::new();
    let mut i = 0usize;
    let b = &out.stdout;
    while i + 7 <= b.len() && b[i] == 0xFF && b[i + 1] & 0xF0 == 0xF0 {
        let len = (((b[i + 3] & 3) as usize) << 11) | ((b[i + 4] as usize) << 3) | ((b[i + 5] as usize) >> 5);
        let hdr = if b[i + 1] & 1 == 0 { 9 } else { 7 };
        if len <= hdr || i + len > b.len() {
            break;
        }
        frames.push(b[i + hdr..i + len].to_vec());
        i += len;
    }
    frames.get(frames.len() / 2).cloned().ok_or_else(|| fmt_err("could not encode a silent AAC frame"))
}

/// Options for [`extract_audio`].
#[derive(Debug, Clone, Default)]
pub struct ExtractOptions {
    /// segment index range `[first, last)`; `None` = whole VOD
    pub segments: Option<(usize, usize)>,
    /// concurrency; `None` = platform default
    pub workers: Option<usize>,
    /// pull audio from the variant closest to this height instead of the
    /// resolver's choice (the primary then becomes a fallback)
    pub variant_height: Option<u32>,
}

/// `src` with `idx` as the audio variant; the old primary becomes the first fallback.
async fn with_audio_variant(src: &Source, idx: usize) -> Result<Source, ExtractError> {
    let playlist = src.variant_playlist(idx).await.map_err(fmt_err)?;
    if !playlist.same_layout(&src.playlist) && playlist.segments.len() != src.playlist.segments.len() {
        return Err(fmt_err("variant segmentation differs from the primary"));
    }
    let init = match &playlist.init_url {
        Some(u) => src.http.get_bytes(u).await?,
        None => Vec::new(),
    };
    let mut out = src.clone();
    let mut fallbacks = vec![crate::source::AudioFallback { playlist: src.playlist.clone(), init: src.init.clone() }];
    fallbacks.extend(src.audio_fallbacks.iter().filter(|f| f.playlist.segments != playlist.segments).cloned());
    out.audio_fallbacks = fallbacks;
    out.playlist = playlist;
    out.init = init;
    out.info.audio_variant = idx;
    Ok(out)
}

/// Extract the audio of `src` into an ADTS file at `out`.
pub async fn extract_audio(
    src: &Source,
    out: &Path,
    opts: &ExtractOptions,
    progress: ProgressFn,
    cancel: CancellationToken,
) -> Result<AudioResult, ExtractError> {
    let t0 = Instant::now();
    let is_ts = src.info.container == Container::Ts;
    let swapped;
    let src = match opts.variant_height {
        Some(h) if src.pick_variant(Some(h)) != src.info.audio_variant => {
            swapped = with_audio_variant(src, src.pick_variant(Some(h))).await?;
            &swapped
        }
        _ => src,
    };
    let pl = &src.playlist;
    let (first, last) = opts.segments.unwrap_or((0, pl.segments.len()));
    let last = last.min(pl.segments.len());
    if first >= last {
        return Err(fmt_err("selected range contains no segments"));
    }

    let (cfg, tid): (AudioConfig, u32) = if is_ts {
        // TS has no init segment: probe the first segment for the AAC config.
        let (probe, _) = src.http.get_full(&pl.segments[first], 4).await?;
        let frames = ts::extract_audio(&probe);
        let f = frames.first().ok_or_else(|| fmt_err("no AAC stream in first TS segment"))?;
        (ts::adts_config(f), 0)
    } else {
        let tid = mp4::audio_track_id(&src.init).ok_or_else(|| fmt_err("no audio track in init segment"))?;
        (mp4::parse_audio_config(&src.init), tid)
    };

    let n = last - first;
    let workers = opts.workers.unwrap_or(src.default_workers).max(1);
    let fetched_total = Arc::new(AtomicU64::new(0));
    let head = src.head_hint;

    // (url, track id, ADTS config) per tier: primary first, then fallbacks
    let mut tiers_cfg: Vec<(u32, AudioConfig)> = vec![(tid, cfg)];
    for fb in &src.audio_fallbacks {
        if is_ts {
            tiers_cfg.push((0, cfg));
        } else {
            let Some(t) = mp4::audio_track_id(&fb.init) else { continue };
            tiers_cfg.push((t, mp4::parse_audio_config(&fb.init)));
        }
    }
    let fallbacks_ok: Vec<&crate::source::AudioFallback> = src
        .audio_fallbacks
        .iter()
        .filter(|fb| is_ts || mp4::audio_track_id(&fb.init).is_some())
        .collect();

    let tasks = (first..last).map(|idx| {
        let http = src.http.clone();
        let mut urls = vec![(pl.segments[idx].clone(), tiers_cfg[0])];
        for (k, fb) in fallbacks_ok.iter().enumerate() {
            if let Some(u) = fb.playlist.segments.get(idx) {
                urls.push((u.clone(), tiers_cfg[k + 1]));
            }
        }
        let cancel = cancel.clone();
        let fetched_total = fetched_total.clone();
        async move {
            if cancel.is_cancelled() {
                return Err(ExtractError::Cancelled(Cancelled));
            }
            let mut last_err = None;
            for (tier, (u, (tid, tcfg))) in urls.iter().enumerate() {
                // fail fast on a variant with a fallback behind it (a CDN-truncated
                // copy never heals via retry); full retry budget only on the last.
                let retries = if tier + 1 == urls.len() { 8 } else { 1 };
                let r = if is_ts { extract_ts(&http, u, retries).await } else { extract_ranged(&http, u, *tid, head, retries).await };
                match r {
                    Ok((frames, fetched, seg_size)) if !frames.is_empty() || tier + 1 == urls.len() => {
                        fetched_total.fetch_add(fetched, Ordering::Relaxed);
                        return Ok(SegOut { frames, fetched, seg_size, tier, cfg: *tcfg });
                    }
                    Ok(_) => {
                        last_err = Some(NetError::Truncated { have: 0, need: 0, url: crate::http::redact(u) });
                    }
                    Err(e) => last_err = Some(e),
                }
            }
            Err(ExtractError::Net(last_err.unwrap()))
        }
    });

    let mut stream = futures::stream::iter(tasks).buffered(workers);
    let mut silent_au: Option<Vec<u8>> = None;
    let mut silent_segments = 0u32;
    let mut file = tokio::io::BufWriter::with_capacity(1 << 20, tokio::fs::File::create(out).await?);
    let mut frames_per_seg = Vec::with_capacity(n);
    let (mut audio_bytes, mut fetched_bytes, mut full_bytes, mut fallbacks) = (0u64, 0u64, 0u64, 0u32);
    let mut done = 0u64;
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(Cancelled.into()),
            x = stream.next() => x,
        };
        let Some(seg) = next else { break };
        let mut seg = seg?;
        if seg.frames.is_empty() {
            // A segment without audio (parse failure, no AAC track) must still
            // take its EXTINF duration on the analysis timeline, or every later
            // time shifts: fill it with silent AAC frames.
            let dur = pl.durations[first + frames_per_seg.len()];
            let count = (dur * seg.cfg.sample_rate() as f64 / 1024.0).round() as usize;
            if count > 0 {
                if silent_au.is_none() {
                    let c = seg.cfg;
                    silent_au = Some(tokio::task::spawn_blocking(move || silent_frame(&c)).await.map_err(|e| fmt_err(e.to_string()))??);
                }
                let au = silent_au.clone().unwrap();
                seg.frames = if is_ts {
                    let mut f = seg.cfg.adts_header(au.len()).to_vec();
                    f.extend_from_slice(&au);
                    vec![f; count]
                } else {
                    vec![au; count]
                };
                silent_segments += 1;
            }
        }
        for fr in &seg.frames {
            if is_ts {
                file.write_all(fr).await?;
                audio_bytes += fr.len() as u64;
            } else {
                file.write_all(&seg.cfg.adts_header(fr.len())).await?;
                file.write_all(fr).await?;
                audio_bytes += fr.len() as u64 + 7;
            }
        }
        frames_per_seg.push(seg.frames.len() as u32);
        fetched_bytes += seg.fetched;
        full_bytes += seg.seg_size;
        if seg.tier > 0 {
            fallbacks += 1;
        }
        done += 1;
        progress(Progress {
            bytes: Some(fetched_total.load(Ordering::Relaxed)),
            ..Progress::new("audio.fetch", done, n as u64)
        });
    }
    file.flush().await?;
    if silent_segments > 0 {
        progress(
            Progress::note("audio.fetch", "silent_segments", format!("{silent_segments} segment(s) had no audio and were filled with silence"))
                .arg("count", silent_segments),
        );
    }
    if fallbacks > 0 {
        progress(Progress::note("audio.fetch", "fallback_segments", format!("{fallbacks} segment(s) recovered via a fallback variant")).arg("count", fallbacks));
    }

    Ok(AudioResult {
        config: cfg,
        index: SegIndex {
            first_segment: first,
            frames: frames_per_seg,
            durations: pl.durations[first..last].to_vec(),
            samples_per_frame: 1024,
            sample_rate: cfg.sample_rate(),
        },
        audio_bytes,
        fetched_bytes,
        full_source_bytes: full_bytes,
        fallback_segments: fallbacks,
        elapsed_sec: t0.elapsed().as_secs_f64(),
    })
}
