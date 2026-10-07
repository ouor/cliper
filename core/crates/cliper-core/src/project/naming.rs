//! Output file naming: user template → safe file name, never overwriting.

use std::path::{Path, PathBuf};

use super::model::{Project, Song};

/// Characters Windows forbids in file names (plus control chars).
const FORBIDDEN: &str = "\\/:*?\"<>|";

pub fn sanitize(s: &str) -> String {
    let cleaned: String = s.chars().map(|c| if FORBIDDEN.contains(c) || c.is_control() { '_' } else { c }).collect();
    // no trailing dots/spaces on Windows, no empty names
    let t = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    if t.is_empty() { "_".into() } else { t }
}

fn hms_dash(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{}-{:02}-{:02}", t / 3600, t / 60 % 60, t % 60)
}

/// Broadcast date as `YYYY-MM-DD` if the platform gave one.
pub fn broadcast_date(p: &Project) -> Option<String> {
    let raw = p.source.as_ref()?.catalog.published_at.clone()?;
    let d: String = raw.chars().take(10).collect();
    (d.len() == 10 && d.as_bytes()[4] == b'-' && d.as_bytes()[7] == b'-').then_some(d)
}

/// Expand `template` for `song` (number = its 1-based position among enabled songs).
pub fn render_name(template: &str, p: &Project, song: &Song, number: usize) -> String {
    let src = p.source.as_ref();
    let date = broadcast_date(p).unwrap_or_default();
    let yymmdd: String = date.replace('-', "").chars().skip(2).collect();
    let title = if song.title.trim().is_empty() { hms_dash(song.start) } else { song.title.trim().to_string() };
    let pairs: [(&str, String); 10] = [
        ("{date}", date.clone()),
        ("{yymmdd}", yymmdd),
        ("{platform}", src.map(|s| s.platform.as_str().to_string()).unwrap_or_default()),
        ("{vod_id}", src.map(|s| s.video_id.clone()).unwrap_or_default()),
        ("{channel}", src.map(|s| s.channel.clone()).unwrap_or_default()),
        ("{vod_title}", src.map(|s| s.title.clone()).unwrap_or_default()),
        ("{idx}", format!("{number:02}")),
        ("{title}", title),
        ("{artist}", song.artist.trim().to_string()),
        ("{start}", hms_dash(song.start)),
    ];
    let mut out = template.to_string();
    for (k, v) in pairs {
        let v: String = v.chars().map(|c| if FORBIDDEN.contains(c) || c.is_control() { '_' } else { c }).collect();
        out = out.replace(k, &v);
    }
    // tidy separators left by empty tokens: "a__b", "a_-_b", trailing "_"
    let mut s = out;
    for (a, b) in [("__", "_"), ("()", ""), ("[]", ""), ("  ", " ")] {
        while s.contains(a) {
            s = s.replace(a, b);
        }
    }
    sanitize(s.trim_matches(['_', '-', ' ']))
}

/// `dir/name.ext`, or `dir/name_v2.ext`, `_v3`… if taken.
pub fn unique_path(dir: &Path, name: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{name}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|v| dir.join(format!("{name}_v{v}.{ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::model::{Origin, Song};

    fn song(title: &str, artist: &str) -> Song {
        Song {
            id: "s".into(),
            title: title.into(),
            artist: artist.into(),
            start: 3723.0,
            end: 3900.0,
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
        }
    }

    #[test]
    fn template_and_sanitize() {
        let p = Project::new("x", "y");
        assert_eq!(render_name("{idx}_{title}_{artist}", &p, &song("夜に駆ける: Yoru?", "YOASOBI"), 3), "03_夜に駆ける_ Yoru__YOASOBI".replace("__", "_"));
        assert_eq!(render_name("{idx}_{title}_{artist}", &p, &song("", ""), 1), "01_1-02-03");
    }

    #[test]
    fn versions() {
        let d = std::env::temp_dir().join(format!("cliper-naming-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let a = unique_path(&d, "x", "mp4");
        std::fs::write(&a, b"1").unwrap();
        assert!(unique_path(&d, "x", "mp4").ends_with("x_v2.mp4"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
