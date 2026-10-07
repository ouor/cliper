//! Setlist / fan-timestamp paste → song titles.
//!
//! Accepts the formats fans and streamers post, one entry per line:
//! `01:12:30 밤편지 - 아이유`, `1:23:40 夜に駆ける / YOASOBI`, `12. 곡명 - 가수`,
//! `곡명` … Lines with timestamps are matched to the nearest detected song
//! (fan timestamps are often off by a minute, or mark the intro talk);
//! lines without timestamps are assigned in order.

use serde::{Deserialize, Serialize};

use super::model::{Project, Song};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SetlistEntry {
    /// the original line
    pub line: String,
    /// VOD seconds, if the line had a timestamp
    pub time: Option<f64>,
    pub title: String,
    pub artist: String,
    /// matched song (None = no song near this entry)
    pub song_id: Option<String>,
    /// |timestamp − song start| for timestamped matches (s)
    pub distance: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SetlistMatch {
    pub entries: Vec<SetlistEntry>,
    /// number of songs whose title/artist were written
    pub applied: u32,
}

fn parse_time(tok: &str) -> Option<f64> {
    let parts: Vec<&str> = tok.split(':').collect();
    if !(2..=3).contains(&parts.len()) || parts.iter().any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_digit())) {
        return None;
    }
    let v: Vec<f64> = parts.iter().map(|p| p.parse().unwrap()).collect();
    Some(match v.len() {
        3 => v[0] * 3600.0 + v[1] * 60.0 + v[2],
        _ => v[0] * 60.0 + v[1],
    })
}

/// Split "title - artist" / "title / artist" (first separator surrounded by spaces).
fn split_title(s: &str) -> (String, String) {
    for sep in [" - ", " – ", " — ", " / "] {
        if let Some((a, b)) = s.split_once(sep) {
            return (a.trim().to_string(), b.trim().to_string());
        }
    }
    (s.trim().to_string(), String::new())
}

pub fn parse(text: &str) -> Vec<SetlistEntry> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let mut rest = line;
        // leading list markers: "1.", "01)", "-", "•"
        rest = rest.trim_start_matches(['-', '•', '*', '·']).trim_start();
        let num_end = rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len());
        if num_end > 0 && num_end < rest.len() && [".", ")"].iter().any(|p| rest[num_end..].starts_with(p)) {
            rest = rest[num_end + 1..].trim_start();
        }
        // timestamp anywhere at the start, optionally in brackets
        let tok_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let tok = rest[..tok_end].trim_matches(['[', ']', '(', ')']);
        let time = parse_time(tok);
        if time.is_some() {
            rest = rest[tok_end..].trim_start().trim_start_matches(['-', '|', '~']).trim_start();
        }
        if rest.is_empty() {
            continue;
        }
        let (title, artist) = split_title(rest);
        out.push(SetlistEntry { line: line.to_string(), time, title, artist, song_id: None, distance: None });
    }
    out
}

/// Assign entries to `songs` (enabled ones, by start). Timestamped entries take
/// the nearest song within `tolerance` s of its span (one song per entry, closest
/// pairs first); untimed entries fill the remaining songs in order.
pub fn match_songs(entries: &mut [SetlistEntry], songs: &[&Song], tolerance: f64) {
    let dist = |t: f64, s: &Song| if t >= s.start && t <= s.end { (t - s.start).abs().min(30.0) * 0.1 } else { (t - s.start).abs().min((t - s.end).abs()) };
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if let Some(t) = e.time {
            for (j, s) in songs.iter().enumerate() {
                let d = dist(t, s);
                if d <= tolerance {
                    pairs.push((d, i, j));
                }
            }
        }
    }
    pairs.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mut used = vec![false; songs.len()];
    for (_, i, j) in pairs {
        if entries[i].song_id.is_none() && !used[j] {
            used[j] = true;
            entries[i].song_id = Some(songs[j].id.clone());
            entries[i].distance = entries[i].time.map(|t| (t - songs[j].start).abs());
        }
    }
    // untimed entries: in order over the songs not yet taken
    let mut free = (0..songs.len()).filter(|&j| !used[j]);
    for e in entries.iter_mut().filter(|e| e.time.is_none()) {
        match free.next() {
            Some(j) => e.song_id = Some(songs[j].id.clone()),
            None => break,
        }
    }
}

/// Parse + match against `p`'s enabled songs; with `apply`, write titles/artists
/// (only into empty fields unless `overwrite`).
pub fn apply(p: &mut Project, text: &str, tolerance: f64, apply: bool, overwrite: bool) -> SetlistMatch {
    let mut entries = parse(text);
    let mut songs: Vec<&Song> = p.songs.iter().filter(|s| s.enabled).collect();
    songs.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
    match_songs(&mut entries, &songs, tolerance);
    let mut applied = 0;
    if apply {
        for e in &entries {
            let Some(id) = &e.song_id else { continue };
            if let Some(s) = p.song_mut(id) {
                let mut changed = false;
                if overwrite || s.title.trim().is_empty() {
                    s.title = e.title.clone();
                    changed = true;
                }
                if !e.artist.is_empty() && (overwrite || s.artist.trim().is_empty()) {
                    s.artist = e.artist.clone();
                    changed = true;
                }
                applied += changed as u32;
            }
        }
    }
    SetlistMatch { entries, applied }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::model::{Origin, Project, Song};

    fn song(id: &str, start: f64, end: f64) -> Song {
        Song {
            id: id.into(),
            title: String::new(),
            artist: String::new(),
            start,
            end,
            origin: Origin::Auto,
            enabled: true,
            reviewed: false,
            note: String::new(),
            confidence: None,
            clip_id: None,
            mutes: vec![],
            export: None,
        }
    }

    #[test]
    fn parses_common_formats() {
        let e = parse("01:12:30 밤편지 - 아이유\n2. [1:23:40] 夜に駆ける / YOASOBI\n• Melt\n\n12:05 - 첫 곡");
        assert_eq!(e.len(), 4);
        assert_eq!((e[0].time, e[0].title.as_str(), e[0].artist.as_str()), (Some(4350.0), "밤편지", "아이유"));
        assert_eq!((e[1].time, e[1].title.as_str(), e[1].artist.as_str()), (Some(5020.0), "夜に駆ける", "YOASOBI"));
        assert_eq!((e[2].time, e[2].title.as_str()), (None, "Melt"));
        assert_eq!((e[3].time, e[3].title.as_str()), (Some(725.0), "첫 곡"));
    }

    #[test]
    fn matches_nearest_and_in_order() {
        let mut p = Project::new("t", "x");
        p.songs = vec![song("a", 100.0, 300.0), song("b", 400.0, 600.0), song("c", 700.0, 900.0)];
        // fan timestamp 60 s early for b; a given inside the song; one untimed line
        let r = apply(&mut p, "1:45 A - x\n5:40 B - y\nC", 150.0, true, false);
        assert_eq!(r.applied, 3);
        assert_eq!(p.songs[0].title, "A");
        assert_eq!(p.songs[1].title, "B");
        assert_eq!(p.songs[2].title, "C");
        assert_eq!(p.songs[1].artist, "y");
    }
}
