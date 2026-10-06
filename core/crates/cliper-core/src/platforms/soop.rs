//! SOOP (sooplive.com) VOD discovery — port of `platforms/soop.py`.
//!
//! `POST api.m.sooplive.com/station/video/a/view` → Wowza smil master → fMP4
//! variants. Audio is byte-identical across variants, so the audio variant is
//! picked for CDN reliability (720p by default — larger variants are often only
//! partially cached) and every other same-layout variant becomes a per-segment
//! fallback.

use std::collections::BTreeMap;

use regex::Regex;
use serde_json::Value;

use crate::hls::m3u8;
use crate::http::Http;
use crate::source::{Catalog, Channel, Container, Platform, ResolveError, Source, SourceInfo, pick_variant};

const VIEW: &str = "https://api.m.sooplive.com/station/video/a/view";
const STATION: &str = "https://api-channel.sooplive.com/v1.1/channel/{bj}/station";
const HEADERS: &[(&str, &str)] = &[
    ("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36"),
    ("Origin", "https://vod.sooplive.com"),
    ("Referer", "https://vod.sooplive.com/"),
];

/// Default audio variant height (override with env `SOOP_VARIANT`: smallest|highest|<height>).
const DEFAULT_AUDIO_HEIGHT: u32 = 720;

/// Recognize a sooplive VOD URL or `soop:<titleNo>` (bare numbers go to chzzk).
pub fn matches(video: &str) -> Option<String> {
    for pat in [r"sooplive\.com/player/(\d+)", r"vod\.sooplive\.com/.*?(\d{6,})", r"^soop:\s*(\d+)\s*$"] {
        if let Some(c) = Regex::new(pat).unwrap().captures(video.trim()) {
            return Some(c[1].to_string());
        }
    }
    None
}

fn s(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn audio_variant(variants: &[m3u8::Variant]) -> usize {
    let pref = std::env::var("SOOP_VARIANT").unwrap_or_default().trim().to_lowercase();
    match pref.as_str() {
        "smallest" => variants.iter().enumerate().min_by_key(|(_, v)| v.pixels()).map(|x| x.0).unwrap_or(0),
        "highest" => pick_variant(variants, None),
        p => pick_variant(variants, Some(p.parse().unwrap_or(DEFAULT_AUDIO_HEIGHT))),
    }
}

pub async fn resolve(video_id: &str, cookies: Option<&BTreeMap<String, String>>) -> Result<Source, ResolveError> {
    let http = Http::new(HEADERS, cookies);
    let resp = http
        .client()
        .post(VIEW)
        .form(&[("nTitleNo", video_id), ("nApiLevel", "11"), ("nPlaylistIdx", "0")])
        .send()
        .await
        .map_err(|e| ResolveError::Net(e.into()))?;
    let body: Value = resp.json().await.map_err(|e| ResolveError::Net(e.into()))?;
    let data = body["data"].clone();
    let files = data["files"].as_array().cloned().unwrap_or_default();
    let Some(master_url) = files.first().and_then(|f| s(&f["file"])) else {
        return Err(ResolveError::NotFound(format!(
            "SOOP VOD {video_id}: no playable files (deleted, paid, or login-required VOD)."
        )));
    };
    let mut notes = Vec::new();
    if files.len() > 1 {
        notes.push(format!("VOD has {} parts; only the first part is processed.", files.len()));
    }

    let master = http.get_text(&master_url).await?;
    let variants = m3u8::parse_master(&master, &master_url, None);
    if variants.is_empty() {
        return Err(ResolveError::Malformed("SOOP: no variant streams in master playlist.".into()));
    }
    let av = audio_variant(&variants);
    let vurl = variants[av].url.clone();
    let playlist = m3u8::parse_media(&http.get_text(&vurl).await?, &vurl, None);
    let Some(init_url) = playlist.init_url.clone().filter(|_| !playlist.segments.is_empty()) else {
        return Err(ResolveError::Malformed(format!("SOOP VOD {video_id}: media playlist missing init/segments.")));
    };
    let init = http.get_bytes(&init_url).await?;

    // other variants, smallest first, as per-segment audio fallbacks
    let mut others: Vec<&m3u8::Variant> = variants.iter().filter(|v| v.url != vurl).collect();
    others.sort_by_key(|v| v.pixels());
    let mut audio_fallbacks = Vec::new();
    for v in others {
        if let Some(fb) = crate::source::load_fallback(&http, v, &playlist, None).await {
            audio_fallbacks.push(fb);
        }
    }

    let bj = s(&data["bj_id"]).unwrap_or_default();
    let station: Value = match http.get_bytes(&STATION.replace("{bj}", &bj)).await {
        Ok(b) => serde_json::from_slice(&b).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let st = &station["station"];
    let catalog = Catalog {
        kind: s(&data["file_type"]),
        channel: Channel {
            id: Some(bj.clone()).filter(|b| !b.is_empty()),
            name: s(&st["userNick"]).or_else(|| s(&st["stationName"])),
            extra_id: Some(data["station_no"].clone()).filter(|v| !v.is_null()),
        },
        published_at: s(&data["broad_start"]),
        category: s(&data["cate_name"]).or_else(|| s(&data["category_name"])),
        tags: data["hash_tags"].as_array().map(|a| a.iter().filter_map(s).collect()).unwrap_or_default(),
        thumbnail: s(&data["thumb"]),
        adult: Some(
            data["grade"].as_i64().unwrap_or(0) != 0
                || !(data["adult_status"].is_null() || data["adult_status"] == "pass"),
        ),
        view_count: data["view_cnt"].as_u64(),
    };
    let mut raw = serde_json::json!({ "soop_view": data });
    if !station.is_null() {
        raw["soop_station"] = station.clone();
    }
    Ok(Source {
        info: SourceInfo {
            platform: Platform::Soop,
            video_id: video_id.to_string(),
            url: format!("https://vod.sooplive.com/player/{video_id}"),
            title: s(&data["full_title"]).or_else(|| s(&data["title"])).unwrap_or_else(|| video_id.to_string()),
            channel: catalog.channel.name.clone().unwrap_or(bj),
            duration: data["total_file_duration"].as_f64().unwrap_or(0.0) / 1000.0,
            container: Container::Fmp4,
            variants,
            audio_variant: av,
            catalog,
            notes,
        },
        playlist,
        init,
        audio_fallbacks,
        token_query: None,
        head_hint: 16384,
        // SOOP CDN drops concurrent Range requests; serial is plenty fast
        default_workers: 1,
        raw,
        http,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn matches() {
        assert_eq!(super::matches("https://vod.sooplive.com/player/198140749").as_deref(), Some("198140749"));
        assert_eq!(super::matches("soop:198140749").as_deref(), Some("198140749"));
        assert_eq!(super::matches("198140749"), None);
    }
}
