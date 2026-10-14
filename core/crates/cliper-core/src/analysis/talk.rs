//! Talk detection on the vocal stem → vocal-mute suggestions.
//!
//! Finds the streamer's speaking voice ("안녕하세요~", "Melt라는 노래였습니다~")
//! so its vocal stem can be muted while the accompaniment plays on. Combines
//! PANNs on the vocal stem (is someone vocalising?) with PANNs on the mix (is it
//! speech rather than singing?); short gaps close, islands drop, regions get a
//! little padding.

use serde::{Deserialize, Serialize};

use super::detect::uniform_filter1d;
use crate::ml::panns::Probs;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TalkParams {
    pub smooth_sec: f64,
    /// smoothed vocal-stem speech must exceed this
    pub vocal_speech_min: f64,
    /// smoothed mix speech must exceed this outside songs …
    pub mix_speech_min: f64,
    /// … and this inside a song (singing rarely lifts mix speech above ~0.1)
    pub mix_speech_min_in_song: f64,
    pub close_sec: f64,
    pub min_sec: f64,
    pub pad_sec: f64,
    /// within this many seconds of a song's start / end the outside-song
    /// threshold applies (detected songs include intros/outros, where
    /// "이번 노래는…" talk happens)
    pub edge_sec: f64,
    /// not talk where the mix says singing at least as strongly as speech
    pub sing_veto: bool,
    /// edge refinement: search this far outward for where the voice ends …
    pub refine_search_sec: f64,
    /// … i.e. where the vocal stem falls this far (dB) below the region's peak
    pub refine_below_db: f64,
}

impl Default for TalkParams {
    fn default() -> Self {
        Self {
            smooth_sec: 0.5,
            vocal_speech_min: 0.3,
            mix_speech_min: 0.15,
            mix_speech_min_in_song: 0.3,
            close_sec: 1.0,
            min_sec: 0.6,
            pad_sec: 0.15,
            edge_sec: 20.0,
            sing_veto: true,
            refine_search_sec: 0.5,
            refine_below_db: 30.0,
        }
    }
}

fn smooth(v: &[f32], w: usize) -> Vec<f64> {
    uniform_filter1d(&v.iter().map(|&x| x as f64).collect::<Vec<_>>(), w)
}

/// Talk regions (seconds from the start of both prob tracks).
///
/// An isolated singing voice is mostly tagged "speech" by PANNs, so the vocal
/// stem alone can't tell talk from singing; the mix can (singing over music
/// keeps mix speech low). A frame is talk when the vocal stem has voice
/// (speech↑) **and** the mix agrees it is speech. `songs` (same time base)
/// tightens the mix threshold inside songs.
pub fn talk_regions(vocals: &Probs, mix: &Probs, songs: &[(f64, f64)], tp: &TalkParams) -> Vec<(f64, f64)> {
    let fps = vocals.fps;
    let w = ((tp.smooth_sec * fps) as usize).max(1);
    let vs = smooth(&vocals.speech, w);
    let ms = smooth(&mix.speech, w);
    let sing = smooth(&mix.sing, w);
    let n = vs.len().min(ms.len()).min(sing.len());
    // the stricter threshold only in a song's body, not its intro/outro edges
    let in_song_body = |i: usize| {
        let t = i as f64 / fps;
        songs.iter().any(|&(a, b)| a + tp.edge_sec <= t && t < b - tp.edge_sec)
    };
    let mask: Vec<bool> = (0..n)
        .map(|i| {
            let thr = if in_song_body(i) { tp.mix_speech_min_in_song } else { tp.mix_speech_min };
            let singing = tp.sing_veto && sing[i] >= ms[i];
            vs[i] >= tp.vocal_speech_min && ms[i] >= thr && !singing
        })
        .collect();

    let mut regs: Vec<(f64, f64)> = Vec::new();
    let mut start = None;
    for (i, &m) in mask.iter().chain(std::iter::once(&false)).enumerate() {
        match (m, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                let (a, b) = (s as f64 / fps, i as f64 / fps);
                match regs.last_mut() {
                    Some(last) if a - last.1 <= tp.close_sec => last.1 = b,
                    _ => regs.push((a, b)),
                }
                start = None;
            }
            _ => {}
        }
    }
    let end = vocals.duration.min(mix.duration);
    regs.into_iter()
        .filter(|(a, b)| b - a >= tp.min_sec)
        .map(|(a, b)| ((a - tp.pad_sec).max(0.0), (b + tp.pad_sec).min(end)))
        .collect()
}

