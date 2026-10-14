//! Engine-side settings files (A-6) and how they are kept safe (A-3).
//!
//! * App preferences ([`AppPrefs`]: keep-awake, video pause) are per user,
//!   not per projects folder: `serve --config <file>` (default
//!   `%LOCALAPPDATA%\Cliper\engine.json`).
//! * The location registry ([`Locations`]: other project roots and project
//!   folders opened from elsewhere) belongs to a projects folder:
//!   `<projects>/locations.json`.
//! * Older engines kept both in `<projects>/engine.json`; it is read once to
//!   fill the new files.
//!
//! A file that doesn't parse is never overwritten: it is set aside as
//! `<name>.corrupt-<time>`, the last good copy (`<name>.bak`, written before
//! every save) is used if there is one, and a warning is reported.
//!
//! [`RootLock`] keeps a second engine (another app instance, `cliper
//! process`, a portable copy) from using the same projects folder.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPrefs {
    /// keep the PC awake while jobs run
    pub keep_awake: bool,
    /// video downloads paused app-wide (`video.pause`)
    pub video_paused: bool,
}

impl Default for AppPrefs {
    fn default() -> Self {
        Self { keep_awake: true, video_paused: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Locations {
    /// other folders holding projects (e.g. the previous projects folder)
    pub project_roots: Vec<PathBuf>,
    /// single project folders opened from elsewhere
    pub project_dirs: Vec<PathBuf>,
}

/// Default per-user preferences file.
pub fn default_prefs_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Cliper").join("engine.json")
}

pub fn locations_path(root: &Path) -> PathBuf {
    root.join("locations.json")
}

/// The pre-split `<projects>/engine.json` (read only).
fn legacy(root: &Path) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(root.join("engine.json")).ok()?).ok()
}

/// Load `path`; a missing file is `None`. A file that doesn't parse is set
/// aside and its `.bak` used; the second value is a warning to report.
pub fn load_guarded<T: DeserializeOwned>(path: &Path) -> (Option<T>, Option<String>) {
    let parse = |p: &Path| std::fs::read(p).ok().and_then(|b| serde_json::from_slice::<T>(&b).ok());
    if !path.exists() {
        return (None, None);
    }
    if let Some(v) = parse(path) {
        return (Some(v), None);
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let bad = path.with_file_name(format!("{name}.corrupt-{}", chrono::Utc::now().format("%Y%m%d%H%M%S")));
    let moved = std::fs::rename(path, &bad).is_ok();
    let bak = parse(&path.with_file_name(format!("{name}.bak")));
    let w = format!(
        "{} could not be read{}; {}",
        path.display(),
        if moved { format!(" and was set aside as {}", bad.file_name().unwrap().to_string_lossy()) } else { String::new() },
        if bak.is_some() { "the last good copy was used" } else { "defaults are used" }
    );
    tracing::error!("{w}");
    (bak, Some(w))
}

/// Save `v` to `path`, keeping the previous file as `.bak`.
pub fn save_guarded<T: Serialize>(path: &Path, v: &T) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    if path.is_file() {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let _ = std::fs::copy(path, path.with_file_name(format!("{name}.bak")));
    }
    crate::project::store::write_atomic(path, &serde_json::to_vec_pretty(v).map_err(std::io::Error::other)?)
}

/// App preferences from `path` (filled from an old `<root>/engine.json` the first time).
pub fn load_prefs(path: &Path, root: &Path) -> (AppPrefs, Option<String>) {
    let (p, w) = load_guarded::<AppPrefs>(path);
    match p {
        Some(p) => (p, w),
        None => {
            let p = legacy(root).and_then(|v| serde_json::from_value::<AppPrefs>(v).ok()).unwrap_or_default();
            (p, w)
        }
    }
}

/// Location registry of a projects folder (filled from its old `engine.json` the first time).
pub fn load_locations(root: &Path) -> (Locations, Option<String>) {
    let (l, w) = load_guarded::<Locations>(&locations_path(root));
    match l {
        Some(l) => (l, w),
        None => (legacy(root).and_then(|v| serde_json::from_value::<Locations>(v).ok()).unwrap_or_default(), w),
    }
}

/// Exclusive use of a projects folder: `<projects>/.cliper.lock`, held open
/// without sharing for the engine's lifetime (Windows).
#[derive(Debug)]
pub struct RootLock(#[allow(dead_code)] std::fs::File);

pub fn lock_root(root: &Path) -> Result<RootLock, String> {
    let path = root.join(".cliper.lock");
    let open = || {
        let mut o = std::fs::OpenOptions::new();
        o.read(true).write(true).create(true).truncate(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            o.share_mode(0);
        }
        o.open(&path)
    };
    // an engine that was just dropped may still be winding down
    let mut last = None;
    for _ in 0..30 {
        match open() {
            Ok(mut f) => {
                use std::io::Write;
                let _ = writeln!(f, "pid {} since {}", std::process::id(), chrono::Utc::now().to_rfc3339());
                return Ok(RootLock(f));
            }
            Err(e) => {
                last = Some(e);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    Err(format!(
        "another Cliper engine is using the projects folder {} ({}); close it first",
        root.display(),
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_files_are_set_aside_and_the_backup_used() {
        let d = std::env::temp_dir().join(format!("cliper-prefs-{}-{}", std::process::id(), crate::project::new_id("")));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("locations.json");
        let l = Locations { project_roots: vec![PathBuf::from("C:/old")], project_dirs: vec![] };
        save_guarded(&f, &l).unwrap();
        save_guarded(&f, &l).unwrap(); // now there is a .bak too
        std::fs::write(&f, b"{ not json").unwrap();
        let (got, w) = load_guarded::<Locations>(&f);
        assert_eq!(got, Some(l));
        assert!(w.is_some());
        assert!(!f.exists(), "the bad file is not left to be overwritten");
        assert!(std::fs::read_dir(&d).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("locations.json.corrupt-")));
        // the old engine.json fills the new files once
        let root = d.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("engine.json"), br#"{"keep_awake":false,"video_paused":true,"project_roots":["C:/a"],"project_dirs":[]}"#).unwrap();
        let (p, _) = load_prefs(&d.join("prefs.json"), &root);
        assert!(!p.keep_awake && p.video_paused);
        assert_eq!(load_locations(&root).0.project_roots, vec![PathBuf::from("C:/a")]);
        // a second engine can't take the same folder
        let lk = lock_root(&root).unwrap();
        #[cfg(windows)]
        assert!(lock_root(&root).is_err());
        drop(lk);
        assert!(lock_root(&root).is_ok());
        let _ = std::fs::remove_dir_all(&d);
    }
}
