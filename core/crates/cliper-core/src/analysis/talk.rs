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
    let n = vs.len().min(ms.len());
    let in_song = |i: usize| {
        let t = i as f64 / fps;
        songs.iter().any(|&(a, b)| a <= t && t < b)
    };
    let mask: Vec<bool> = (0..n)
        .map(|i| {
            let thr = if in_song(i) { tp.mix_speech_min_in_song } else { tp.mix_speech_min };
            vs[i] >= tp.vocal_speech_min && ms[i] >= thr
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

/// Tag a stem file (any format ffmpeg reads) on the 10 fps grid.
pub fn probs_of_file(
    tagger: &mut crate::ml::panns::Tagger,
    file: &std::path::Path,
    cancel: &crate::progress::CancellationToken,
) -> Result<Probs, crate::ml::panns::PannsError> {
    let mut rd = crate::audio::ffmpeg::decode(file, crate::ml::panns::SAMPLE_RATE, 1, None, None)
        .map_err(std::io::Error::other)?;
    tagger.probs_streaming(|b| rd.read(b).map_err(std::io::Error::other), None, &crate::progress::noop(), cancel)
}
