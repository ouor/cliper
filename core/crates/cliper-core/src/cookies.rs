//! Login cookies (chzzk `NID_AUT`/`NID_SES`, SOOP tickets) for private /
//! membership VODs.
//!
//! Stored in the projects root: on Windows encrypted for the current user with
//! DPAPI (`cookies.dpapi`), elsewhere as `cookies.json`. A plaintext
//! `cookies.json` from older versions is migrated on load (Windows).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    cookies: BTreeMap<String, String>,
    saved_at: String,
}

/// What the UI can show about the stored cookies (never the values).
#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CookieStatus {
    pub set: bool,
    /// cookie names (values are never returned)
    pub names: Vec<String>,
    /// platforms the names look like they belong to ("chzzk", "soop")
    pub platforms: Vec<String>,
    pub saved_at: Option<String>,
    /// stored encrypted for this Windows user (DPAPI)
    pub encrypted: bool,
    /// result of the last `system.check_cookies` (None = not checked / can't tell)
    pub valid: Option<bool>,
    pub checked_at: Option<String>,
    /// account nickname from the last successful check
    pub account: Option<String>,
}

pub struct CookieJar {
    root: PathBuf,
    stored: Option<Stored>,
    check: Option<(Option<bool>, String, Option<String>)>,
}

fn platform_of(name: &str) -> &'static str {
    if name.starts_with("NID_") { "chzzk" } else { "soop" }
}

impl CookieJar {
    pub fn load(root: &Path) -> Self {
        let mut jar = Self { root: root.to_path_buf(), stored: None, check: None };
        let plain = root.join("cookies.json");
        if let Some(b) = std::fs::read(root.join("cookies.dpapi")).ok().and_then(|b| dpapi::unprotect(&b)) {
            jar.stored = serde_json::from_slice(&b).ok();
        } else if let Ok(b) = std::fs::read(&plain) {
            jar.stored = serde_json::from_slice::<Stored>(&b).ok().or_else(|| {
                // older format: a bare name → value map
                serde_json::from_slice::<BTreeMap<String, String>>(&b).ok().map(|cookies| Stored { cookies, saved_at: String::new() })
            });
            if dpapi::AVAILABLE && jar.stored.is_some() && jar.save().is_ok() {
                let _ = std::fs::remove_file(&plain);
            }
        }
        jar
    }

    pub fn get(&self) -> Option<BTreeMap<String, String>> {
        self.stored.as_ref().map(|s| s.cookies.clone()).filter(|c| !c.is_empty())
    }

    pub fn set(&mut self, cookies: Option<BTreeMap<String, String>>) -> std::io::Result<()> {
        self.check = None;
        match cookies.filter(|c| !c.is_empty()) {
            Some(cookies) => {
                self.stored = Some(Stored { cookies, saved_at: crate::project::now() });
                self.save()
            }
            None => {
                self.stored = None;
                let _ = std::fs::remove_file(self.root.join("cookies.dpapi"));
                let _ = std::fs::remove_file(self.root.join("cookies.json"));
                Ok(())
            }
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(s) = &self.stored else { return Ok(()) };
        let json = serde_json::to_vec_pretty(s).unwrap();
        if dpapi::AVAILABLE {
            let enc = dpapi::protect(&json).ok_or_else(|| std::io::Error::other("DPAPI encryption failed"))?;
            crate::project::store::write_atomic(&self.root.join("cookies.dpapi"), &enc)
        } else {
            crate::project::store::write_atomic(&self.root.join("cookies.json"), &json)
        }
    }

    pub fn record_check(&mut self, valid: Option<bool>, account: Option<String>) {
        self.check = Some((valid, crate::project::now(), account));
    }

    pub fn status(&self) -> CookieStatus {
        let names: Vec<String> = self.stored.as_ref().map(|s| s.cookies.keys().cloned().collect()).unwrap_or_default();
        let platforms: Vec<String> =
            names.iter().map(|n| platform_of(n).to_string()).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        CookieStatus {
            set: !names.is_empty(),
            platforms,
            names,
            saved_at: self.stored.as_ref().map(|s| s.saved_at.clone()).filter(|s| !s.is_empty()),
            encrypted: dpapi::AVAILABLE && self.stored.is_some(),
            valid: self.check.as_ref().and_then(|c| c.0),
            checked_at: self.check.as_ref().map(|c| c.1.clone()),
            account: self.check.as_ref().and_then(|c| c.2.clone()),
        }
    }
}

#[cfg(windows)]
mod dpapi {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData};

    pub const AVAILABLE: bool = true;

    /// App-specific entropy: other programs running as the user can't decrypt
    /// the blob with a plain CryptUnprotectData call.
    const ENTROPY: &[u8] = b"cliper/cookies/v1";

    fn run(data: &[u8], encrypt: bool, entropy: Option<&[u8]>) -> Option<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let ent = entropy.map(|e| CRYPT_INTEGER_BLOB { cbData: e.len() as u32, pbData: e.as_ptr() as *mut u8 });
        let ent_ptr = ent.as_ref().map_or(std::ptr::null(), |e| e as *const _);
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: valid input/entropy blobs that outlive the call; on success the
        // output buffer is LocalAlloc'd by the system, copied, zeroed, then freed.
        unsafe {
            let ok = if encrypt {
                CryptProtectData(&input, std::ptr::null(), ent_ptr, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), ent_ptr, std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out)
            };
            if ok == 0 || out.pbData.is_null() {
                return None;
            }
            let v = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            std::ptr::write_bytes(out.pbData, 0, out.cbData as usize);
            LocalFree(out.pbData as _);
            Some(v)
        }
    }

    pub fn protect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, true, Some(ENTROPY))
    }
    /// Also reads blobs written before the entropy was added.
    pub fn unprotect(data: &[u8]) -> Option<Vec<u8>> {
        run(data, false, Some(ENTROPY)).or_else(|| run(data, false, None))
    }
}

#[cfg(not(windows))]
mod dpapi {
    pub const AVAILABLE: bool = false;
    pub fn protect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
    pub fn unprotect(_: &[u8]) -> Option<Vec<u8>> {
        None
    }
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn dpapi_roundtrip() {
        let enc = super::dpapi::protect(b"NID_AUT=x").unwrap();
        assert_ne!(enc, b"NID_AUT=x");
        assert_eq!(super::dpapi::unprotect(&enc).unwrap(), b"NID_AUT=x");
    }
}
