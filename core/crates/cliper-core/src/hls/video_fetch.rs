//! Fetch the video of a VOD time range: only the segments covering it, at a
//! chosen rendition, remuxed (no re-encode) into one MP4.
//!
//! Segments are fetched with plain GETs (never Range — see `Http::warm`). CDN
//! copies can be truncated; fMP4 segments are validated against their own box
//! sizes and a broken clip is retried as a whole on the next lower variant
//! (mixing renditions inside one clip would break the stream).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use super::m3u8::{MediaPlaylist, SegRange};
use super::mp4;
use crate::audio::ffmpeg;
use crate::http::NetError;
use crate::progress::{Cancelled, CancellationToken, Progress, ProgressFn};
use crate::source::{Container, Source};

#[derive(Debug, thiserror::Error)]
pub enum VideoError {
    #[error("internal: {0}")]
    Internal(String),
    #[error(transparent)]
    Net(#[from] NetError),
    #[error("{0}")]
    Format(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Ffmpeg(#[from] ffmpeg::FfmpegError),
    #[error(transparent)]
    Cancelled(#[from] Cancelled),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct VideoClip {
    pub path: PathBuf,
    /// variant actually used
    pub variant: usize,
    pub resolution: String,
    /// segments `[first, last)` of that variant's playlist
    pub segments: SegRange,
    /// VOD time (EXTINF timeline) at clip t=0
    pub vod_start: f64,
    /// clip duration (EXTINF sum)
    pub duration: f64,
    pub bytes: u64,
    /// variants that were tried and found broken on the CDN
    pub skipped_variants: Vec<usize>,
}

/// fMP4 segment sanity: every top-level box fits inside the payload.
pub(crate) fn fmp4_complete(seg: &[u8]) -> bool {
    let boxes = mp4::iter_boxes(seg, 0, seg.len());
    match boxes.last() {
        Some((_, b)) => b.end() == seg.len() && boxes.iter().any(|(t, _)| t == b"mdat"),
        None => false,
    }
}

/// Download segments `range` of `pl` into `out_raw` (init + concatenated segments).
async fn fetch_raw(
    src: &Source,
    pl: &MediaPlaylist,
    range: SegRange,
    out_raw: &Path,
    workers: usize,
    progress: &ProgressFn,
    cancel: &CancellationToken,
) -> Result<u64, VideoError> {
    let is_ts = src.info.container == Container::Ts;
    let mut file = tokio::io::BufWriter::with_capacity(4 << 20, tokio::fs::File::create(out_raw).await?);
    let mut written = 0u64;
    if !is_ts {
        let init_url = pl.init_url.as_ref().ok_or_else(|| VideoError::Format("variant has no init segment".into()))?;
        let init = src.http.get_bytes(init_url).await?;
        file.write_all(&init).await?;
        written += init.len() as u64;
    }
    let n = (range.last - range.first) as u64;
    let fetched = Arc::new(AtomicU64::new(0));
    let tasks = (range.first..range.last).map(|i| {
        let http = src.http.clone();
        let url = pl.segments[i].clone();
        let fetched = fetched.clone();
        async move {
            let (seg, _) = http.get_full(&url, 6).await?;
            if !is_ts && !fmp4_complete(&seg) {
                return Err(VideoError::Net(NetError::Truncated {
                    have: seg.len() as u64,
                    need: 0,
                    url: crate::http::redact(&url),
                }));
            }
            fetched.fetch_add(seg.len() as u64, Ordering::Relaxed);
            Ok::<_, VideoError>(seg)
        }
    });
    let mut stream = futures::stream::iter(tasks).buffered(workers.max(1));
    let mut done = 0u64;
    loop {
        let next = tokio::select! {
            _ = cancel.cancelled() => return Err(Cancelled.into()),
            x = stream.next() => x,
        };
        let Some(seg) = next else { break };
        let seg = seg?;
        file.write_all(&seg).await?;
        written += seg.len() as u64;
        done += 1;
        progress(Progress { bytes: Some(fetched.load(Ordering::Relaxed)), ..Progress::new("video.fetch", done, n) });
    }
    file.flush().await?;
    Ok(written)
}

/// Options for [`fetch_video`].
#[derive(Debug, Clone)]
pub struct VideoOptions {
    /// target height (`None` = highest)
    pub height: Option<u32>,
    pub workers: Option<usize>,
}

/// Fetch VOD time `[start, end)` into `out` (MP4, stream copy). The clip starts
/// on a segment boundary at or before `start` and ends on one at or after `end`.
pub async fn fetch_video(
    src: &Source,
    start: f64,
    end: f64,
    out: &Path,
    opts: &VideoOptions,
    progress: ProgressFn,
    cancel: CancellationToken,
) -> Result<VideoClip, VideoError> {
    // candidate variants: requested first, then progressively lower ones
    let first = src.pick_variant(opts.height);
    let mut order: Vec<usize> = (0..src.info.variants.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(src.info.variants[i].pixels()));
    let h0 = src.info.variants[first].height;
    let mut candidates = vec![first];
    candidates.extend(order.into_iter().filter(|&i| i != first && src.info.variants[i].height < h0));

    let workers = opts.workers.unwrap_or(src.default_workers.min(8)).max(1);
    let raw = out.with_extension(if src.info.container == Container::Ts { "part.ts" } else { "part.mp4" });
    let mut skipped = Vec::new();
    for v in candidates {
        let pl = src.variant_playlist(v).await.map_err(|e| VideoError::Format(e.to_string()))?;
        if pl.segments.len() != src.playlist.segments.len() {
            tracing::warn!("variant {v} segmentation differs from the audio playlist; skipping");
            skipped.push(v);
            continue;
        }
        let range = pl.segment_range(start, Some(end));
        match fetch_raw(src, &pl, range, &raw, workers, &progress, &cancel).await {
            Ok(_) => {}
            Err(VideoError::Net(NetError::Truncated { .. })) => {
                let res = src.info.variants[v].resolution();
                progress(
                    Progress::note("video.fetch", "variant_truncated", format!("{res} copy is truncated on the CDN; trying a lower rendition"))
                        .arg("resolution", res),
                );
                skipped.push(v);
                continue;
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&raw).await;
                return Err(e);
            }
        }
        progress(Progress::note("video.remux", "remux", "remuxing"));
        let (raw_c, out_c, cancel_c) = (raw.clone(), out.to_path_buf(), cancel.clone());
        tokio::task::spawn_blocking(move || {
            let mut c = ffmpeg::command();
            c.arg("-y").arg("-i").arg(&raw_c).args(["-map", "0:v:0", "-map", "0:a:0", "-c", "copy", "-movflags", "+faststart"]).arg(&out_c);
            ffmpeg::run_cancellable(c, &cancel_c)
        })
        .await
        .map_err(|e| VideoError::Internal(e.to_string()))??;
        let _ = tokio::fs::remove_file(&raw).await;
        let bytes = tokio::fs::metadata(out).await?.len();
        let duration = pl.durations[range.first..range.last].iter().sum();
        return Ok(VideoClip {
            path: out.to_path_buf(),
            variant: v,
            resolution: src.info.variants[v].resolution(),
            segments: range,
            vod_start: range.seg_start,
            duration,
            bytes,
            skipped_variants: skipped,
        });
    }
    let _ = tokio::fs::remove_file(&raw).await;
    Err(VideoError::Format("no rendition could serve this range (all CDN copies truncated)".into()))
}
