//! Project directory store.
//!
//! ```text
//! <root>/<project id>/
//!   project.json          state (atomic writes)
//!   raw.json              platform API responses (metadata sidecar `raw` layer)
//!   audio.aac             analysis audio (lossless ADTS)
//!   probs.bin             PANNs probabilities (CLPROBS1)
//!   overview.peaks        waveform peaks of the whole VOD
//!   video/                downloaded HLS segments + index.json (see `video::store`)
//!   stems/<span id>/      orig.wav, vocals.wav, *.peaks (see `StemSpan`)
//!   clips/<clip id>/      adopted from schema @1 projects (source.mp4, stems)
//!   cache/                audio windows, playable views
//!   exports/              rendered songs (unless settings.output_dir)
//! ```
//!
//! Every path stored in the project is relative to this folder, so the folder
//! can be moved as a whole.

use std::path::{Path, PathBuf};

use super::model::{Project, now};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("project {0} not found")]
    NotFound(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("corrupt project.json: {0}")]
    Json(#[from] serde_json::Error),
    /// written by a newer engine (schema version)
    #[error("project {0} was saved by a newer Cliper (schema @{1}); update Cliper to open it")]
    Newer(String, u32),
}

/// Projects living outside the projects folder (C2-2): other project roots
/// (e.g. the previous projects folder) and single project folders opened
/// from elsewhere. New projects are always created in `root`.
#[derive(Debug, Default)]
struct Extra {
    roots: Vec<PathBuf>,
    projects: Vec<PathBuf>,
    /// project id → its folder (outside `root`)
    index: std::collections::HashMap<String, PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
    extra: std::sync::Arc<std::sync::RwLock<Extra>>,
}

