//! chzzk (chzzk.naver.com) VOD discovery — port of `platforms/chzzk.py`.
//!
//! * **live rewind** — `liveRewindPlaybackJson` carries an fMP4 HLS master.
//! * **general VOD** — playback from the Naver Playback API (`vodplay`) keyed by
//!   `videoId`+`inKey`, pointing to MPEG-TS segments whose playlist token query
//!   (`_lsu_sa_=…`) must be propagated to every variant/segment URL.

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::Value;

use crate::hls::m3u8;
use crate::http::Http;
use crate::source::{Catalog, Channel, Container, Platform, ResolveError, Source, SourceInfo, pick_variant};

const API: &str = "https://api.chzzk.naver.com";
const HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36"),
    ("Origin", "https://chzzk.naver.com"),
    ("Referer", "https://chzzk.naver.com"),
    ("Front-Client-Platform-Type", "PC"),
    ("Front-Client-Product-Type", "web"),
    ("deviceId", "123e4567-e89b-12d3-a456-426614174000"),
    ("Accept", "application/json, text/plain, */*"),
];

/// Recognize a chzzk URL, a bare number, or `chzzk:<no>`.
pub fn matches(video: &str) -> Option<String> {
    let url = Regex::new(r"chzzk\.naver\.com/video/(\d+)").unwrap();
    if let Some(c) = url.captures(video) {
        return Some(c[1].to_string());
    }
    let bare = Regex::new(r"^(?:chzzk:)?\s*(\d+)\s*$").unwrap();
    bare.captures(video.trim()).map(|c| c[1].to_string())
}

