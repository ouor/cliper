//! Separation that follows the video (S-E5) and incremental mute suggestions
//! (S-E6).
//!
//! `stems.ensure` separates the parts of the requested VOD ranges that have
//! video and aren't separated yet. Existing spans are kept and joined:
//!
//! 1. the result range `U` (request ∪ the spans it touches) is decoded again
//!    from the video. An old span is kept if it holds the same audio: most
//!    samples bit-identical at lag 0 (AAC noise substitution makes some frames
//!    differ between decodes that started at different places);
//! 2. only the missing parts are separated, each with [`CTX`] seconds of
//!    already separated context on both sides;
//! 3. the new (original, vocals) pair replaces the old pair from the middle of
//!    that context on, with one raised-cosine crossfade of [`XF`] seconds —
//!    far from the model's chunk edges, where both separations of the same
//!    audio agree closely — so there is no seam, and original and vocals
//!    always come from the same decode.
//!
//! An old span that doesn't match (other bytes, e.g. another rendition) is
//! separated again where video exists (Q-S7's allowed fallback).
//! Talk is searched only in newly separated parts that were never searched
//! before (`talk_scanned`), so deleted suggestions never come back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, Engine, JobCtx, LockExt, is_local, precondition};
use crate::analysis::talk::{self, TalkParams};
use crate::audio::{peaks, wav};
use crate::ml::separator::ModelSpec;
use crate::progress::{CancellationToken, Progress, ProgressFn};
use crate::project::{self, Mute, Origin, Project, StemSpan};
use crate::ranges::{self, Range};
use crate::stems::STEM_RATE;
use crate::video::media::{self, Piece};

/// Seconds of already separated audio a new separation reaches into.
pub const CTX: f64 = 10.0;
/// Crossfade between old and new vocals (s).
pub const XF: f64 = 1.0;
/// Longest span written (R-2): a 48 kHz stereo float WAV can't pass 4 GiB
/// (≈ 3.1 h), and every extension rewrites the span it grows.
pub const MAX_SPAN_SEC: f64 = 30.0 * 60.0;
/// Spans that would grow past [`MAX_SPAN_SEC`] stay apart and overlap by this
/// much instead; readers crossfade across the overlap.
pub const SPAN_OVERLAP: f64 = 2.0 * CTX;
/// Old original audio must match the fresh decode to this (max abs, per block).
const MATCH_TOL: f32 = 1e-4;
const RATE: f64 = STEM_RATE as f64;

/// What `stems.ensure` did.
#[derive(Debug, Clone, Serialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct EnsureResult {
    /// VOD ranges separated now (new parts)
    pub separated: Vec<Range>,
    /// ranges separated again as a whole because old stems didn't match
    pub reseparated: Vec<Range>,
    /// ids of the spans written
    pub spans: Vec<String>,
    /// mute suggestions added
    pub suggestions: usize,
    /// … per song (song id → count)
    pub suggestions_by_song: std::collections::BTreeMap<String, usize>,
    /// requested ranges without video (not separated)
    pub no_video: Vec<Range>,
}

fn grid(t: f64) -> i64 {
    (t * RATE).round() as i64
}

/// Interleaved stereo frames `[g0, g1)` (grid) of a span file, zero where the
/// span has no data.
fn read_span(dir: &Path, s: &StemSpan, file: &str, g0: i64, g1: i64) -> std::io::Result<Vec<f32>> {
    let mut out = vec![0f32; ((g1 - g0).max(0) * 2) as usize];
    let sg = grid(s.start);
    let a = g0.max(sg);
    let b = g1.min(sg + s.frames as i64);
    if b > a {
        let w = wav::read_f32_range(&dir.join(s.file(file)), (a - sg) as u64, (b - a) as u64)?;
        let o = ((a - g0) * 2) as usize;
        out[o..o + w.samples.len()].copy_from_slice(&w.samples);
    }
    Ok(out)
}

fn raised(x: f64) -> f64 {
    if x <= 0.0 {
        0.0
    } else if x >= 1.0 {
        1.0
    } else {
        0.5 - 0.5 * (std::f64::consts::PI * x).cos()
    }
}

/// Weight of the new vocals at VOD time `t` for a new part `m` separated over
/// `r` (⊇ m): 1 inside, ramping to 0 at the middle of each context side.
fn new_weight(t: f64, m: Range, r: Range) -> f64 {
    let left = if m.0 - r.0 < 1e-6 {
        if t >= m.0 { 1.0 } else { 0.0 }
    } else {
        let c = m.0 - (m.0 - r.0) / 2.0;
        let xf = XF.min(m.0 - r.0);
        raised((t - (c - xf / 2.0)) / xf)
    };
    let right = if r.1 - m.1 < 1e-6 {
        if t < m.1 { 1.0 } else { 0.0 }
    } else {
        let c = m.1 + (r.1 - m.1) / 2.0;
        let xf = XF.min(r.1 - m.1);
        1.0 - raised((t - (c - xf / 2.0)) / xf)
    };
    left * right
}

impl Engine {
    pub(crate) fn stems_lock(&self, pid: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.stems_locks.lk().entry(pid.to_string()).or_default().clone()
    }

