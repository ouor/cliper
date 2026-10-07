//! ONNX Runtime setup: CUDA DLL discovery and session creation (CUDA EP → CPU).
//!
//! The CUDA execution provider (`onnxruntime_providers_cuda.dll`, CUDA 13 build)
//! needs cudart/cublas/cufft 13 and cuDNN 9 at load time. Windows resolves those
//! through the DLL search path, so known runtime folders are prepended to `PATH`
//! once, before the first session is created:
//!   1. `$CLIPER_CUDA_DIR`
//!   2. `<exe dir>/cuda`
//!   3. `runtime/cuda` in any ancestor of the exe (dev checkout)
//!   4. `$CUDA_PATH/bin/x64`, `$CUDA_PATH/bin`

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ort::ep;
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeInfo {
    pub cuda_dirs: Vec<PathBuf>,
}

static INIT: OnceLock<RuntimeInfo> = OnceLock::new();

fn candidate_dirs() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(d) = std::env::var_os("CLIPER_CUDA_DIR") {
        out.push(PathBuf::from(d));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent() {
            out.push(dir.join("cuda"));
            for anc in dir.ancestors() {
                let p = anc.join("runtime").join("cuda");
                if p.is_dir() {
                    out.push(p);
                    break;
                }
            }
        }
    if let Some(cp) = std::env::var_os("CUDA_PATH") {
        let cp = PathBuf::from(cp);
        out.push(cp.join("bin").join("x64"));
        out.push(cp.join("bin"));
    }
    out.into_iter().filter(|p| p.is_dir()).collect()
}

/// Register CUDA runtime folders on the DLL search path (idempotent).
pub fn init() -> &'static RuntimeInfo {
    INIT.get_or_init(|| {
        let dirs = candidate_dirs();
        if !dirs.is_empty() {
            let mut paths: Vec<PathBuf> = dirs.clone();
            if let Some(p) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&p));
            }
            if let Ok(joined) = std::env::join_paths(paths) {
                // SAFETY: called once, before inference threads exist.
                unsafe { std::env::set_var("PATH", joined) };
            }
        }
        RuntimeInfo { cuda_dirs: dirs }
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(rename_all = "lowercase")]
pub enum Device {
    Cuda,
    Cpu,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    #[error("model file not found: {0}")]
    Missing(PathBuf),
    #[error("onnxruntime: {0}")]
    Ort(String),
}

impl<T> From<ort::Error<T>> for ModelError {
    fn from(e: ort::Error<T>) -> Self {
        ModelError::Ort(e.to_string())
    }
}

/// Open `model`, preferring CUDA unless `prefer_cpu`. Returns the device used.
pub fn open_session(model: &Path, prefer_cpu: bool) -> Result<(Session, Device), ModelError> {
    if !model.is_file() {
        return Err(ModelError::Missing(model.to_path_buf()));
    }
    init();
    let build = |cuda: bool| -> Result<Session, ModelError> {
        let mut b = Session::builder()?.with_optimization_level(GraphOptimizationLevel::Level3)?;
        if cuda {
            b = b.with_execution_providers([ep::CUDA::default().build().error_on_failure()])?;
        }
        Ok(b.commit_from_file(model)?)
    };
    if !prefer_cpu {
        match build(true) {
            Ok(s) => return Ok((s, Device::Cuda)),
            Err(e) => tracing::warn!("CUDA execution provider unavailable, falling back to CPU: {e}"),
        }
    }
    let s = build(false)?;
    Ok((s, Device::Cpu))
}
