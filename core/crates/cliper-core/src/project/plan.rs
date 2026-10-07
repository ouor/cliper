//! Clip planning: songs ± margins → merged VOD ranges to fetch as video.
//!
//! Detection boundaries can be tight (they sit on the music/speech threshold
//! and may clip intros/outros), so every song gets margins; overlapping ranges
//! merge into one clip so shared footage is downloaded once.

use super::model::{Clip, Project, new_id};

/// Merged clips stop growing past this length (songs whose margins overlap
/// otherwise chain into hour-long clips); a single longer song still gets
/// one clip.
pub const MAX_CLIP_SEC: f64 = 20.0 * 60.0;

/// Group `(index, start, end)` ranges (sorted by start) into clips: overlapping
/// ranges merge unless the clip would exceed [`MAX_CLIP_SEC`].
fn group(ranges: Vec<(usize, f64, f64)>) -> Vec<(f64, f64, Vec<usize>)> {
    let mut groups: Vec<(f64, f64, Vec<usize>)> = Vec::new();
    for (i, a, b) in ranges {
        match groups.last_mut() {
            Some(g) if a <= g.1 && b.max(g.1) - g.0 <= MAX_CLIP_SEC => {
                g.1 = g.1.max(b);
                g.2.push(i);
            }
            _ => groups.push((a, b, vec![i])),
        }
    }
    groups
}

/// Plan clips for every enabled song. Songs inside a downloaded clip (or a
/// pinned one, see [`pin_exact`]) keep it; the rest are grouped into new (not
/// yet fetched) clips. Clips that no longer hold any song and have no video
/// are dropped. Every clip's `song_ids` is refreshed.
pub fn plan_clips(p: &mut Project) {
    let dur = p.audio.as_ref().map(|a| a.duration).or(p.source.as_ref().map(|s| s.duration)).unwrap_or(f64::MAX);
    let (mb, ma) = (p.settings.margin_before, p.settings.margin_after);

    // 1) keep songs whose range is covered by a fetched clip, or by a pinned
    //    clip they were assigned to
    let mut pending: Vec<(usize, f64, f64)> = Vec::new();
    for (i, s) in p.songs.iter_mut().enumerate() {
        if !s.enabled {
            s.clip_id = None;
            continue;
        }
        let fetched = p.clips.iter().find(|c| c.covered().is_some_and(|(a, b)| a <= s.start && s.end <= b));
        let pinned = || {
            p.clips.iter().find(|c| c.video.is_none() && c.pinned && s.clip_id.as_deref() == Some(&c.id) && c.start <= s.start && s.end <= c.end)
        };
        match fetched.or_else(pinned) {
            Some(c) => s.clip_id = Some(c.id.clone()),
            None => {
                s.clip_id = None;
                pending.push((i, (s.start - mb).max(0.0), (s.end + ma).min(dur)));
            }
        }
    }

    // 2) merge pending ranges (capped length)
    pending.sort_by(|a, b| a.1.total_cmp(&b.1));
    let groups = group(pending);

    // 3) drop unpinned unfetched clips (they get re-planned) and pinned ones
    //    left without songs; fetched ones stay for reuse
    // ids of planned clips are reused when the same range comes back, so
    // re-planning after an unrelated edit doesn't rename every clip
    let old: Vec<(f64, f64, String)> = p.clips.iter().filter(|c| c.video.is_none() && !c.pinned).map(|c| (c.start, c.end, c.id.clone())).collect();
    let hosted: Vec<String> = p.songs.iter().filter(|s| s.enabled).filter_map(|s| s.clip_id.clone()).collect();
    p.clips.retain(|c| c.video.is_some() || (c.pinned && hosted.contains(&c.id)));
    for s in p.songs.iter_mut() {
        if s.clip_id.as_ref().is_some_and(|id| !p.clips.iter().any(|c| &c.id == id)) {
            s.clip_id = None;
        }
    }

    for (a, b, members) in groups {
        let mut clip = new_clip(a, b, false);
        if let Some((_, _, id)) = old.iter().find(|o| o.0 == a && o.1 == b) {
            clip.id = id.clone();
        }
        for i in members {
            p.songs[i].clip_id = Some(clip.id.clone());
        }
        p.clips.push(clip);
    }
    p.clips.sort_by(|a, b| a.start.total_cmp(&b.start));
    refresh_song_ids(p);
}

fn new_clip(start: f64, end: f64, pinned: bool) -> Clip {
    Clip { id: new_id("c"), start, end, video: None, offset: 0.0, stems: None, talk: Vec::new(), song_ids: Vec::new(), pinned, alignment: None }
}

fn refresh_song_ids(p: &mut Project) {
    for c in p.clips.iter_mut() {
        c.song_ids = p.songs.iter().filter(|s| s.clip_id.as_deref() == Some(&c.id)).map(|s| s.id.clone()).collect();
    }
}