    /// `stems.ensure` job: separate (and suggest mutes for) the parts of the
    /// songs ± margins / ranges that have video and aren't separated yet.
    pub fn job_stems(self: &Arc<Self>, pid: &str, song_ids: Option<Vec<String>>, ranges_: Option<Vec<Range>>) -> ApiResult<String> {
        let p = self.get(pid)?;
        if p.audio.is_none() {
            return Err(precondition("fetch the audio first"));
        }
        let targets = self.stem_targets(&p, song_ids.as_deref(), ranges_.as_deref())?;
        self.spawn_job("stems.ensure", pid, Vec::new(), move |eng, ctx| async move {
            let r = eng.stems_ensure(&p.id, &targets, &ctx).await?;
            Ok(serde_json::to_value(r).unwrap_or(Value::Null))
        })
    }

    /// Ranges to separate: given ranges, else given songs ± margins, else every enabled song.
    pub(crate) fn stem_targets(&self, p: &Project, song_ids: Option<&[String]>, rs: Option<&[Range]>) -> ApiResult<Vec<Range>> {
        let mut out: Vec<Range> = rs.map(|r| r.to_vec()).unwrap_or_default();
        match song_ids {
            Some(ids) => {
                for id in ids {
                    let s = p.song(id).ok_or_else(|| ApiError::new("NOT_FOUND", format!("song {id}")))?;
                    out.push(s.with_margins(&p.settings));
                }
            }
            None if rs.is_none() => out.extend(p.songs.iter().filter(|s| s.enabled).map(|s| s.with_margins(&p.settings))),
            None => {}
        }
        Ok(ranges::normalize(&out))
    }

    /// Remember ranges to separate after the running `stems.ensure` (fetches
    /// with `process`); starts a job when none runs.
    pub(crate) fn queue_process(self: &Arc<Self>, pid: &str, rs: Vec<Range>) {
        // kept in video/index.json until a job takes it (R-14)
        let _ = self.with_ps(pid, |ps| {
            ps.idx.pending_process = ranges::union(&ps.idx.pending_process, &rs);
            ps.idx_dirty = true;
        });
        self.save_index(pid);
        self.kick_process(pid);
    }

    pub(crate) fn kick_process(self: &Arc<Self>, pid: &str) {
        if self.jobs.lk().values().any(|j| j.info.project_id == pid && j.info.kind == "stems.ensure") {
            return;
        }
        let rs = self.with_ps(pid, |ps| std::mem::take(&mut ps.idx.pending_process)).unwrap_or_default();
        if rs.is_empty() {
            return;
        }
        let r = self.job_stems(pid, None, Some(rs.clone()));
        if let Err(e) = r {
            tracing::warn!("{pid}: separating fetched video: {e}");
            if e.code == "BUSY" {
                let _ = self.with_ps(pid, |ps| ps.idx.pending_process = ranges::union(&ps.idx.pending_process, &rs));
            }
        }
        let _ = self.with_ps(pid, |ps| ps.idx_dirty = true);
        self.save_index(pid);
    }

    /// Called when a `stems.ensure` job ends: start the next queued one.
    pub(crate) fn after_stems_job(self: &Arc<Self>, pid: &str) {
        self.kick_process(pid);
    }

    /// Separate what's missing in `targets` (VOD ranges). See the module docs.
    pub(crate) async fn stems_ensure(self: &Arc<Self>, pid: &str, targets: &[Range], ctx: &JobCtx) -> ApiResult<EnsureResult> {
        let lock = self.stems_lock(pid);
        let _g = lock.lock().await;
        let p = self.get(pid)?;
        let have = self.video_have(pid)?;
        let mut res = EnsureResult { no_video: ranges::drop_short(ranges::subtract(targets, &have), 0.05), ..Default::default() };
        let done: Vec<Range> = p.stems.iter().map(|s| (s.start, s.end)).collect();
        // contiguous video stretches the targets fall in
        let stretches: Vec<(Range, Vec<Piece>)> = if is_local(&p) {
            let src = p.source.as_ref().unwrap();
            vec![((0.0, p.audio.as_ref().map(|a| a.duration).unwrap_or(src.duration)), vec![Piece::Local { file: PathBuf::from(&src.url), track: p.settings.audio_track }])]
        } else {
            self.with_ps(pid, |ps| {
                let tl = ps.tl.clone();
                ps.stretches().into_iter().filter_map(|(a, b, pcs)| tl.as_ref().map(|tl| ((tl.start(a), tl.end(b - 1)), pcs))).collect()
            })?
        };
        let tl = self.with_ps(pid, |ps| ps.tl.clone()).ok().flatten();
        let mut checked: Vec<String> = Vec::new();
        for (crange, pieces) in stretches {
            let want = ranges::drop_short(ranges::intersect(targets, &[crange]), 0.5);
            for t in want.into_iter().flat_map(|t| split_target(t, MAX_SPAN_SEC - 2.0 * SPAN_OVERLAP, SPAN_OVERLAP)) {
                if ranges::covers(&done, t, 0.01) {
                    // spans from before the QA-2 fix are checked again where a
                    // rendition changes inside them (R-10); others just get the new version
                    let olds: Vec<&StemSpan> = p.stems.iter().filter(|s| s.version < project::STEM_VERSION && s.start < t.1 && s.end > t.0).collect();
                    if olds.is_empty() {
                        continue;
                    }
                    let segs = tl.as_ref().map(|tl| tl.segments_for(t.0, t.1, 0.0)).unwrap_or(0..0);
                    let inside: Vec<&Piece> = pieces.iter().filter(|pc| matches!(pc, Piece::Local { .. }) || (pc.first() < segs.end && pc.last() > segs.start)).collect();
                    let joins = inside.windows(2).any(|w| w[0].variant() != w[1].variant()) || inside.iter().any(|pc| pc.partial());
                    if !joins {
                        checked.extend(olds.iter().map(|s| s.id.clone()));
                        continue;
                    }
                }
                if ctx.cancel.is_cancelled() {
                    return Err(ApiError::new("CANCELLED", "cancelled"));
                }
                let (eng, pc, ctxc, pcs) = (self.clone(), p.id.clone(), ctx.clone(), pieces.clone());
                let r = tokio::task::spawn_blocking(move || eng.ensure_one(&pc, t, crange, &pcs, &ctxc)).await??;
                res.separated.extend(r.separated);
                res.reseparated.extend(r.reseparated);
                res.spans.extend(r.spans);
                res.suggestions += r.suggestions;
                for (k, v) in r.suggestions_by_song {
                    *res.suggestions_by_song.entry(k).or_default() += v;
                }
            }
        }
        if !checked.is_empty() {
            self.mutate(pid, |p| {
                for s in p.stems.iter_mut().filter(|s| checked.contains(&s.id)) {
                    s.version = project::STEM_VERSION;
                }
                Ok(())
            })
            .await?;
        }
        res.separated = ranges::normalize(&res.separated);
        Ok(res)
    }

