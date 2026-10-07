//! Platform registry — maps a VOD reference (URL / id) to a discovery adapter.

pub mod chzzk;
pub mod soop;

use std::collections::BTreeMap;

use crate::source::{Platform, ResolveError, Source};

/// Most specific first: soop needs an explicit URL/prefix, chzzk catches bare numbers.
pub fn detect(video: &str) -> Result<(Platform, String), ResolveError> {
    if let Some(id) = soop::matches(video) {
        return Ok((Platform::Soop, id));
    }
    if let Some(id) = chzzk::matches(video) {
        return Ok((Platform::Chzzk, id));
    }
    Err(ResolveError::Unrecognized(video.to_string()))
}

pub async fn resolve(video: &str, cookies: Option<&BTreeMap<String, String>>) -> Result<Source, ResolveError> {
    let (platform, id) = detect(video)?;
    match platform {
        Platform::Chzzk => chzzk::resolve(&id, cookies).await,
        Platform::Soop => soop::resolve(&id, cookies).await,
        Platform::Local => Err(ResolveError::Unrecognized(video.to_string())),
    }
}