/// Plan clips covering exactly `song_ids` (± margins, merged among themselves),
/// so fetching them doesn't drag in neighbouring songs that a normal plan
/// would merge into the same clip. Songs already covered by a fetched clip are
/// left alone. Returns the clip ids that hold the requested songs.
pub fn pin_exact(p: &mut Project, song_ids: &[String]) -> Vec<String> {
    let dur = p.audio.as_ref().map(|a| a.duration).or(p.source.as_ref().map(|s| s.duration)).unwrap_or(f64::MAX);
    let (mb, ma) = (p.settings.margin_before, p.settings.margin_after);
    let mut want: Vec<(usize, f64, f64)> = p
        .songs
        .iter()
        .enumerate()
        .filter(|(_, s)| s.enabled && song_ids.contains(&s.id))
        .filter(|(_, s)| !p.clips.iter().any(|c| c.covered().is_some_and(|(a, b)| a <= s.start && s.end <= b)))
        .map(|(i, s)| (i, (s.start - mb).max(0.0), (s.end + ma).min(dur)))
        .collect();
    want.sort_by(|a, b| a.1.total_cmp(&b.1));
    let groups = group(want);
    for (a, b, members) in groups {
        let clip = new_clip(a, b, true);
        for i in members {
            p.songs[i].clip_id = Some(clip.id.clone());
        }
        p.clips.push(clip);
    }
    plan_clips(p);
    let mut out: Vec<String> = p.songs.iter().filter(|s| song_ids.contains(&s.id)).filter_map(|s| s.clip_id.clone()).collect();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::model::Project;

    #[test]
    fn merges_close_songs() {
        let mut p = Project::new("t", "x");
        let a = crate::analysis::detect::Analysis {
            segments: vec![(100.0, 300.0), (330.0, 500.0), (2000.0, 2200.0)],
            ..Default::default()
        };
        p.apply_detection(&a);
        plan_clips(&mut p);
        assert_eq!(p.clips.len(), 2);
        assert_eq!((p.clips[0].start, p.clips[0].end), (70.0, 530.0));
        assert_eq!(p.songs[0].clip_id, p.songs[1].clip_id);
        // replanning is stable for unfetched clips (same ids)
        let ids: Vec<String> = p.clips.iter().map(|c| c.id.clone()).collect();
        plan_clips(&mut p);
        assert_eq!(p.clips.len(), 2);
        assert_eq!(ids, p.clips.iter().map(|c| c.id.clone()).collect::<Vec<_>>());
        assert_eq!(p.clips[0].song_ids.len(), 2);
    }

    #[test]
    fn long_chains_are_split() {
        let mut p = Project::new("t", "x");
        // 12 back-to-back 4-minute songs with 20 s between them
        let segs: Vec<(f64, f64)> = (0..12).map(|k| (k as f64 * 260.0, k as f64 * 260.0 + 240.0)).collect();
        p.apply_detection(&crate::analysis::detect::Analysis { segments: segs, ..Default::default() });
        plan_clips(&mut p);
        assert!(p.clips.len() > 1);
        assert!(p.clips.iter().all(|c| c.end - c.start <= MAX_CLIP_SEC));
        assert!(p.songs.iter().all(|s| s.clip_id.is_some()));
    }

    #[test]
    fn exact_plan_leaves_neighbours_out() {
        let mut p = Project::new("t", "x");
        let a = crate::analysis::detect::Analysis {
            segments: vec![(100.0, 300.0), (330.0, 500.0), (2000.0, 2200.0)],
            ..Default::default()
        };
        p.apply_detection(&a);
        plan_clips(&mut p);
        let first = p.songs[0].id.clone();
        let ids = pin_exact(&mut p, std::slice::from_ref(&first));
        assert_eq!(ids.len(), 1);
        let c = p.clip(&ids[0]).unwrap();
        assert_eq!((c.start, c.end), (70.0, 330.0));
        assert_eq!(c.song_ids, vec![first.clone()]);
        // the neighbour got its own clip, and re-planning keeps the pinned one
        assert_ne!(p.songs[1].clip_id.as_deref(), Some(ids[0].as_str()));
        plan_clips(&mut p);
        assert_eq!(p.songs[0].clip_id.as_deref(), Some(ids[0].as_str()));
    }

    #[test]
    fn redetect_replaces_planned_auto_songs() {
        let mut p = Project::new("t", "x");
        let a = crate::analysis::detect::Analysis { segments: vec![(100.0, 300.0), (400.0, 500.0)], ..Default::default() };
        p.apply_detection(&a);
        plan_clips(&mut p);
        assert!(p.songs.iter().all(|s| s.clip_id.is_some()));
        let b = crate::analysis::detect::Analysis { segments: vec![(90.0, 310.0)], ..Default::default() };
        p.apply_detection(&b);
        assert_eq!(p.songs.len(), 1);
        assert_eq!((p.songs[0].start, p.songs[0].end), (90.0, 310.0));
    }
}