    /// Separate the missing parts of `t` (inside stretch `crange` = `pieces`). Blocking.
    fn ensure_one(&self, pid: &str, t: Range, crange: Range, pieces: &[Piece], ctx: &JobCtx) -> ApiResult<EnsureResult> {
        let p = self.get(pid)?;
        let dir = self.store.dir(pid);
        // spans t touches → result range U; spans that would make U longer
        // than the cap stay as they are and the new span overlaps them (R-2)
        let near: Vec<StemSpan> =
            p.stems.iter().filter(|s| s.start <= t.1 + 0.5 && s.end >= t.0 - 0.5 && s.start < crange.1 && s.end > crange.0).cloned().collect();
        let (merge, t) = plan_span(t, &near.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(), MAX_SPAN_SEC, SPAN_OVERLAP);
        let olds: Vec<StemSpan> = near.into_iter().enumerate().filter(|(k, _)| merge.contains(k)).map(|(_, s)| s).collect();
        if t.1 - t.0 < 0.5 && olds.is_empty() {
            return Ok(EnsureResult::default());
        }
        let u = olds.iter().fold(t, |acc, s| (acc.0.min(s.start), acc.1.max(s.end)));
        let d = (u.0.max(crange.0), u.1.min(crange.1));
        if d.1 - d.0 < 0.5 {
            return Ok(EnsureResult::default());
        }
        let id = project::new_id("st");
        let rel = format!("stems/{id}");
        let out_dir = dir.join(&rel);
        std::fs::create_dir_all(&out_dir)?;
        let cleanup = scopeguard(out_dir.clone());

        // 1) decode D from the video, on the sample grid
        (ctx.progress)(Progress::note("separate", "decode", "decoding the video's audio"));
        let (g0, g1) = self.decode_grid(&p, pieces, d, &out_dir.join("new_orig.wav"), &ctx.cancel)?;
        let d = (g0 as f64 / RATE, g1 as f64 / RATE);
        let new_orig = out_dir.join("new_orig.wav");

        // 2) old spans stay where they hold the same audio. Decoding is
        //    deterministic except where the AAC stream uses noise substitution
        //    (PNS: the decoder's random generator depends on where decoding
        //    started), so "same" = most samples bit-identical at lag 0; an old
        //    span from other bytes (another rendition) or misplaced has ~none.
        // Checked per 0.5 s block, so a misplaced part of a span (QA-2: an
        // old join of two renditions) is separated again on its own.
        let mut good: Vec<(i64, i64)> = Vec::new();
        let mut res = EnsureResult::default();
        const CHECK: i64 = 24000;
        for s in &olds {
            let (sa, sb) = (grid(s.start), grid(s.start) + s.frames as i64);
            let (a, b) = (sa.max(g0), sb.min(g1));
            // what lies outside the video we have can't be redone: keep it
            if sa < g0 {
                good.push((sa, g0.min(sb)));
            }
            if sb > g1 {
                good.push((g1.max(sa), sb));
            }
            let mut bad: Vec<(i64, i64)> = Vec::new();
            let mut k = a;
            while k < b {
                let ke = (k + CHECK).min(b);
                let old = read_span(&dir, s, "orig.wav", k, ke)?;
                let new = wav::read_f32_range(&new_orig, (k - g0) as u64, (ke - k) as u64)?.samples;
                let same = old.iter().zip(&new).filter(|(x, y)| (*x - *y).abs() <= MATCH_TOL).count();
                // a decoder cold start at the old span's first block may differ: allow it
                // a decoder cold start (old span's or this decode's first block,
                // R-19) may differ; silence only counts when both are silent (R-9)
                let first_block = k == sa || k == g0;
                let silent = old.iter().all(|x| x.abs() < 1e-6) && new.iter().all(|x| x.abs() < 1e-6);
                let ok = same * 2 >= old.len() || (first_block && same * 4 >= old.len()) || silent;
                if ok {
                    good.push((k, ke));
                } else {
                    bad.push((k, ke));
                }
                k = ke;
            }
            if !bad.is_empty() {
                let secs: Vec<Range> = bad.iter().map(|&(x, y)| (x as f64 / RATE, y as f64 / RATE)).collect();
                let secs = ranges::normalize(&secs);
                tracing::warn!("{pid}: span {} doesn't match the video's audio in {:.1} s; separating that again", s.id, ranges::total(&secs));
                (ctx.progress)(Progress::note("separate", "stems_reseparated", "old stems don't match the video; separating them again"));
                res.reseparated.extend(secs);
            }
        }
        let good: Vec<(i64, i64)> = {
            let mut g = good;
            g.sort();
            let mut out: Vec<(i64, i64)> = Vec::new();
            for (x, y) in g {
                match out.last_mut() {
                    Some(l) if x <= l.1 => l.1 = l.1.max(y),
                    _ => out.push((x, y)),
                }
            }
            out
        };
        let to_sec = |v: &[(i64, i64)]| -> Vec<Range> { v.iter().map(|&(a, b)| (a as f64 / RATE, b as f64 / RATE)).collect() };
        // missing = D − good, on the grid (no slivers), short gaps of "good" between missing parts closed
        let missing: Vec<Range> = {
            let mut cur = vec![(g0, g1)];
            for &(a, b) in &good {
                let mut next = Vec::new();
                for (x, y) in cur {
                    if b <= x || a >= y {
                        next.push((x, y));
                        continue;
                    }
                    if a > x {
                        next.push((x, a));
                    }
                    if b < y {
                        next.push((b, y));
                    }
                }
                cur = next;
            }
            ranges::normalize_with(&to_sec(&cur), CTX)
        };
        let good = to_sec(&good);

        // 3) separate the missing parts with context
        let mut news: Vec<(Range, Range, PathBuf)> = Vec::new();
        for (k, &m) in missing.iter().enumerate() {
            let r = ((m.0 - CTX).max(d.0), (m.1 + CTX).min(d.1));
            let (ra, rb) = (grid(r.0) - g0, grid(r.1) - g0);
            let sec_in = out_dir.join(format!("sep{k}_in.wav"));
            let sec_out = out_dir.join(format!("sep{k}_vocals.wav"));
            let w = wav::read_f32_range(&new_orig, ra.max(0) as u64, (rb - ra).max(0) as u64)?;
            wav::write_f32(&sec_in, STEM_RATE, 2, &w.samples)?;
            drop(w);
            let progress: ProgressFn = ctx.progress.clone();
            self.separate_wav_fallback(&sec_in, &sec_out, &progress, &ctx.cancel)?;
            let _ = std::fs::remove_file(&sec_in);
            let r = ((ra + g0) as f64 / RATE, (rb + g0) as f64 / RATE);
            news.push((m, r, sec_out));
        }

        // 4) compose the span over U: old (orig, vocals) where kept, the new
        //    decode and its separation in the new parts, and between them one
        //    crossfade of both together, so orig and vocals always belong to
        //    the same decode and `orig − (1 − g)·vocals` stays consistent
        // only what was decoded or is kept from old spans (R-9: no silent
        // "separated" edges where the decode produced nothing)
        let gk: Vec<(i64, i64)> = good.iter().map(|&(a, b)| (grid(a), grid(b))).collect();
        let ug0 = gk.iter().map(|x| x.0).fold(g0, i64::min);
        let ug1 = gk.iter().map(|x| x.1).fold(g1, i64::max);
        let _ = u;
        let fin_frames = (ug1 - ug0) as u64;
        let mut ow = wav::WavWriter::create(&out_dir.join("orig.wav"), STEM_RATE, 2)?;
        let mut vw = wav::WavWriter::create(&out_dir.join("vocals.wav"), STEM_RATE, 2)?;
        let mut opk = peaks::PeakBuilder::new(STEM_RATE, 100);
        let mut vpk = peaks::PeakBuilder::new(STEM_RATE, 100);
        let step = 48000i64;
        let kept: Vec<&StemSpan> = olds.iter().filter(|s| ranges::intersect(&[(s.start, s.end)], &good).iter().any(|(a, b)| b - a > 0.0)).collect();
        let mut k = ug0;
        while k < ug1 {
            if ctx.cancel.is_cancelled() {
                return Err(ApiError::new("CANCELLED", "cancelled"));
            }
            let ke = (k + step).min(ug1);
            let n = (ke - k) as usize;
            let mut orig = vec![0f32; n * 2];
            let mut voc = vec![0f32; n * 2];
            for s in &kept {
                let o = read_span(&dir, s, "orig.wav", k, ke)?;
                let v = read_span(&dir, s, "vocals.wav", k, ke)?;
                let (sa, sb) = (grid(s.start), grid(s.start) + s.frames as i64);
                for j in 0..n {
                    let g = k + j as i64;
                    if g >= sa && g < sb {
                        orig[2 * j] = o[2 * j];
                        orig[2 * j + 1] = o[2 * j + 1];
                        voc[2 * j] = v[2 * j];
                        voc[2 * j + 1] = v[2 * j + 1];
                    }
                }
            }
            for (m, r, f) in &news {
                let (ra, rb) = (grid(r.0), grid(r.1));
                let (a, b) = (k.max(ra), ke.min(rb));
                if b <= a {
                    continue;
                }
                let nv = wav::read_f32_range(f, (a - ra) as u64, (b - a) as u64)?.samples;
                let no = wav::read_f32_range(&new_orig, (a - g0) as u64, (b - a) as u64)?.samples;
                // outside m (inside r) the old pair is valid: missing = D − good
                for g in a..b {
                    let w = new_weight(g as f64 / RATE, *m, *r) as f32;
                    if w <= 0.0 {
                        continue;
                    }
                    let j = (g - k) as usize;
                    let x = ((g - a) * 2) as usize;
                    for ch in 0..2 {
                        orig[2 * j + ch] = (1.0 - w) * orig[2 * j + ch] + w * no[x + ch];
                        voc[2 * j + ch] = (1.0 - w) * voc[2 * j + ch] + w * nv[x + ch];
                    }
                }
            }
            opk.push_stereo(&orig);
            vpk.push_stereo(&voc);
            ow.write(&orig)?;
            vw.write(&voc)?;
            k = ke;
        }
        ow.finish()?;
        vw.finish()?;
        opk.finish().write(&out_dir.join("orig.peaks"))?;
        vpk.finish().write(&out_dir.join("vocals.peaks"))?;
        for (_, _, f) in &news {
            let _ = std::fs::remove_file(f);
        }
        let _ = std::fs::remove_file(&new_orig);
        let (model, device) = {
            let s = self.separator.lk();
            (ModelSpec::voc_ft().name, s.as_ref().map(|x| x.device).unwrap_or(crate::ml::runtime::Device::Cpu))
        };
        let span = StemSpan {
            id: id.clone(),
            start: ug0 as f64 / RATE,
            end: ug1 as f64 / RATE,
            frames: fin_frames,
            sample_rate: STEM_RATE,
            dir: rel,
            model,
            device,
            created_at: project::now(),
            version: crate::project::STEM_VERSION,
        };
        std::mem::forget(cleanup);
        res.separated = missing.clone();
        res.spans.push(id.clone());

        // 5) talk in parts never searched before → suggestions
        let scan = if p.settings.suggest_mutes { ranges::subtract(&missing, &p.talk_scanned) } else { Vec::new() };
        let talk_new = if scan.is_empty() {
            Vec::new()
        } else {
            (ctx.progress)(Progress::note("talk", "find_talk", "finding talk in the new part"));
            self.find_talk(&p, &dir, &span, &scan, &ctx.cancel)?
        };

        let old_ids: Vec<String> = olds.iter().map(|s| s.id.clone()).collect();
        let mut added = std::collections::BTreeMap::new();
        let rt = tokio::runtime::Handle::current();
        let span_c = span.clone();
        rt.block_on(self.mutate(pid, |p| {
            p.stems.retain(|s| !old_ids.contains(&s.id));
            p.stems.push(span_c);
            p.stems.sort_by(|a, b| a.start.total_cmp(&b.start));
            if !scan.is_empty() {
                p.talk = ranges::union(&p.talk, &talk_new);
                p.talk_scanned = ranges::union(&p.talk_scanned, &scan);
                added = add_suggestions(p, &talk_new);
            }
            Ok(())
        }))?;
        res.suggestions = added.values().sum();
        res.suggestions_by_song = added;
        // the old spans' files (folders under stems/ only; adopted clip folders keep their video)
        for s in &olds {
            for f in ["orig.wav", "vocals.wav", "orig.peaks", "vocals.peaks"] {
                let _ = std::fs::remove_file(dir.join(s.file(f)));
            }
            if s.dir.starts_with("stems/") {
                let _ = std::fs::remove_dir(dir.join(&s.dir));
            }
        }
        Ok(res)
    }

