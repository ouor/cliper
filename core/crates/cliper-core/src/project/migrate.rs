//! Schema migrations of `project.json`, run as a chain on the raw JSON:
//!
//! * `@1 → @2` (segment store, S-E11): nothing is downloaded or re-separated.
//!   A clip's `source.mp4` becomes a legacy piece of the segment store (it
//!   covers exactly the HLS segments it was remuxed from; a clip whose
//!   segments another clip fully holds is left out, R-1) and its stems become
//!   a [`StemSpan`] in place (`clips/<id>/orig.wav`, `vocals.wav` — same files,
//!   same time base: file t = VOD `vod_start + offset + t`). Talk regions and
//!   the clip's range move to `talk` / `talk_scanned`, so suggestions the user
//!   already deleted don't come back. Old absolute paths are ignored (the
//!   folder may have moved); everything is resolved inside the project folder.
//! * `@2 → @3`: the protocol-2 leftovers go (`clips`, `Song.clip_id`); the
//!   project gets its `uid`.
//!
//! The caller backs up the original file first and refuses newer schemas.

use std::path::Path;

use serde::Deserialize;
use serde_json::{Value, json};

use super::model::{SCHEMA, StemSpan, new_uid};
use crate::ranges::{self, Range};
use crate::video::store::{LegacyPiece, VideoIndex};

/// The schema version of `cliper/project@N` (absent = 1).
pub fn version_of(v: &Value) -> u32 {
    v.get("schema").and_then(Value::as_str).and_then(|s| s.rsplit_once('@')).and_then(|(_, n)| n.parse().ok()).unwrap_or(1)
}

/// The version this engine writes.
pub fn current() -> u32 {
    SCHEMA.rsplit_once('@').and_then(|(_, n)| n.parse().ok()).unwrap_or(0)
}

/// Bring `v` from `from` up to the current schema.
pub fn chain(v: &mut Value, from: u32, dir: &Path) -> std::io::Result<()> {
    if from <= 1 {
        m1_to_2(v, dir)?;
    }
    if from <= 2 {
        m2_to_3(v);
    }
    v["schema"] = json!(SCHEMA);
    Ok(())
}

// ── @1 shapes (read only) ───────────────────────────────────────────────

#[derive(Deserialize)]
struct SegRange1 {
    first: usize,
    last: usize,
}

#[derive(Deserialize)]
struct Video1 {
    variant: usize,
    #[serde(default)]
    resolution: String,
    segments: SegRange1,
    vod_start: f64,
    duration: f64,
}

#[derive(Deserialize)]
struct Stems1 {
    sample_rate: u32,
    frames: u64,
    #[serde(default)]
    model: String,
    device: crate::ml::runtime::Device,
}

#[derive(Deserialize)]
struct Clip1 {
    id: String,
    video: Option<Video1>,
    #[serde(default)]
    offset: f64,
    stems: Option<Stems1>,
    #[serde(default)]
    talk: Vec<Range>,
}

fn m1_to_2(v: &mut Value, dir: &Path) -> std::io::Result<()> {
    let local = v["source"]["platform"].as_str() == Some("local");
    let clips: Vec<Clip1> = serde_json::from_value(v.get("clips").cloned().unwrap_or(json!([]))).map_err(std::io::Error::other)?;
    let updated_at = v["updated_at"].as_str().unwrap_or_default().to_string();
    let mut idx = VideoIndex::load(dir).map_err(std::io::Error::other)?;
    let mut stems: Vec<StemSpan> = serde_json::from_value(v.get("stems").cloned().unwrap_or(json!([]))).unwrap_or_default();
    let mut talk: Vec<Range> = serde_json::from_value(v.get("talk").cloned().unwrap_or(json!([]))).unwrap_or_default();
    let mut scanned: Vec<Range> = serde_json::from_value(v.get("talk_scanned").cloned().unwrap_or(json!([]))).unwrap_or_default();
    let mut pieces: Vec<LegacyPiece> = Vec::new();
    for c in clips {
        let Some(vid) = &c.video else { continue };
        let rel = format!("clips/{}", c.id);
        let origin = vid.vod_start + c.offset;
        if !local && dir.join(&rel).join("source.mp4").is_file() && vid.segments.last > vid.segments.first && !idx.legacy.iter().any(|l| l.id == c.id) {
            pieces.push(LegacyPiece {
                id: c.id.clone(),
                file: format!("{rel}/source.mp4"),
                variant: vid.variant,
                first: vid.segments.first,
                last: vid.segments.last,
                origin,
                resolution: vid.resolution.clone(),
                key: None,
            });
        }
        if let Some(s) = &c.stems
            && dir.join(&rel).join("orig.wav").is_file()
            && dir.join(&rel).join("vocals.wav").is_file()
        {
            let rate = s.sample_rate.max(1);
            let start = (origin * rate as f64).round() / rate as f64;
            stems.push(StemSpan {
                id: format!("x{}", c.id),
                start,
                end: start + s.frames as f64 / rate as f64,
                frames: s.frames,
                sample_rate: rate,
                dir: rel.clone(),
                model: s.model.clone(),
                device: s.device,
                created_at: updated_at.clone(),
                version: 0,
            });
            scanned.push((origin, origin + vid.duration));
        }
        talk.extend(c.talk.iter().copied());
    }
    // a clip whose segments another clip of the same rendition fully holds
    // adds nothing but a second copy (R-1)
    let all = pieces.clone();
    pieces.retain(|a| !all.iter().any(|b| b.id != a.id && b.variant == a.variant && b.first <= a.first && a.last <= b.last && (b.last - b.first > a.last - a.first || b.id < a.id)));
    idx.legacy.extend(pieces);
    idx.legacy.sort_by_key(|l| l.first);
    stems.sort_by(|a, b| a.start.total_cmp(&b.start));
    v["stems"] = serde_json::to_value(&stems).map_err(std::io::Error::other)?;
    v["talk"] = serde_json::to_value(ranges::normalize(&talk)).map_err(std::io::Error::other)?;
    v["talk_scanned"] = serde_json::to_value(ranges::union(&scanned, &[])).map_err(std::io::Error::other)?;
    idx.save(dir)?;
    Ok(())
}

fn m2_to_3(v: &mut Value) {
    if let Some(o) = v.as_object_mut() {
        o.remove("clips");
        if o.get("uid").and_then(Value::as_str).is_none_or(str::is_empty) {
            o.insert("uid".into(), json!(new_uid()));
        }
    }
    if let Some(songs) = v.get_mut("songs").and_then(Value::as_array_mut) {
        for s in songs {
            if let Some(o) = s.as_object_mut() {
                o.remove("clip_id");
            }
        }
    }
}