/// Widen each region's edges to where the voice actually stops: PANNs' 10 fps,
/// smoothed decision is only accurate to a few hundred ms, which clips the
/// first syllable ("안…") and the tail ("…다"). Scans the vocal stem's 10 ms
/// energy outward (at most `refine_search_sec`) until it drops
/// `refine_below_db` under the region's peak. Never shrinks a region.
/// `section`: only `[start, start + duration)` of the file (regions are then
/// relative to `start`).
pub fn refine_edges(regions: &[(f64, f64)], vocals: &std::path::Path, section: Option<(f64, f64)>, tp: &TalkParams) -> std::io::Result<Vec<(f64, f64)>> {
    if regions.is_empty() {
        return Ok(Vec::new());
    }
    const RATE: u32 = 8000;
    const HOP: usize = 80; // 10 ms
    let mut rd = crate::audio::ffmpeg::decode(vocals, RATE, 1, section.map(|s| s.0), section.map(|s| s.1)).map_err(std::io::Error::other)?;
    let mut env: Vec<f32> = Vec::new();
    let (mut acc, mut k) = (0f64, 0usize);
    let mut buf = vec![0f32; 1 << 15];
    loop {
        let n = rd.read(&mut buf).map_err(std::io::Error::other)?;
        if n == 0 {
            break;
        }
        for &x in &buf[..n] {
            acc += (x * x) as f64;
            k += 1;
            if k == HOP {
                env.push((acc / HOP as f64).sqrt() as f32);
                acc = 0.0;
                k = 0;
            }
        }
    }
    let fps = RATE as f64 / HOP as f64;
    let last = env.len().saturating_sub(1);
    let idx = |t: f64| ((t * fps) as usize).min(last);
    let reach = (tp.refine_search_sec * fps) as usize;
    let ratio = 10f32.powf(-(tp.refine_below_db as f32) / 20.0);
    Ok(regions
        .iter()
        .map(|&(a, b)| {
            let (ia, ib) = (idx(a), idx(b).max(idx(a)));
            let peak = env[ia..=ib].iter().copied().fold(0f32, f32::max);
            let thr = (peak * ratio).max(1e-4);
            let mut s = ia;
            while s > 0 && ia - s < reach && env[s - 1] > thr {
                s -= 1;
            }
            let mut e = ib;
            while e < last && e - ib < reach && env[e + 1] > thr {
                e += 1;
            }
            ((s as f64 / fps).min(a), ((e + 1) as f64 / fps).max(b))
        })
        .collect())
}

/// Tag a stem file (any format ffmpeg reads) on the 10 fps grid; `section`
/// = only `[start, start + duration)` (times then start at its start).
pub fn probs_of_file(
    tagger: &mut crate::ml::panns::Tagger,
    file: &std::path::Path,
    section: Option<(f64, f64)>,
    cancel: &crate::progress::CancellationToken,
) -> Result<Probs, crate::ml::panns::PannsError> {
    let mut rd = crate::audio::ffmpeg::decode(file, crate::ml::panns::SAMPLE_RATE, 1, section.map(|s| s.0), section.map(|s| s.1))
        .map_err(std::io::Error::other)?;
    tagger.probs_streaming(|b| rd.read(b).map_err(std::io::Error::other), None, &crate::progress::noop(), cancel)
}
