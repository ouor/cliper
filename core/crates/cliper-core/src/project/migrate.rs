//! `cliper/project@1` (one video file per clip) → `@2` (segment store, S-E11).
//!
//! Nothing is downloaded or re-separated: a clip's `source.mp4` becomes a
//! legacy piece of the segment store (it covers exactly the HLS segments it
//! was remuxed from) and its stems become a [`StemSpan`] in place
//! (`clips/<id>/orig.wav`, `vocals.wav` — same files, same time base: file
//! t = VOD `vod_start + offset + t`). Talk regions and the clip's range move
//! to `talk` / `talk_scanned`, so suggestions the user already deleted don't
//! come back. Old absolute paths are ignored (the folder may have moved);
//! everything is resolved inside the project folder.

use std::path::Path;

use super::model::{Project, SCHEMA, StemSpan};
use crate::ranges;
use crate::stems::STEM_RATE;
use crate::video::store::{LegacyPiece, VideoIndex};

/// Migrate in memory + write `video/index.json`. Returns true if `p` changed
/// (the caller saves it).
pub fn migrate(p: &mut Project, dir: &Path) -> std::io::Result<bool> {
    if p.schema == SCHEMA {
        return Ok(false);
    }
    let local = p.source.as_ref().is_some_and(|s| s.platform == crate::source::Platform::Local);
    let mut idx = VideoIndex::load(dir);
    let mut scanned = Vec::new();
    for c in std::mem::take(&mut p.clips) {
        let Some(v) = &c.video else { continue };
        let rel = format!("clips/{}", c.id);
        let origin = v.vod_start + c.offset;
        if !local && dir.join(&rel).join("source.mp4").is_file() && v.segments.last > v.segments.first && !idx.legacy.iter().any(|l| l.id == c.id) {
            idx.legacy.push(LegacyPiece {
                id: c.id.clone(),
                file: format!("{rel}/source.mp4"),
                variant: v.variant,
                first: v.segments.first,
                last: v.segments.last,
                origin,
                resolution: v.resolution.clone(),
            });
        }
        if let Some(s) = &c.stems
            && dir.join(&rel).join("orig.wav").is_file()
            && dir.join(&rel).join("vocals.wav").is_file()
        {
            let rate = s.sample_rate.max(1);
            let start = (origin * rate as f64).round() / rate as f64;
            p.stems.push(StemSpan {
                id: format!("x{}", c.id),
                start,
                end: start + s.frames as f64 / rate as f64,
                frames: s.frames,
                sample_rate: rate,
                dir: rel.clone(),
                model: s.model.clone(),
                device: s.device,
                created_at: p.updated_at.clone(),
            });
            scanned.push((origin, origin + v.duration));
        }
        p.talk.extend(c.talk.iter().copied());
    }
    debug_assert!(p.stems.iter().all(|s| s.sample_rate == STEM_RATE));
    p.stems.sort_by(|a, b| a.start.total_cmp(&b.start));
    p.talk = ranges::normalize(&p.talk);
    p.talk_scanned = ranges::union(&p.talk_scanned, &scanned);
    for s in p.songs.iter_mut() {
        s.clip_id = None;
    }
    idx.legacy.sort_by_key(|l| l.first);
    idx.save(dir)?;
    p.schema = SCHEMA.into();
    Ok(true)
}