fn s(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn catalog(c: &Value) -> Catalog {
    let ch = &c["channel"];
    Catalog {
        kind: s(&c["videoType"]),
        channel: Channel { id: s(&ch["channelId"]), name: s(&ch["channelName"]), extra_id: None },
        published_at: s(&c["publishDate"]).or_else(|| s(&c["liveOpenDate"])),
        category: s(&c["videoCategoryValue"]).or_else(|| s(&c["videoCategory"])),
        tags: c["tags"].as_array().map(|a| a.iter().filter_map(s).collect()).unwrap_or_default(),
        thumbnail: s(&c["thumbnailImageUrl"]),
        adult: c["adult"].as_bool(),
        view_count: c["readCount"].as_u64(),
    }
}

/// Are these cookies logged in? Returns (logged in, nickname).
pub async fn check_login(cookies: &BTreeMap<String, String>) -> Result<(bool, Option<String>), crate::http::NetError> {
    let http = Http::new(HEADERS, Some(cookies));
    let url = "https://comm-api.game.naver.com/nng_main/v1/user/getUserStatus";
    let resp = http.client().get(url).send().await?;
    if !resp.status().is_success() {
        return Err(crate::http::NetError::Status { status: resp.status().as_u16(), url: url.into() });
    }
    let body: Value = resp.json().await?;
    let c = &body["content"];
    Ok((c["loggedIn"].as_bool().unwrap_or(false), c["nickname"].as_str().map(str::to_string)))
}

pub async fn resolve(video_id: &str, cookies: Option<&BTreeMap<String, String>>) -> Result<Source, ResolveError> {
    let http = Http::new(HEADERS, cookies);
    let resp = http
        .client()
        .get(format!("{API}/service/v3/videos/{video_id}"))
        .send()
        .await
        .map_err(|e| ResolveError::Net(e.into()))?;
    if resp.status().as_u16() == 404 {
        return Err(ResolveError::NotFound(format!("chzzk VOD {video_id} not found.")));
    }
    if !resp.status().is_success() {
        return Err(ResolveError::Net(crate::http::NetError::Status {
            status: resp.status().as_u16(),
            url: format!("{API}/service/v3/videos/{video_id}"),
        }));
    }
    let body: Value = resp.json().await.map_err(|e| ResolveError::Net(e.into()))?;
    let content = body["content"].clone();
    if content.is_null() {
        return Err(ResolveError::NotFound(format!("chzzk VOD {video_id}: empty content.")));
    }
    let mut pj = content["liveRewindPlaybackJson"].clone();
    if let Value::String(st) = &pj {
        pj = serde_json::from_str(st).unwrap_or(Value::Null);
    }
    let rewind_master = pj["media"].get(0).and_then(|m| s(&m["path"]));
    match rewind_master {
        Some(master) => resolve_rewind(http, &master, content, video_id).await,
        None => resolve_ts(http, content, video_id).await,
    }
}

fn info(content: &Value, video_id: &str, container: Container, variants: Vec<m3u8::Variant>, audio_variant: usize) -> SourceInfo {
    SourceInfo {
        platform: Platform::Chzzk,
        video_id: video_id.to_string(),
        url: format!("https://chzzk.naver.com/video/{video_id}"),
        title: s(&content["videoTitle"]).unwrap_or_else(|| video_id.to_string()),
        channel: s(&content["channel"]["channelName"]).unwrap_or_default(),
        duration: content["duration"].as_f64().unwrap_or(0.0),
        container,
        variants,
        audio_variant,
        catalog: catalog(content),
        notes: Vec::new(),
        notices: Vec::new(),
        audio_tracks: Vec::new(),
    }
}

async fn resolve_rewind(http: Http, master_url: &str, content: Value, video_id: &str) -> Result<Source, ResolveError> {
    let master = http.get_text(master_url).await?;
    let variants = m3u8::parse_master(&master, master_url, None);
    if variants.is_empty() {
        return Err(ResolveError::Malformed("chzzk: no variant streams in master playlist.".into()));
    }
    // highest = un-transcoded original: fastest to serve, 192k audio
    let audio_variant = pick_variant(&variants, None);
    let vurl = variants[audio_variant].url.clone();
    let playlist = m3u8::parse_media(&http.get_text(&vurl).await?, &vurl, None);
    let Some(init_url) = playlist.init_url.clone().filter(|_| !playlist.segments.is_empty()) else {
        return Err(ResolveError::Malformed(format!(
            "chzzk VOD {video_id}: variant playlist missing init/segments (playback token may have expired)."
        )));
    };
    let init = http.get_bytes(&init_url).await?;
    // The edge sometimes holds truncated copies of 1080p segments (see
    // `Http::warm`); 720p/480p carry the same 192k audio (separately encoded,
    // so frame counts can differ by one at segment boundaries).
    let mut audio_fallbacks = Vec::new();
    let mut lower: Vec<&m3u8::Variant> =
        variants.iter().filter(|v| v.height < variants[audio_variant].height && v.height >= 480).collect();
    lower.sort_by_key(|v| std::cmp::Reverse(v.height));
    for v in lower.into_iter().take(2) {
        if let Some(fb) = crate::source::load_fallback(&http, v, &playlist, None).await {
            audio_fallbacks.push(fb);
        }
    }
    Ok(Source {
        info: info(&content, video_id, Container::Fmp4, variants, audio_variant),
        playlist,
        init,
        audio_fallbacks,
        token_query: None,
        head_hint: 32768,
        default_workers: 16,
        raw: serde_json::json!({ "chzzk_video": content }),
        http,
    })
}

async fn resolve_ts(http: Http, content: Value, video_id: &str) -> Result<Source, ResolveError> {
    let (Some(vid), Some(key)) = (s(&content["videoId"]), s(&content["inKey"])) else {
        return Err(ResolveError::AuthRequired(
            "chzzk VOD: no playback key (videoId/inKey) — private / membership / adult VOD needs login cookies.".into(),
        ));
    };
    let url = format!("https://apis.naver.com/neonplayer/vodplay/v2/playback/{vid}?key={key}");
    let playback: Value = serde_json::from_slice(&http.get_bytes(&url).await?)
        .map_err(|e| ResolveError::Malformed(format!("chzzk playback JSON: {e}")))?;
    let master_url = playback["period"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["adaptationSet"].as_array().into_iter().flatten())
        .find(|a| a["mimeType"] == "video/mp2t")
        .and_then(|a| s(&a["otherAttributes"]["m3u"]))
        .ok_or_else(|| ResolveError::Malformed("chzzk VOD: playback response has no mp2t/HLS adaptation set.".into()))?;
    let token = url::Url::parse(&master_url).ok().and_then(|u| u.query().map(str::to_string));
    let master = http.get_text(&master_url).await?;
    let variants = m3u8::parse_master(&master, &master_url, token.as_deref());
    if variants.is_empty() {
        return Err(ResolveError::Malformed("chzzk: no variant streams in master playlist.".into()));
    }
    // TS has no range trick (audio is interleaved), so whole segments are pulled:
    // analyse from the lightest variant (64k audio is as good as 192k for song
    // detection, docs/03); export later uses the downloaded clip's own audio.
    let audio_variant = variants.iter().enumerate().min_by_key(|(_, v)| v.pixels()).map(|x| x.0).unwrap_or(0);
    let vurl = variants[audio_variant].url.clone();
    let playlist = m3u8::parse_media(&http.get_text(&vurl).await?, &vurl, token.as_deref());
    let mut audio_fallbacks = Vec::new();
    let mut others: Vec<&m3u8::Variant> = variants.iter().filter(|v| v.url != vurl).collect();
    others.sort_by_key(|v| v.pixels());
    if let Some(v) = others.first()
        && let Some(fb) = crate::source::load_fallback(&http, v, &playlist, token.as_deref()).await {
            audio_fallbacks.push(fb);
        }
    if playlist.segments.is_empty() {
        return Err(ResolveError::Malformed(format!(
            "chzzk VOD {video_id}: media playlist missing segments (playback token may have expired)."
        )));
    }
    Ok(Source {
        info: info(&content, video_id, Container::Ts, variants, audio_variant),
        playlist,
        init: Vec::new(),
        audio_fallbacks,
        token_query: token,
        head_hint: 32768,
        default_workers: 16,
        raw: serde_json::json!({ "chzzk_video": content, "chzzk_playback": playback }),
        http,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn matches() {
        assert_eq!(super::matches("https://chzzk.naver.com/video/13702685").as_deref(), Some("13702685"));
        assert_eq!(super::matches(" 13702685 ").as_deref(), Some("13702685"));
        assert_eq!(super::matches("chzzk:42").as_deref(), Some("42"));
        assert_eq!(super::matches("soop:42"), None);
    }
}
