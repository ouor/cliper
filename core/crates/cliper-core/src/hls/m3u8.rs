//! HLS playlist parsing (master → variants, media → init + segments).

use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Variant {
    pub width: u32,
    pub height: u32,
    pub bandwidth: u64,
    pub codecs: Option<String>,
    /// absolute media-playlist URL (with propagated token query, if any)
    pub url: String,
}

impl Variant {
    pub fn resolution(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }
    pub fn pixels(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MediaPlaylist {
    pub init_url: Option<String>,
    pub segments: Vec<String>,
    pub durations: Vec<f64>,
}

impl MediaPlaylist {
    /// Start time (s) of each segment from cumulative EXTINF.
    pub fn starts(&self) -> Vec<f64> {
        let mut t = 0.0;
        self.durations
            .iter()
            .map(|d| {
                let s = t;
                t += d;
                s
            })
            .collect()
    }

    pub fn total_duration(&self) -> f64 {
        self.durations.iter().sum()
    }

    /// Same segmentation (count + EXTINF within 1 ms) — used to swap a refreshed
    /// playlist in for an expired one, or to pair variants segment-by-segment.
    pub fn same_layout(&self, other: &MediaPlaylist) -> bool {
        self.durations.len() == other.durations.len()
            && self.durations.iter().zip(&other.durations).all(|(a, b)| (a - b).abs() < 1e-3)
    }

    /// Segment index range covering `[start, end)` seconds, plus the offset of
    /// `start` from the first selected segment's start (port of `_segment_range`).
    pub fn segment_range(&self, start: f64, end: Option<f64>) -> SegRange {
        let starts = self.starts();
        let n = starts.len();
        if n == 0 {
            return SegRange { first: 0, last: 0, seg_start: 0.0, trim: 0.0 };
        }
        let total = self.total_duration();
        let s = start.max(0.0);
        let e = end.unwrap_or(total);
        let mut i0 = 0;
        while i0 + 1 < n && starts[i0 + 1] <= s {
            i0 += 1;
        }
        let mut i1 = i0;
        while i1 < n && starts[i1] < e {
            i1 += 1;
        }
        let i1 = i1.max(i0 + 1);
        SegRange { first: i0, last: i1, seg_start: starts[i0], trim: s - starts[i0] }
    }
}

/// Segments `[first, last)`; `seg_start` = VOD time of segment `first`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct SegRange {
    pub first: usize,
    pub last: usize,
    pub seg_start: f64,
    pub trim: f64,
}

fn attr<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    // KEY=value or KEY="value", attributes separated by commas outside quotes
    let mut rest = line.split_once(':').map(|x| x.1).unwrap_or(line);
    while !rest.is_empty() {
        let (k, after) = rest.split_once('=')?;
        let (val, next) = if let Some(q) = after.strip_prefix('"') {
            let end = q.find('"')?;
            (&q[..end], q[end + 1..].trim_start_matches(','))
        } else {
            match after.find(',') {
                Some(i) => (&after[..i], &after[i + 1..]),
                None => (after, ""),
            }
        };
        if k.trim().eq_ignore_ascii_case(key) {
            return Some(val);
        }
        rest = next;
    }
    None
}

pub fn join(base: &str, uri: &str) -> String {
    match Url::parse(base).and_then(|b| b.join(uri)) {
        Ok(u) => u.to_string(),
        Err(_) => uri.to_string(),
    }
}

/// Append `query` (e.g. chzzk `_lsu_sa_=…` token) unless the URL already has one.
pub fn with_query(url: &str, query: Option<&str>) -> String {
    match query {
        Some(q) if !q.is_empty() && !url.contains('?') => format!("{url}?{q}"),
        _ => url.to_string(),
    }
}

pub fn parse_master(text: &str, master_url: &str, propagate_query: Option<&str>) -> Vec<Variant> {
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let mut out = Vec::new();
    for (i, ln) in lines.iter().enumerate() {
        if !ln.starts_with("#EXT-X-STREAM-INF") {
            continue;
        }
        let Some(uri) = lines[i + 1..].iter().find(|l| !l.is_empty() && !l.starts_with('#')) else { continue };
        let (w, h) = attr(ln, "RESOLUTION")
            .and_then(|r| r.split_once('x'))
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .unwrap_or((0, 0));
        out.push(Variant {
            width: w,
            height: h,
            bandwidth: attr(ln, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
            codecs: attr(ln, "CODECS").map(str::to_string),
            url: with_query(&join(master_url, uri), propagate_query),
        });
    }
    out
}

pub fn parse_media(text: &str, media_url: &str, propagate_query: Option<&str>) -> MediaPlaylist {
    let mut pl = MediaPlaylist::default();
    let mut pending: Option<f64> = None;
    for ln in text.lines().map(str::trim) {
        if ln.starts_with("#EXT-X-MAP") {
            if let Some(u) = attr(ln, "URI") {
                pl.init_url = Some(with_query(&join(media_url, u), propagate_query));
            }
        } else if let Some(rest) = ln.strip_prefix("#EXTINF:") {
            pending = rest.split(',').next().and_then(|d| d.trim().parse().ok()).or(Some(0.0));
        } else if !ln.is_empty() && !ln.starts_with('#') {
            pl.segments.push(with_query(&join(media_url, ln), propagate_query));
            pl.durations.push(pending.take().unwrap_or(0.0));
        }
    }
    pl
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_and_media() {
        let m = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1692000,CODECS=\"avc1.4D001F,mp4a.40.2\",RESOLUTION=852x480\n480p/a/vod.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080\n1080p/a/vod.m3u8\n";
        let v = parse_master(m, "https://cdn.x/v/master.m3u8?tok=1", Some("tok=1"));
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].codecs.as_deref(), Some("avc1.4D001F,mp4a.40.2"));
        assert_eq!(v[1].url, "https://cdn.x/v/1080p/a/vod.m3u8?tok=1");
        let media = "#EXTM3U\n#EXT-X-MAP:URI=\"init.m4s\"\n#EXTINF:2.0,\nseg-1.m4v\n#EXTINF:2.5,\nseg-2.m4v\n#EXTINF:2.0,\nseg-3.m4v\n";
        let pl = parse_media(media, "https://cdn.x/v/1080p/a/vod.m3u8", None);
        assert_eq!(pl.init_url.as_deref(), Some("https://cdn.x/v/1080p/a/init.m4s"));
        assert_eq!(pl.segments.len(), 3);
        let r = pl.segment_range(3.0, Some(4.6));
        assert_eq!((r.first, r.last), (1, 3));
        assert!((r.trim - 1.0).abs() < 1e-9);
    }
}
