//! Platform-agnostic description of a resolved VOD (port of `source.py`, extended
//! with every variant so video can later be fetched at any rendition).

use serde::{Deserialize, Serialize};

use crate::hls::m3u8::{self, MediaPlaylist, Variant};
use crate::http::{Http, NetError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    Chzzk,
    Soop,
    /// a recording on this PC (e.g. OBS) — `SourceInfo.url` is its path
    Local,
}

impl Platform {
    pub fn as_str(&self) -> &'static str {
        match self {
            Platform::Chzzk => "chzzk",
            Platform::Soop => "soop",
            Platform::Local => "local",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Container {
    /// fragmented MP4 (moof/mdat) — audio runs range-fetchable
    Fmp4,
    /// MPEG-TS — audio PID interleaved, whole segment needed
    Ts,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Channel {
    pub id: Option<String>,
    pub name: Option<String>,
    pub extra_id: Option<serde_json::Value>,
}

/// Normalized catalog fields (metadata sidecar `source` block, docs/06).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Catalog {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub channel: Channel,
    pub published_at: Option<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub thumbnail: Option<String>,
    pub adult: Option<bool>,
    pub view_count: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("unrecognized VOD reference {0:?} — use a chzzk/sooplive URL, a chzzk number, or 'soop:<titleNo>'")]
    Unrecognized(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    AuthRequired(String),
    #[error("{0}")]
    Malformed(String),
    #[error(transparent)]
    Net(#[from] NetError),
}

/// Serializable snapshot of a resolved VOD (stored in project.json).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SourceInfo {
    pub platform: Platform,
    pub video_id: String,
    pub url: String,
    pub title: String,
    pub channel: String,
    /// seconds, from metadata (0 if unknown)
    pub duration: f64,
    pub container: Container,
    pub variants: Vec<Variant>,
    /// index into `variants` used for audio extraction
    pub audio_variant: usize,
    pub catalog: Catalog,
    pub notes: Vec<String>,
}

#[derive(Clone)]
pub struct AudioFallback {
    pub playlist: MediaPlaylist,
    /// init segment (fMP4) — track ids / AAC config can differ per variant
    pub init: Vec<u8>,
}

/// A resolved VOD: metadata + the audio variant's playlist and a live HTTP client.
#[derive(Clone)]
pub struct Source {
    pub info: SourceInfo,
    pub playlist: MediaPlaylist,
    pub init: Vec<u8>,
    /// other variants (same segmentation) to pull a segment's audio from when the
    /// primary's CDN copy is broken, most preferred first
    pub audio_fallbacks: Vec<AudioFallback>,
    /// query string to propagate to variant/segment URLs (chzzk TS token)
    pub token_query: Option<String>,
    /// bytes fetched up-front per fragment (covers the moof)
    pub head_hint: u64,
    pub default_workers: usize,
    pub raw: serde_json::Value,
    pub http: Http,
}

impl Source {
    /// Media playlist of any variant (for video fetching).
    pub async fn variant_playlist(&self, idx: usize) -> Result<MediaPlaylist, ResolveError> {
        let v = self
            .info
            .variants
            .get(idx)
            .ok_or_else(|| ResolveError::Malformed(format!("no variant #{idx}")))?;
        if idx == self.info.audio_variant {
            return Ok(self.playlist.clone());
        }
        let text = self.http.get_text(&v.url).await?;
        Ok(m3u8::parse_media(&text, &v.url, self.token_query.as_deref()))
    }

    /// Variant index best matching a target height (`None` = highest).
    /// Load `variants[idx]`'s playlist (+ init) as an audio fallback if its
/// segmentation matches `primary`.
pub async fn load_fallback(
    http: &Http,
    variant: &Variant,
    primary: &MediaPlaylist,
    token_query: Option<&str>,
) -> Option<AudioFallback> {
    let text = http.get_text(&variant.url).await.ok()?;
    let playlist = m3u8::parse_media(&text, &variant.url, token_query);
    if playlist.segments.len() != primary.segments.len() {
        return None;
    }
    let init = match &playlist.init_url {
        Some(u) => http.get_bytes(u).await.ok()?,
        None => Vec::new(),
    };
    Some(AudioFallback { playlist, init })
}

pub fn pick_variant(&self, height: Option<u32>) -> usize {
        pick_variant(&self.info.variants, height)
    }
}

/// Load `variants[idx]`'s playlist (+ init) as an audio fallback if its
/// segmentation matches `primary`.
pub async fn load_fallback(
    http: &Http,
    variant: &Variant,
    primary: &MediaPlaylist,
    token_query: Option<&str>,
) -> Option<AudioFallback> {
    let text = http.get_text(&variant.url).await.ok()?;
    let playlist = m3u8::parse_media(&text, &variant.url, token_query);
    if playlist.segments.len() != primary.segments.len() {
        return None;
    }
    let init = match &playlist.init_url {
        Some(u) => http.get_bytes(u).await.ok()?,
        None => Vec::new(),
    };
    Some(AudioFallback { playlist, init })
}

pub fn pick_variant(variants: &[Variant], height: Option<u32>) -> usize {
    let best = |it: &mut dyn Iterator<Item = (usize, &Variant)>, key: &dyn Fn(&Variant) -> i64| {
        it.min_by_key(|(_, v)| key(v)).map(|(i, _)| i)
    };
    match height {
        None => best(&mut variants.iter().enumerate(), &|v| -(v.pixels() as i64)).unwrap_or(0),
        Some(h) => {
            let below: Vec<(usize, &Variant)> = variants.iter().enumerate().filter(|(_, v)| v.height <= h).collect();
            let pool: Vec<(usize, &Variant)> = if below.is_empty() { variants.iter().enumerate().collect() } else { below };
            best(&mut pool.into_iter(), &|v| (v.height as i64 - h as i64).abs()).unwrap_or(0)
        }
    }
}