    /// Decode stretch audio covering `d` into `out` (48 kHz stereo float) on
    /// the VOD sample grid, starting one segment early so the decoder is warm.
    /// Returns the grid range written (may be shorter than `d` at the edges).
    fn decode_grid(&self, p: &Project, pieces: &[Piece], d: Range, out: &Path, cancel: &CancellationToken) -> ApiResult<(i64, i64)> {
        let sub = self.decode_pieces(p, pieces, d)?;
        if sub.is_empty() {
            return Err(precondition("no video for this range"));
        }
        let container = p.source.as_ref().map(|s| s.container).unwrap_or(crate::source::Container::Fmp4);
        let dir = self.store.dir(&p.id);
        // One decode per rendition, each placed by its own media clock (QA-2:
        // a rendition change continued by sample count put the second
        // rendition tens of ms off). Same-rendition pieces (an adopted clip
        // + new segments) stay one decode: that is bit-exact and warm.
        let mut groups: Vec<Vec<Piece>> = Vec::new();
        // an adopted clip used only in part (overlapping clips, R-1) or two
        // adopted clips in a row are decoded on their own: concatenating whole
        // files would repeat the overlap and shift everything after it
        for pc in sub {
            let legacy = matches!(pc, Piece::Legacy { .. });
            let joins = |g: &Vec<Piece>| {
                let two_clips = legacy && g.iter().any(|x| matches!(x, Piece::Legacy { .. }));
                g[0].variant() == pc.variant() && !pc.partial() && !g.last().unwrap().partial() && !two_clips
            };
            match groups.last_mut() {
                Some(g) if joins(g) => g.push(pc),
                _ => groups.push(vec![pc]),
            }
        }
        let (gs, ge) = (grid(d.0), grid(d.1));
        let mut w = GridWriter::new(out)?;
        let mut buf = vec![0f32; 1 << 16];
        for grp in &groups {
            if w.pos.is_some_and(|x| x >= ge) {
                break;
            }
            let og = grid(self.decode_origin(p, &grp[0])?);
            let mut dec = media::decode(&dir, container, grp, STEM_RATE, 2)?;
            let mut cur = og;
            let mut first = true;
            loop {
                if cancel.is_cancelled() {
                    return Err(ApiError::new("CANCELLED", "cancelled"));
                }
                let n = dec.pcm.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                let frames = (n / 2) as i64;
                // the part of [cur, cur + frames) to keep: from the write
                // position (or the range start) to the range end
                let from = w.pos.unwrap_or(gs.max(og)).max(cur).max(gs);
                let to = (cur + frames).min(ge);
                if to > from {
                    let x = &buf[((from - cur) * 2) as usize..((to - cur) * 2) as usize];
                    w.put(from, x, first)?;
                    first = false;
                }
                cur += frames;
                if cur >= ge {
                    break;
                }
            }
        }
        w.finish()
    }

