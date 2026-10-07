//! HTTP helpers — a client carrying platform headers/cookies and a robust ranged GET.
//!
//! VOD CDNs occasionally drop a transfer mid-stream (SOOP especially under
//! concurrency); ranged fetches retry with backoff and treat short reads as
//! transient. Port of `stream_audio/http.py`.

use std::collections::BTreeMap;
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONNECTION, CONTENT_RANGE, COOKIE, RANGE};

/// Network-level failure. Kept distinct from parse errors: the extractor escalates
/// these to a fallback variant, while parse anomalies fall back to a full-segment read.
#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("HTTP {status} for {url}")]
    Status { status: u16, url: String },
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("short read: got {got} of {want} bytes")]
    ShortRead { got: usize, want: usize },
    /// Server answered a Range request with 200 and a partial body (cold-cache
    /// edge bug, see [`Http::warm`]) — the bytes can't be trusted.
    #[error("range ignored by server (cold cache?) for {0}")]
    RangeIgnored(String),
    /// The CDN holds a truncated copy of the object (smaller than its own boxes claim).
    #[error("CDN copy truncated ({have} of {need} bytes) for {url}")]
    Truncated { have: u64, need: u64, url: String },
}

impl NetError {
    pub fn status(&self) -> Option<u16> {
        match self {
            NetError::Status { status, .. } => Some(*status),
            NetError::Request(e) => e.status().map(|s| s.as_u16()),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    pub fn new(headers: &[(&str, &str)], cookies: Option<&BTreeMap<String, String>>) -> Self {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            if let (Ok(name), Ok(val)) = (HeaderName::try_from(*k), HeaderValue::from_str(v)) {
                map.insert(name, val);
            }
        }
        if let Some(c) = cookies {
            let s = c
                .iter()
                .filter(|(_, v)| !v.is_empty() && !v.starts_with("YOUR_"))
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("; ");
            if !s.is_empty()
                && let Ok(v) = HeaderValue::from_str(&s) {
                    map.insert(COOKIE, v);
                }
        }
        let client = reqwest::Client::builder()
            .default_headers(map)
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .pool_max_idle_per_host(32)
            .build()
            .expect("reqwest client");
        Self { client }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// Full GET returning bytes, retrying transient failures.
    pub async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, NetError> {
        let mut last = None;
        for attempt in 0..5u32 {
            match self.get_once(url, None, attempt > 0).await {
                Ok((b, _)) => return Ok(b),
                Err(e) => {
                    if is_permanent(&e) {
                        return Err(e);
                    }
                    last = Some(e);
                    backoff(attempt).await;
                }
            }
        }
        Err(last.unwrap())
    }

    pub async fn get_text(&self, url: &str) -> Result<String, NetError> {
        let b = self.get_bytes(url).await?;
        Ok(String::from_utf8_lossy(&b).into_owned())
    }

    /// Range GET `bytes=a-b` → `(content, total_size)`; `total_size` from Content-Range.
    pub async fn get_range(
        &self,
        url: &str,
        a: u64,
        b: u64,
        retries: u32,
    ) -> Result<(Vec<u8>, Option<u64>), NetError> {
        let retries = retries.max(1);
        let want = b - a + 1;
        let mut last = None;
        for attempt in 0..retries {
            match self.get_once(url, Some((a, b)), attempt > 0).await {
                Ok((buf, total)) => {
                    if let Some(t) = total {
                        let need = want.min(t.saturating_sub(a)) as usize;
                        if buf.len() < need {
                            last = Some(NetError::ShortRead { got: buf.len(), want: need });
                            if attempt + 1 < retries {
                                backoff(attempt).await;
                            }
                            continue;
                        }
                    }
                    return Ok((buf, total));
                }
                Err(e) => {
                    if is_permanent(&e) {
                        return Err(e);
                    }
                    last = Some(e);
                    if attempt + 1 < retries {
                        backoff(attempt).await;
                    }
                }
            }
        }
        Err(last.unwrap())
    }

    /// Whole resource via a plain GET (no Range header).
    pub async fn get_full(&self, url: &str, retries: u32) -> Result<(Vec<u8>, Option<u64>), NetError> {
        let retries = retries.max(1);
        let mut last = None;
        for attempt in 0..retries {
            match self.get_once(url, None, attempt > 0).await {
                Ok((buf, _)) => {
                    let n = buf.len() as u64;
                    return Ok((buf, Some(n)));
                }
                Err(e) => {
                    if is_permanent(&e) {
                        return Err(e);
                    }
                    last = Some(e);
                    if attempt + 1 < retries {
                        backoff(attempt).await;
                    }
                }
            }
        }
        Err(last.unwrap())
    }

    /// HEAD → `Content-Length`.
    ///
    /// Also *warms* the CDN edge: the chzzk VOD edge (nginx, `EO-Cache-Status`)
    /// answers a Range GET on a cold cache with `200` + only the requested bytes
    /// and then caches that truncated body as the whole object (later requests
    /// see e.g. `bytes 0-100/101` and 416). A HEAD on a cold cache instead pulls
    /// the full object into the edge, after which Range requests are served
    /// correctly — so every segment is HEADed before its first Range GET.
    pub async fn warm(&self, url: &str, retries: u32) -> Result<Option<u64>, NetError> {
        let retries = retries.max(1);
        let mut last = None;
        for attempt in 0..retries {
            let res = async {
                let resp = self.client.head(url).send().await?;
                let status = resp.status();
                if !status.is_success() {
                    return Err(NetError::Status { status: status.as_u16(), url: redact(url) });
                }
                Ok(resp
                    .headers()
                    .get(reqwest::header::CONTENT_LENGTH)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok()))
            }
            .await;
            match res {
                Ok(n) => return Ok(n),
                Err(e) => {
                    if is_permanent(&e) {
                        return Err(e);
                    }
                    last = Some(e);
                    if attempt + 1 < retries {
                        backoff(attempt).await;
                    }
                }
            }
        }
        Err(last.unwrap())
    }

    async fn get_once(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        fresh_conn: bool,
    ) -> Result<(Vec<u8>, Option<u64>), NetError> {
        let mut req = self.client.get(url);
        if let Some((a, b)) = range {
            req = req.header(RANGE, format!("bytes={a}-{b}"));
        }
        if fresh_conn {
            // drop a poisoned keep-alive connection
            req = req.header(CONNECTION, "close");
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(NetError::Status { status: status.as_u16(), url: redact(url) });
        }
        let total = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit('/').next())
            .and_then(|t| t.trim().parse::<u64>().ok());
        let partial = status.as_u16() == 206;
        let body = resp.bytes().await?;
        match range {
            // 200 to a Range request: only usable if it is evidently the complete
            // object — longer than the range end, or (from 0) shorter than asked.
            // Exactly `want` bytes is the cold-cache bug's signature → reject.
            Some((a, b)) if !partial => {
                let len = body.len() as u64;
                if len > b || (a == 0 && len < b + 1) {
                    let end = (b + 1).min(len) as usize;
                    Ok((body[a.min(len) as usize..end].to_vec(), Some(len)))
                } else {
                    Err(NetError::RangeIgnored(redact(url)))
                }
            }
            _ => Ok((body.to_vec(), total)),
        }
    }
}

/// 4xx (other than 408/429) won't heal with a retry.
fn is_permanent(e: &NetError) -> bool {
    matches!(e.status(), Some(s) if (400..500).contains(&s) && s != 408 && s != 429)
}

async fn backoff(attempt: u32) {
    let ms = (250 * (attempt as u64 + 1)).min(2000);
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// Strip the query (playback tokens) from URLs that end up in errors/logs.
pub fn redact(url: &str) -> String {
    match url.split_once('?') {
        Some((base, _)) => format!("{base}?…"),
        None => url.to_string(),
    }
}