/// The id stored in a folder's project.json (None if it isn't a project).
pub fn project_id_at(dir: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Id {
        id: String,
    }
    let b = std::fs::read(dir.join("project.json")).ok()?;
    serde_json::from_slice::<Id>(&b).ok().map(|x| x.id).filter(|id| Store::valid_id(id))
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), extra: Default::default() }
    }

    fn extra(&self) -> std::sync::RwLockReadGuard<'_, Extra> {
        self.extra.read().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        match self.extra().index.get(id) {
            Some(d) => d.clone(),
            None => self.root.join(id),
        }
    }

    /// Other project roots and single project folders to include; returns
    /// the ids skipped because a project with the same id is already known
    /// (the projects folder wins, then roots, then single folders, in order).
    pub fn set_locations(&self, roots: Vec<PathBuf>, projects: Vec<PathBuf>) -> Vec<(String, PathBuf)> {
        let mut index: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
        let mut skipped = Vec::new();
        let in_root = |id: &str| self.root.join(id).join("project.json").is_file();
        let mut add = |id: String, d: PathBuf, index: &mut std::collections::HashMap<String, PathBuf>| {
            if same_dir(&d, &self.root.join(&id)) {
                return;
            }
            if in_root(&id) || index.contains_key(&id) {
                if !index.get(&id).is_some_and(|x| same_dir(x, &d)) {
                    skipped.push((id, d));
                }
                return;
            }
            index.insert(id, d);
        };
        for r in &roots {
            for e in std::fs::read_dir(r).into_iter().flatten().flatten() {
                if let Some(id) = project_id_at(&e.path()) {
                    add(id, e.path(), &mut index);
                }
            }
        }
        for d in &projects {
            if let Some(id) = project_id_at(d) {
                add(id, d.clone(), &mut index);
            }
        }
        let mut x = self.extra.write().unwrap_or_else(std::sync::PoisonError::into_inner);
        *x = Extra { roots, projects, index };
        skipped
    }

    pub fn locations(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let x = self.extra();
        (x.roots.clone(), x.projects.clone())
    }

    /// Is the project outside the projects folder?
    pub fn is_external(&self, id: &str) -> bool {
        self.extra().index.contains_key(id)
    }

    pub fn path(&self, id: &str, rel: &str) -> PathBuf {
        self.dir(id).join(rel)
    }

    /// Project ids come from the client: only plain names (letters, digits,
    /// `-`, `_`, `.` not leading), never paths.
    pub fn valid_id(id: &str) -> bool {
        !id.is_empty()
            && id.len() <= 160
            && !id.starts_with('.')
            && id.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
            && !id.contains("..")
    }

    pub fn create(&self, id: &str, input: &str) -> Result<Project, StoreError> {
        if !Self::valid_id(id) {
            return Err(StoreError::NotFound(id.to_string()));
        }
        std::fs::create_dir_all(self.dir(id))?;
        let mut p = Project::new(id, input);
        self.save_raw(&p)?;
        p.dir = abs(&self.dir(id));
        Ok(p)
    }

    pub fn exists(&self, id: &str) -> bool {
        Self::valid_id(id) && self.path(id, "project.json").is_file()
    }

    pub fn load(&self, id: &str) -> Result<Project, StoreError> {
        if !Self::valid_id(id) {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let path = self.path(id, "project.json");
        let b = std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StoreError::NotFound(id.to_string()),
            _ => e.into(),
        })?;
        let v: serde_json::Value = serde_json::from_slice(&b)?;
        let ver = super::migrate::version_of(&v);
        let cur = super::migrate::current();
        if ver > cur {
            // never "migrate" (and so overwrite) what a newer engine wrote (A-2)
            return Err(StoreError::Newer(id.to_string(), ver));
        }
        let mut p: Project = if ver < cur {
            // one migration at a time (concurrent loads of the same project)
            static MIGRATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _g = MIGRATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let raw = std::fs::read(&path)?;
            let mut v: serde_json::Value = serde_json::from_slice(&raw)?;
            let ver = super::migrate::version_of(&v);
            if ver < cur {
                // the original stays next to it, once per old version
                let bak = path.with_file_name(format!("project.json.bak-@{ver}"));
                if !bak.exists() {
                    std::fs::write(&bak, &raw)?;
                }
                super::migrate::chain(&mut v, ver, &self.dir(id))?;
                let p: Project = serde_json::from_value(v)?;
                tracing::info!("migrated project {id} from @{ver} to {}", super::model::SCHEMA);
                self.save_raw(&p)?;
                p
            } else {
                serde_json::from_value(v)?
            }
        } else {
            serde_json::from_value(v)?
        };
        if p.uid.is_empty() {
            p.uid = super::model::new_uid();
            self.save_raw(&p)?;
        }
        relink_exports(&mut p, &self.dir(id));
        p.dir = abs(&self.dir(id));
        Ok(p)
    }

    /// Save with `updated_at` bumped; write-then-rename so a crash never leaves
    /// a half-written project.json.
    pub fn save(&self, p: &mut Project) -> Result<(), StoreError> {
        p.updated_at = now();
        p.revision += 1;
        self.save_raw(p)
    }

    fn save_raw(&self, p: &Project) -> Result<(), StoreError> {
        // `dir` is where it was loaded from, not part of the project
        let mut v = serde_json::to_value(p)?;
        if let Some(o) = v.as_object_mut() {
            o.remove("dir");
        }
        write_atomic(&self.path(&p.id, "project.json"), &serde_json::to_vec_pretty(&v)?)?;
        Ok(())
    }

    /// All projects, newest first.
    pub fn list(&self) -> Vec<Project> {
        let external: Vec<String> = self.extra().index.keys().cloned().collect();
        let mut out: Vec<Project> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| self.load(&e.file_name().to_string_lossy()).ok())
            .chain(external.iter().filter_map(|id| self.load(id).ok()))
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    /// Ids of every known project (projects folder + other locations).
    pub fn ids(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|id| self.exists(id))
            .collect();
        out.extend(self.extra().index.keys().cloned());
        out
    }

    pub fn delete(&self, id: &str) -> Result<(), StoreError> {
        if !Self::valid_id(id) {
            return Err(StoreError::NotFound(id.to_string()));
        }
        let d = self.dir(id);
        if !d.join("project.json").is_file() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        std::fs::remove_dir_all(d)?;
        Ok(())
    }
}

/// Export records are absolute (exports may go to an output folder): one that
/// pointed into the project's own `exports/` before the folder moved (or was
/// copied) is pointed at the same file here (QA-17).
fn abs(d: &Path) -> std::path::PathBuf {
    std::path::absolute(d).unwrap_or_else(|_| d.to_path_buf())
}

fn relink_exports(p: &mut Project, dir: &Path) {
    for s in p.songs.iter_mut() {
        let Some(e) = s.export.as_mut() else { continue };
        let old = Path::new(&e.path);
        if old.is_file() && old.starts_with(dir) {
            continue;
        }
        let in_old_exports = old.parent().and_then(|d| d.file_name()).is_some_and(|n| n == "exports");
        if let Some(name) = old.file_name().filter(|_| in_old_exports) {
            let here = dir.join("exports").join(name);
            if here.is_file() && !old.starts_with(dir) {
                e.path = here.to_string_lossy().into_owned();
            }
        }
    }
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Write `bytes` to a unique temp file next to `path`, flush it to disk, then
/// rename it over `path` (retrying briefly: on Windows an antivirus or indexer
/// can hold the target open for a moment).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}-{}.tmp", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    let mut delay = 20u64;
    for attempt in 0..6 {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && attempt < 5 => {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                delay *= 2;
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        }
    }
    unreachable!()
}