    /// The pieces to decode for `d`: from one segment before it (decoder
    /// warm-up) to its end; legacy clips and local files whole.
    fn decode_pieces(&self, p: &Project, pieces: &[Piece], d: Range) -> ApiResult<Vec<Piece>> {
        if let [Piece::Local { .. }] = pieces {
            return Ok(pieces.to_vec());
        }
        let tl = self.with_ps(&p.id, |ps| ps.tl.clone())?.ok_or_else(|| precondition("fetch the audio first"))?;
        let segs = tl.segments_for(d.0, d.1, 0.0);
        let (s0, s1) = (segs.start.saturating_sub(1), segs.end);
        let mut out = Vec::new();
        for pc in pieces {
            if pc.last() <= s0 || pc.first() >= s1 {
                continue;
            }
            out.push(match pc {
                Piece::Raw { variant, first, last } => Piece::Raw { variant: *variant, first: (*first).max(s0), last: (*last).min(s1) },
                other => other.clone(),
            });
        }
        Ok(out)
    }

    /// Separate a WAV with the cached separator; on GPU out-of-memory free the
    /// detection model and retry, then fall back to the CPU. Blocking.
    pub(crate) fn separate_wav_fallback(&self, input: &Path, out: &Path, progress: &ProgressFn, cancel: &CancellationToken) -> ApiResult<u64> {
        use crate::ml::separator::{SeparateError, Separator};
        use crate::stems::StemError;
        let mut slot = self.separator.lk();
        let mut freed = false;
        loop {
            if slot.is_none() {
                *slot = Some(Separator::open(&self.cfg.models_dir, ModelSpec::voc_ft(), self.cfg.prefer_cpu)?);
            }
            match crate::stems::separate_wav(input, out, slot.as_mut().unwrap(), progress, cancel) {
                Err(StemError::Separate(SeparateError::OutOfMemory)) if !freed => {
                    freed = true;
                    *slot = None;
                    *self.tagger.lk() = None;
                    tracing::warn!("separation out of GPU memory; freeing the detection model and retrying");
                }
                Err(StemError::Separate(SeparateError::OutOfMemory)) => {
                    *slot = None;
                    progress(Progress::note("separate", "gpu_out_of_memory", "GPU out of memory; separating on the CPU"));
                    let mut cpu = Separator::open(&self.cfg.models_dir, ModelSpec::voc_ft(), true)?;
                    return Ok(crate::stems::separate_wav(input, out, &mut cpu, progress, cancel)?);
                }
                r => return Ok(r?),
            }
        }
    }

