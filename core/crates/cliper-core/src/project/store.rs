//! Project directory store.
//!
//! ```text
//! <root>/<project id>/
//!   project.json          state (atomic writes)
//!   raw.json              platform API responses (metadata sidecar `raw` layer)
//!   audio.aac             analysis audio (lossless ADTS)
//!   probs.bin             PANNs probabilities (CLPROBS1)
//!   overview.peaks        waveform peaks of the whole VOD
//!   clips/<clip id>/source.mp4, orig.wav, vocals.wav, *.peaks
//!   exports/              rendered songs (unless settings.output_dir)
//! ```

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
}

#[derive(Debug, Clone)]
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn path(&self, id: &str, rel: &str) -> PathBuf {
        self.dir(id).join(rel)
    }

    pub fn create(&self, id: &str, input: &str) -> Result<Project, StoreError> {
        std::fs::create_dir_all(self.dir(id))?;
        let p = Project::new(id, input);
        self.save_raw(&p)?;
        Ok(p)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.path(id, "project.json").is_file()
    }

    pub fn load(&self, id: &str) -> Result<Project, StoreError> {
        let path = self.path(id, "project.json");
        let b = std::fs::read(&path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => StoreError::NotFound(id.to_string()),
            _ => e.into(),
        })?;
        Ok(serde_json::from_slice(&b)?)
    }

    /// Save with `updated_at` bumped; write-then-rename so a crash never leaves
    /// a half-written project.json.
    pub fn save(&self, p: &mut Project) -> Result<(), StoreError> {
        p.updated_at = now();
        self.save_raw(p)
    }

    fn save_raw(&self, p: &Project) -> Result<(), StoreError> {
        write_atomic(&self.path(&p.id, "project.json"), &serde_json::to_vec_pretty(p)?)?;
        Ok(())
    }

    /// All projects, newest first.
    pub fn list(&self) -> Vec<Project> {
        let mut out: Vec<Project> = std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| self.load(&e.file_name().to_string_lossy()).ok())
            .collect();
        out.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        out
    }

    pub fn delete(&self, id: &str) -> Result<(), StoreError> {
        let d = self.dir(id);
        if !d.join("project.json").is_file() {
            return Err(StoreError::NotFound(id.to_string()));
        }
        std::fs::remove_dir_all(d)?;
        Ok(())
    }
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}
