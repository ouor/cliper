//! Disk usage per project and the cleanup levels (S-E8).
//!
//! Levels, lightest first (each includes nothing of the next):
//! * `unused`: video no enabled song's `[start − margin, end + margin]` touches
//!   (excluded songs don't count, unreviewed ones do — Q-S6), plus the view cache
//! * `stems`: separated audio (vocals / original stems)
//! * `video`: all video + separated audio (+ view cache)
//! * `audio`: all of the above + the analysis audio
//!
//! Decisions (songs, boundaries, mutes and their review state, settings,
//! export history) are never touched; exported files are never touched.
//! Nothing is ever deleted automatically.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::project::Project;
use crate::ranges::{self, Range};
use crate::video::store::{Have, HaveKind};
use crate::video::timeline::Timeline;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum DropLevel {
    Unused,
    Stems,
    Video,
    Audio,
}

/// Bytes on disk of one project, by kind.
#[derive(Debug, Clone, Serialize, Default)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct StorageUsage {
    pub project_id: String,
    /// analysis audio (+ its waveform/probabilities)
    pub audio: u64,
    /// downloaded video (segments + adopted clips)
    pub video: u64,
    /// separated audio
    pub stems: u64,
    /// part of `video` that no song uses (what level `unused` frees, without the cache)
    pub unused_video: u64,
    /// rebuildable caches (playable views, audio windows)
    pub cache: u64,
    /// exported files inside the project folder (never cleaned; not in `total`)
    pub exports: u64,
    /// audio + video + stems + cache
    pub total: u64,
    pub pinned: bool,
    /// seconds of video on disk
    pub video_sec: f64,
    pub updated_at: String,
}

/// Ranges the project's songs use: enabled songs ± margins.
pub fn used_ranges(p: &Project) -> Vec<Range> {
    let (mb, ma) = (p.settings.margin_before, p.settings.margin_after);
    ranges::normalize(&p.songs.iter().filter(|s| s.enabled).map(|s| ((s.start - mb).max(0.0), s.end + ma)).collect::<Vec<_>>())
}

/// What level `unused` deletes: raw segments outside `used`, and legacy clips
/// none of whose segments is used (a clip file can't be cut without
/// re-encoding, so a partly used one stays whole).
pub fn unused_selection(have: &BTreeMap<usize, Have>, tl: &Timeline, used: &[Range]) -> (Vec<usize>, BTreeSet<usize>) {
    let is_used = |i: usize| !ranges::intersect(&[(tl.start(i), tl.end(i))], used).is_empty();
    let mut raw = Vec::new();
    let mut legacy_used: BTreeMap<usize, bool> = BTreeMap::new();
    for (&i, h) in have {
        if !tl.contains(i) {
            continue;
        }
        match h.kind {
            HaveKind::Raw(_) => {
                if !is_used(i) {
                    raw.push(i);
                }
            }
            HaveKind::Legacy(n) => {
                *legacy_used.entry(n).or_insert(false) |= is_used(i);
            }
        }
    }
    let legacy = legacy_used.into_iter().filter(|(_, u)| !u).map(|(n, _)| n).collect();
    (raw, legacy)
}

/// Free bytes on the drive holding `path` (None if unknown).
pub fn free_space(path: &std::path::Path) -> Option<u64> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let mut p = path.to_path_buf();
        while !p.exists() {
            if !p.pop() {
                return None;
            }
        }
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let mut free: u64 = 0;
        // SAFETY: valid NUL-terminated path; out-pointers to locals
        let ok = unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, std::ptr::null_mut(), std::ptr::null_mut()) };
        if ok != 0 { Some(free) } else { None }
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Origin, Song};

    fn song(a: f64, b: f64, enabled: bool) -> Song {
        Song {
            id: crate::project::new_id("s"),
            title: String::new(),
            artist: String::new(),
            start: a,
            end: b,
            origin: Origin::Auto,
            enabled,
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
    fn unused_keeps_songs_with_margins_and_drops_excluded() {
        let mut p = Project::new("t", "x");
        p.settings.margin_before = 4.0;
        p.settings.margin_after = 4.0;
        p.songs = vec![song(20.0, 30.0, true), song(60.0, 70.0, false)];
        assert_eq!(used_ranges(&p), vec![(16.0, 34.0)]);
        let tl = Timeline::from_durations(0, &[10.0; 10]);
        let mut have = BTreeMap::new();
        for i in 0..8 {
            have.insert(i, Have { kind: HaveKind::Raw(0), bytes: 1 });
        }
        // a legacy clip partly used (segment 3 used) and one unused
        have.insert(3, Have { kind: HaveKind::Legacy(0), bytes: 1 });
        have.insert(4, Have { kind: HaveKind::Legacy(0), bytes: 1 });
        have.insert(8, Have { kind: HaveKind::Legacy(1), bytes: 1 });
        let (raw, legacy) = unused_selection(&have, &tl, &used_ranges(&p));
        // segments 1, 2 (16–34 s) and the legacy clip with 3 stay
        assert_eq!(raw, vec![0, 5, 6, 7]);
        assert_eq!(legacy.into_iter().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn levels_are_ordered() {
        assert!(DropLevel::Unused < DropLevel::Stems && DropLevel::Stems < DropLevel::Video && DropLevel::Video < DropLevel::Audio);
        assert!(free_space(&std::env::temp_dir()).is_none_or(|f| f > 0));
    }
}