    /// Talk regions (VOD time) inside `scan`, from the span's stems. Blocking.
    fn find_talk(&self, p: &Project, dir: &Path, span: &StemSpan, scan: &[Range], cancel: &CancellationToken) -> ApiResult<Vec<Range>> {
        let tp = TalkParams::default();
        let songs: Vec<Range> = p.songs.iter().filter(|s| s.enabled).map(|s| (s.start, s.end)).collect();
        let mut out = Vec::new();
        for &(a, b) in scan {
            // some context so a region crossing the edge is seen whole
            let sa = (a - 5.0).max(span.start);
            let sb = (b + 5.0).min(span.end);
            if sb - sa < 1.0 {
                continue;
            }
            let section = Some((sa - span.start, sb - sa));
            let songs_rel: Vec<Range> = songs.iter().map(|&(x, y)| (x - sa, y - sa)).collect();
            let regs = {
                let mut slot = self.tagger.lk();
                let tagger = self.tagger(&mut slot)?;
                let v = talk::probs_of_file(tagger, &dir.join(span.file("vocals.wav")), section, cancel)?;
                let m = talk::probs_of_file(tagger, &dir.join(span.file("orig.wav")), section, cancel)?;
                talk::talk_regions(&v, &m, &songs_rel, &tp)
            };
            let regs = talk::refine_edges(&regs, &dir.join(span.file("vocals.wav")), section, &tp)?;
            let vod: Vec<Range> = regs.iter().map(|&(x, y)| (x + sa, y + sa)).collect();
            out.extend(ranges::drop_short(ranges::intersect(&vod, &[(a, b)]), 0.3));
        }
        Ok(ranges::normalize(&out))
    }

