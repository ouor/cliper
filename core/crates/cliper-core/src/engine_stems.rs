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
/// Old original audio must match the fresh decode to this (max abs, per block).
const MATCH_TOL: f32 = 1e-4;
/// Comparison block (frames, 0.1 s).
const BLOCK: u64 = 4800;
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
        {
            let mut q = self.pending_process.lk();
            let e = q.entry(pid.to_string()).or_default();
            *e = ranges::union(e, &rs);
        }
        self.kick_process(pid);
    }

    fn kick_process(self: &Arc<Self>, pid: &str) {
        if self.jobs.lk().values().any(|j| j.info.project_id == pid && j.info.kind == "stems.ensure") {
            return;
        }
        let Some(rs) = self.pending_process.lk().remove(pid) else { return };
        if rs.is_empty() {
            return;
        }
        let r = self.job_stems(pid, None, Some(rs.clone()));
        if let Err(e) = r {
            tracing::warn!("{pid}: separating fetched video: {e}");
            if e.code == "BUSY" {
                self.pending_process.lk().insert(pid.to_string(), rs);
            }
        }
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
        for (crange, pieces) in stretches {
            let want = ranges::drop_short(ranges::intersect(targets, &[crange]), 0.5);
            for t in want {
                if ranges::covers(&done, t, 0.01) {
                    continue;
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
            }
        }
        res.separated = ranges::normalize(&res.separated);
        Ok(res)
    }

    /// Separate the missing parts of `t` (inside stretch `crange` = `pieces`). Blocking.
    fn ensure_one(&self, pid: &str, t: Range, crange: Range, pieces: &[Piece], ctx: &JobCtx) -> ApiResult<EnsureResult> {
        let p = self.get(pid)?;
        let dir = self.store.dir(pid);
        // spans t touches → result range U
        let olds: Vec<StemSpan> =
            p.stems.iter().filter(|s| s.start <= t.1 + 0.5 && s.end >= t.0 - 0.5 && s.start < crange.1 && s.end > crange.0).cloned().collect();
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
        let mut good: Vec<(i64, i64)> = Vec::new();
        let mut res = EnsureResult::default();
        for s in &olds {
            let (sa, sb) = (grid(s.start), grid(s.start) + s.frames as i64);
            let (a, b) = (sa.max(g0), sb.min(g1));
            let (mut same, mut total) = (0u64, 0u64);
            // every 5th 0.1 s block, past the old decode's possible cold start
            let mut k = a + BLOCK as i64 * 2;
            while k + (BLOCK as i64) <= b {
                let old = read_span(&dir, s, "orig.wav", k, k + BLOCK as i64)?;
                let new = wav::read_f32_range(&new_orig, (k - g0) as u64, BLOCK)?.samples;
                total += old.len() as u64;
                same += old.iter().zip(&new).filter(|(x, y)| (*x - *y).abs() <= MATCH_TOL).count() as u64;
                k += BLOCK as i64 * 5;
            }
            let ok = total == 0 || same as f64 >= 0.5 * total as f64;
            tracing::info!("{pid}: old span {} matches the fresh decode in {:.1}% of samples", s.id, 100.0 * same as f64 / total.max(1) as f64);
            if ok {
                good.push((sa, sb));
            } else {
                tracing::warn!("{pid}: span {} doesn't match the video's audio; separating it again", s.id);
                (ctx.progress)(Progress::note("separate", "stems_reseparated", "old stems don't match the video; separating them again"));
                res.reseparated.push(((a as f64) / RATE, (b as f64) / RATE));
                // what lies outside the video we have can't be redone: keep it
                if sa < g0 {
                    good.push((sa, g0));
                }
                if sb > g1 {
                    good.push((g1, sb));
                }
            }
        }
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
        let (ug0, ug1) = (grid(u.0).min(g0), grid(u.1).max(g1));
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
        };
        std::mem::forget(cleanup);
        res.separated = missing.clone();
        res.spans.push(id.clone());

        // 5) talk in parts never searched before → suggestions
        let scan = if p.settings.suggest_mutes { ranges::subtract(&missing, &p.talk_scanned) } else { Vec::new() };
        let talk_new = if scan.is_empty() { Vec::new() } else { self.find_talk(&p, &dir, &span, &scan, &ctx.cancel)? };

        let old_ids: Vec<String> = olds.iter().map(|s| s.id.clone()).collect();
        let mut added = 0usize;
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
        res.suggestions = added;
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
        let first = sub.first().ok_or_else(|| precondition("no video for this range"))?;
        let origin = self.decode_origin(p, first)?;
        let container = p.source.as_ref().map(|s| s.container).unwrap_or(crate::source::Container::Fmp4);
        let dir = self.store.dir(&p.id);
        let mut dec = media::decode(&dir, container, &sub, STEM_RATE, 2)?;
        let og = grid(origin);
        let (g0, g1) = (grid(d.0).max(og), grid(d.1));
        let mut skip = ((g0 - og) * 2) as usize;
        let mut w = wav::WavWriter::create(out, STEM_RATE, 2)?;
        let want = ((g1 - g0).max(0) * 2) as usize;
        let mut got = 0usize;
        let mut buf = vec![0f32; 1 << 16];
        while got < want {
            if cancel.is_cancelled() {
                return Err(ApiError::new("CANCELLED", "cancelled"));
            }
            let n = dec.pcm.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let mut x = &buf[..n];
            if skip > 0 {
                let s = skip.min(x.len());
                skip -= s;
                x = &x[s..];
            }
            let take = x.len().min(want - got);
            w.write(&x[..take])?;
            got += take;
        }
        w.finish()?;
        Ok((g0, g0 + (got / 2) as i64))
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
        // later spans win where spans overlap (migrated projects)
        for s in &p.stems {
            let (sa, sb) = (grid(s.start).max(g0), (grid(s.start) + s.frames as i64).min(g1));
            if sb <= sa {
                continue;
            }
            let o = read_span(&dir, s, "orig.wav", sa, sb)?;
            let v = read_span(&dir, s, "vocals.wav", sa, sb)?;
            let off = ((sa - g0) * 2) as usize;
            orig[off..off + o.len()].copy_from_slice(&o);
            voc[off..off + v.len()].copy_from_slice(&v);
        }
        Ok((orig, voc))
    }
}

/// Mute suggestions for talk regions in songs (± margins), skipping what
/// overlaps the user's own or confirmed mutes. Returns how many were added.
pub(crate) fn add_suggestions(p: &mut Project, talk: &[Range]) -> usize {
    let (fade, gain_db) = (p.settings.mute_fade, p.settings.mute_gain_db);
    let (mb, ma) = (p.settings.margin_before, p.settings.margin_after);
    let mut n = 0;
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
        n += fresh.len();
        s.mutes.extend(fresh);
        s.mutes.sort_by(|a, b| a.start.total_cmp(&b.start));
    }
    n
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
            clip_id: None,
            mutes: vec![],
            mute_enabled: true,
            fade_in: None,
            fade_out: None,
            export: None,
        };
        s.mutes.push(Mute { id: "k".into(), start: 20.0, end: 22.0, fade_in: 0.1, fade_out: 0.1, origin: Origin::Manual, confirmed: true, gain_db: None });
        p.songs.push(s);
        let n = add_suggestions(&mut p, &[(6.0, 8.0), (21.0, 23.0), (30.0, 31.0), (80.0, 81.0)]);
        assert_eq!(n, 2);
        assert_eq!(p.songs[0].mutes.len(), 3);
    }
}
