//! Song list → data for spreadsheets, wikis and YouTube descriptions.
//!
//! All times are on the **original VOD** timeline (what archives and fans
//! reference), never the cut file's.

use serde::{Deserialize, Serialize};

use super::model::{Project, Song};
use super::naming::broadcast_date;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum ReportFormat {
    /// UTF-8 with BOM (opens correctly in Excel), one row per song
    Csv,
    /// array of song objects
    Json,
    /// YouTube chapters for a full-VOD upload (`00:00 …` first)
    Chapters,
    /// `hh:mm:ss 곡명 - 원곡자` per song (setlist / pinned comment)
    Timestamps,
}

impl ReportFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            ReportFormat::Csv => "csv",
            ReportFormat::Json => "json",
            _ => "txt",
        }
    }
}

fn hms(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

fn label(s: &Song) -> String {
    match (s.title.trim(), s.artist.trim()) {
        ("", _) => format!("노래 {}", hms(s.start)),
        (t, "") => t.to_string(),
        (t, a) => format!("{t} - {a}"),
    }
}

fn csv_field(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) { format!("\"{}\"", v.replace('"', "\"\"")) } else { v.to_string() }
}

/// URL that opens the VOD at `t` where the platform supports it.
pub fn timed_url(p: &Project, t: f64) -> String {
    let Some(src) = &p.source else { return String::new() };
    let secs = t.max(0.0) as u64;
    match src.platform {
        crate::source::Platform::Soop => format!("{}?change_second={secs}", src.url),
        crate::source::Platform::Chzzk => format!("{}?currentTime={secs}", src.url),
        crate::source::Platform::Local => src.url.clone(),
    }
}

pub fn render(p: &Project, format: ReportFormat, include_disabled: bool) -> String {
    let songs: Vec<&Song> = p.songs.iter().filter(|s| include_disabled || s.enabled).collect();
    let src = p.source.as_ref();
    let date = broadcast_date(p).unwrap_or_default();
    match format {
        ReportFormat::Timestamps => songs.iter().map(|s| format!("{} {}\n", hms(s.start), label(s))).collect(),
        ReportFormat::Chapters => {
            let mut out = String::new();
            if songs.first().is_none_or(|s| s.start >= 1.0) {
                out.push_str("00:00:00 시작\n");
            }
            for s in &songs {
                out.push_str(&format!("{} {}\n", hms(s.start), label(s)));
            }
            out
        }
        ReportFormat::Json | ReportFormat::Csv => {
            let rows: Vec<serde_json::Value> = songs
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    serde_json::json!({
                        "platform": src.map(|x| x.platform.as_str()).unwrap_or_default(),
                        "vod_id": src.map(|x| x.video_id.clone()).unwrap_or_default(),
                        "vod_url": src.map(|x| x.url.clone()).unwrap_or_default(),
                        "vod_title": src.map(|x| x.title.clone()).unwrap_or_default(),
                        "date": date,
                        "channel": src.map(|x| x.channel.clone()).unwrap_or_default(),
                        "index": i + 1,
                        "title": s.title,
                        "artist": s.artist,
                        "start_sec": (s.start * 1000.0).round() / 1000.0,
                        "end_sec": (s.end * 1000.0).round() / 1000.0,
                        "start": hms(s.start),
                        "end": hms(s.end),
                        "duration_sec": ((s.end - s.start) * 10.0).round() / 10.0,
                        "timed_url": timed_url(p, s.start),
                        "enabled": s.enabled,
                        "reviewed": s.reviewed,
                        "note": s.note,
                        "mutes_applied": s.mutes.iter().filter(|m| m.confirmed).count(),
                        "exported_file": s.export.as_ref().map(|e| e.path.clone()).unwrap_or_default(),
                    })
                })
                .collect();
            if format == ReportFormat::Json {
                return serde_json::to_string_pretty(&rows).unwrap_or_default();
            }
            let cols = [
                "platform", "vod_id", "vod_url", "vod_title", "date", "channel", "index", "title", "artist", "start_sec", "end_sec",
                "start", "end", "duration_sec", "timed_url", "enabled", "reviewed", "note", "mutes_applied", "exported_file",
            ];
            let mut out = String::from("\u{feff}");
            out.push_str(&cols.join(","));
            out.push('\n');
            for r in rows {
                let line: Vec<String> = cols
                    .iter()
                    .map(|c| match &r[*c] {
                        serde_json::Value::String(s) => csv_field(s),
                        v => v.to_string(),
                    })
                    .collect();
                out.push_str(&line.join(","));
                out.push('\n');
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::model::{Origin, Project, Song};

    #[test]
    fn chapters_start_at_zero() {
        let mut p = Project::new("t", "x");
        p.songs.push(Song {
            id: "a".into(),
            title: "밤편지".into(),
            artist: "아이유".into(),
            start: 4350.0,
            end: 4600.0,
            origin: Origin::Auto,
            enabled: true,
            reviewed: true,
            note: "a, \"b\"".into(),
            confidence: None,
            clip_id: None,
            mutes: vec![],
            export: None,
        });
        let c = render(&p, ReportFormat::Chapters, false);
        assert_eq!(c, "00:00:00 시작\n01:12:30 밤편지 - 아이유\n");
        let csv = render(&p, ReportFormat::Csv, false);
        assert!(csv.starts_with('\u{feff}'));
        assert!(csv.contains("\"a, \"\"b\"\"\""));
    }
}