    /// Assemble the stems of `[a, b)` from the spans (interleaved stereo at
    /// 48 kHz, frame k = VOD `round(a·48000)/48000 + k/48000`). Err with the
    /// missing ranges when not fully separated. Blocking.
    pub(crate) fn read_stems(&self, p: &Project, a: f64, b: f64) -> ApiResult<(Vec<f32>, Vec<f32>)> {
        let have: Vec<Range> = p.stems.iter().map(|s| (s.start, s.end)).collect();
        let miss = ranges::drop_short(ranges::subtract(&[(a, b)], &have), 1.0 / RATE);
        if !miss.is_empty() {
            return Err(ApiError::new("PRECONDITION", "these ranges aren't separated yet").with_data(json!({ "ranges": miss })));
        }
        let dir = self.store.dir(&p.id);
        let (g0, g1) = (grid(a), grid(b));
        let n = (g1 - g0).max(0) as usize;
        let mut orig = vec![0f32; n * 2];
        let mut voc = vec![0f32; n * 2];
        // where spans overlap (capped spans, adopted clips), crossfade from the
        // earlier span to the later one across the overlap
        let mut spans: Vec<&StemSpan> = p.stems.iter().collect();
        spans.sort_by(|x, y| x.start.total_cmp(&y.start));
        let mut filled = i64::MIN;
        for s in spans {
            let (s0, s1) = (grid(s.start), grid(s.start) + s.frames as i64);
            let (sa, sb) = (s0.max(g0), s1.min(g1));
            if sb <= sa {
                filled = filled.max(s1);
                continue;
            }
            let o = read_span(&dir, s, "orig.wav", sa, sb)?;
            let v = read_span(&dir, s, "vocals.wav", sa, sb)?;
            let ov_end = filled.min(s1);
            for g in sa..sb {
                let j = ((g - g0) * 2) as usize;
                let x = ((g - sa) * 2) as usize;
                let w = if g < ov_end { raised((g - s0) as f64 / (ov_end - s0).max(1) as f64) as f32 } else { 1.0 };
                for ch in 0..2 {
                    orig[j + ch] = (1.0 - w) * orig[j + ch] + w * o[x + ch];
                    voc[j + ch] = (1.0 - w) * voc[j + ch] + w * v[x + ch];
                }
            }
            filled = filled.max(s1);
        }
        Ok((orig, voc))
    }
}

/// Split a target longer than `max` into pieces overlapping by `ovl`.
fn split_target(t: Range, max: f64, ovl: f64) -> Vec<Range> {
    let mut out = Vec::new();
    let mut a = t.0;
    loop {
        let b = (a + max).min(t.1);
        out.push((a, b));
        if b >= t.1 {
            return out;
        }
        a = b - ovl;
    }
}

/// Which nearby spans (indices into `near`) a new span over `t` merges with,
/// and the target trimmed to overlap the others by `ovl` only: merging stops
/// before the result would exceed `max` (R-2).
fn plan_span(t: Range, near: &[Range], max: f64, ovl: f64) -> (Vec<usize>, Range) {
    let mut order: Vec<usize> = (0..near.len()).collect();
    // closest first: those overlapping t most
    order.sort_by(|&a, &b| {
        let ov = |r: Range| (r.1.min(t.1) - r.0.max(t.0)).max(0.0);
        ov(near[b]).total_cmp(&ov(near[a]))
    });
    let mut hull = t;
    let mut merge = Vec::new();
    let mut t = t;
    for k in order {
        let r = near[k];
        let h = (hull.0.min(r.0), hull.1.max(r.1));
        if h.1 - h.0 <= max {
            hull = h;
            merge.push(k);
        } else if r.0 <= t.0 {
            t.0 = t.0.max(r.1 - ovl);
        } else {
            t.1 = t.1.min(r.0 + ovl);
        }
    }
    if t.1 < t.0 {
        t.1 = t.0;
    }
    merge.sort_unstable();
    (merge, t)
}

/// Mute suggestions for talk regions in songs (± margins), skipping what
/// overlaps the user's own or confirmed mutes. Returns how many were added.
pub(crate) fn add_suggestions(p: &mut Project, talk: &[Range]) -> std::collections::BTreeMap<String, usize> {
    let (fade, gain_db) = (p.settings.mute_fade, p.settings.mute_gain_db);
    let (mb, ma) = (p.settings.margin_before, p.settings.margin_after);
    let mut n = std::collections::BTreeMap::new();
    for s in p.songs.iter_mut().filter(|s| s.enabled) {
        let kept: Vec<Range> = s.mutes.iter().map(|m| (m.start, m.end)).collect();
        let fresh: Vec<Mute> = talk
            .iter()
            .filter(|&&(a, b)| b > s.start - mb && a < s.end + ma)
            .filter(|&&(a, b)| !kept.iter().any(|&(x, y)| a < y && x < b))
            // fades lie inside a mute: put them outside the speech so the voice
            // is fully removed from its first syllable on
            .map(|&(a, b)| Mute {
                id: project::new_id("m"),
                start: (a - fade).max(0.0),
                end: b + fade,
                fade_in: fade.min(a),
                fade_out: fade,
                origin: Origin::Auto,
                confirmed: false,
                gain_db,
            })
            .collect();
        if !fresh.is_empty() {
            n.insert(s.id.clone(), fresh.len());
        }
        s.mutes.extend(fresh);
        s.mutes.sort_by(|a, b| a.start.total_cmp(&b.start));
    }
    n
}

/// Writes decoded audio onto the sample grid: later pieces start where their
/// own clock puts them; an overlap keeps what was written first, a gap (the
/// two renditions' segments split the audio differently) becomes silence
/// with 5 ms fades on both sides so it doesn't click.
struct GridWriter {
    w: wav::WavWriter,
    /// grid index of the first frame written
    start: Option<i64>,
    /// next grid index to write
    pos: Option<i64>,
    /// last frames held back so they can be faded before a gap
    tail: Vec<f32>,
}

const FADE: usize = 240;

impl GridWriter {
    fn new(out: &Path) -> std::io::Result<Self> {
        Ok(Self { w: wav::WavWriter::create(out, STEM_RATE, 2)?, start: None, pos: None, tail: Vec::new() })
    }

