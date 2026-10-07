//! Clip planning: songs ± margins → merged VOD ranges to fetch as video.
//!
//! Detection boundaries can be tight (they sit on the music/speech threshold
//! and may clip intros/outros), so every song gets margins; overlapping ranges
//! merge into one clip so shared footage is downloaded once.

use super::model::{Clip, Project, new_id};

/// Plan clips for every enabled song. Songs already inside a downloaded clip
/// keep it; the rest are grouped into new (not yet fetched) clips. Clips that
/// no longer hold any song and have no video are dropped.
pub fn plan_clips(p: &mut Project) {
    let dur = p.audio.as_ref().map(|a| a.duration).or(p.source.as_ref().map(|s| s.duration)).unwrap_or(f64::MAX);
    let (mb, ma) = (p.settings.margin_before, p.settings.margin_after);

    // 1) keep songs whose wanted range is already covered by a fetched clip
    let mut pending: Vec<(usize, f64, f64)> = Vec::new();
    for (i, s) in p.songs.iter_mut().enumerate() {
        if !s.enabled {
            continue;
        }
        let host = p.clips.iter().find(|c| c.covered().is_some_and(|(a, b)| a <= s.start && s.end <= b));
        match host {
            Some(c) => s.clip_id = Some(c.id.clone()),
            None => pending.push((i, (s.start - mb).max(0.0), (s.end + ma).min(dur))),
        }
    }

    // 2) merge pending ranges
    pending.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let mut groups: Vec<(f64, f64, Vec<usize>)> = Vec::new();
    for (i, a, b) in pending {
        match groups.last_mut() {
            Some(g) if a <= g.1 => {
                g.1 = g.1.max(b);
                g.2.push(i);
            }
            _ => groups.push((a, b, vec![i])),
        }
    }

    // 3) drop unfetched clips (they get re-planned); fetched ones stay for reuse
    p.clips.retain(|c| c.video.is_some());
    for s in p.songs.iter_mut() {
        if s.clip_id.as_ref().is_some_and(|id| !p.clips.iter().any(|c| &c.id == id)) {
            s.clip_id = None;
        }
    }

    for (a, b, members) in groups {
        let clip = Clip { id: new_id("c"), start: a, end: b, video: None, offset: 0.0, stems: None, talk: Vec::new() };
        for i in members {
            p.songs[i].clip_id = Some(clip.id.clone());
        }
        p.clips.push(clip);
    }
    p.clips.sort_by(|a, b| a.start.partial_cmp(&b.start).unwrap());
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
        // replanning is stable for unfetched clips
        plan_clips(&mut p);
        assert_eq!(p.clips.len(), 2);
    }
}