    fn put(&mut self, at: i64, x: &[f32], new_piece: bool) -> std::io::Result<()> {
        let mut x = x.to_vec();
        match self.pos {
            None => self.start = Some(at),
            Some(p) if at > p => {
                // gap: fade out what came before, silence, fade in
                let n = self.tail.len() / 2;
                for k in 0..n.min(FADE) {
                    let g = (n.min(FADE) - k) as f32 / (n.min(FADE) as f32 + 1.0);
                    let j = n - n.min(FADE) + k;
                    self.tail[2 * j] *= g;
                    self.tail[2 * j + 1] *= g;
                }
                self.flush_tail()?;
                self.w.write(&vec![0f32; ((at - p) * 2) as usize])?;
                tracing::info!("stems: {} ms without audio where two renditions join", (at - p) as f64 / 48.0);
            }
            _ => {}
        }
        if new_piece && self.pos.is_some_and(|p| at > p) {
            let n = (x.len() / 2).min(FADE);
            for k in 0..n {
                let g = (k + 1) as f32 / (n as f32 + 1.0);
                x[2 * k] *= g;
                x[2 * k + 1] *= g;
            }
        }
        self.tail.extend_from_slice(&x);
        let keep = FADE * 2;
        if self.tail.len() > keep {
            let n = self.tail.len() - keep;
            self.w.write(&self.tail[..n])?;
            self.tail.drain(..n);
        }
        self.pos = Some(at + (x.len() / 2) as i64);
        Ok(())
    }

    fn flush_tail(&mut self) -> std::io::Result<()> {
        self.w.write(&self.tail)?;
        self.tail.clear();
        Ok(())
    }

    fn finish(mut self) -> ApiResult<(i64, i64)> {
        self.flush_tail()?;
        self.w.finish()?;
        Ok(match (self.start, self.pos) {
            (Some(a), Some(b)) => (a, b),
            _ => (0, 0),
        })
    }
}

/// Removes a half-written span folder unless defused with `mem::forget`.
struct Guard(PathBuf);

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn scopeguard(p: PathBuf) -> Guard {
    Guard(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_stop_merging_at_the_cap() {
        // R-2: a 40-min stretch of separated songs + a new song after it
        let near = [(0.0, 1500.0), (1500.0, 2400.0)];
        let (merge, t) = plan_span((2400.0, 2600.0), &near, 1800.0, 20.0);
        // the second span (900 s) merges (1100 s ≤ 1800), the first would exceed: overlap it by 20 s
        assert_eq!(merge, vec![1]);
        assert_eq!(t, (2400.0, 2600.0));
        let (merge, t) = plan_span((1400.0, 1700.0), &[(0.0, 1500.0), (1600.0, 3500.0)], 1800.0, 20.0);
        assert!(merge.len() == 1);
        assert!(t.1 - t.0 <= 300.0 && t.1 - t.0 > 0.0);
        let parts = split_target((0.0, 4000.0), 1760.0, 20.0);
        assert!(parts.iter().all(|r| r.1 - r.0 <= 1760.0));
        assert_eq!(parts.first().unwrap().0, 0.0);
        assert_eq!(parts.last().unwrap().1, 4000.0);
        assert!(parts.windows(2).all(|w| (w[0].1 - w[1].0 - 20.0).abs() < 1e-9));
    }

    #[test]
    fn crossfade_weights() {
        // new part [100, 120] separated over [90, 130]: switch at 95 / 125
        let (m, r) = ((100.0, 120.0), (90.0, 130.0));
        assert_eq!(new_weight(90.0, m, r), 0.0);
        assert_eq!(new_weight(94.0, m, r), 0.0);
        assert!((new_weight(95.0, m, r) - 0.5).abs() < 1e-9);
        assert_eq!(new_weight(96.0, m, r), 1.0);
        assert_eq!(new_weight(110.0, m, r), 1.0);
        assert!((new_weight(125.0, m, r) - 0.5).abs() < 1e-9);
        assert_eq!(new_weight(126.0, m, r), 0.0);
        // at the edge of the range: no context, new from the start
        assert_eq!(new_weight(100.0, (100.0, 120.0), (100.0, 130.0)), 1.0);
        assert_eq!(new_weight(99.0, (100.0, 120.0), (100.0, 130.0)), 0.0);
    }

    #[test]
    fn suggestions_skip_kept_mutes_and_other_songs() {
        let mut p = Project::new("t", "x");
        p.settings.margin_before = 5.0;
        p.settings.margin_after = 5.0;
        let mut s = crate::project::Song {
            id: "a".into(),
            title: String::new(),
            artist: String::new(),
            start: 10.0,
            end: 50.0,
            origin: Origin::Auto,
            enabled: true,
            reviewed: false,
            note: String::new(),
            confidence: None,
            mutes: vec![],
            mute_enabled: true,
            fade_in: None,
            fade_out: None,
            export: None,
            video_wanted: false,
            listened: false,
            extra: Default::default(),
        };
        s.mutes.push(Mute { id: "k".into(), start: 20.0, end: 22.0, fade_in: 0.1, fade_out: 0.1, origin: Origin::Manual, confirmed: true, gain_db: None });
        p.songs.push(s);
        let n = add_suggestions(&mut p, &[(6.0, 8.0), (21.0, 23.0), (30.0, 31.0), (80.0, 81.0)]);
        assert_eq!(n.get("a"), Some(&2));
        assert_eq!(p.songs[0].mutes.len(), 3);
    }
}
