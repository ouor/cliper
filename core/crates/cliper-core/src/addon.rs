//! CUDA add-on: the CUDA 13 / cuDNN 9 runtime DLLs the GPU path needs are not
//! shipped in the app (≈ 1.3 GB). When an NVIDIA GPU is present and no usable
//! runtime is found, the app asks the engine to download the add-on zip from
//! the v0.1.0 GitHub release (every later version uses the same asset), verify
//! its SHA-256 and unpack it under `%LOCALAPPDATA%\Cliper\addons\cuda-v1`. The
//! engine finds that folder at its next start ([`crate::ml::runtime`]).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use futures::StreamExt;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::progress::{CancellationToken, Progress, ProgressFn};

pub const CUDA_ADDON_VERSION: &str = "cuda-v1";
pub const CUDA_ADDON_URL: &str = "https://github.com/ouor/cliper/releases/download/v0.1.0/cliper-cuda-addon-v1-amd64.zip";
/// SHA-256 and size of the release asset (pinned: the engine refuses anything else)
pub const CUDA_ADDON_SHA256: &str = "e73b28d19705de569c50808998274cf9046c0d16e64e25a010b7e8eb2a422ba6";
pub const CUDA_ADDON_SIZE: u64 = 1012083860;

/// DLLs that make a folder a usable CUDA runtime for this engine.
pub const CUDA_REQUIRED: &[&str] = &[
    "cudart64_13.dll",
    "cublas64_13.dll",
    "cublasLt64_13.dll",
    "cufft64_12.dll",
    "cudnn64_9.dll",
    "cudnn_adv64_9.dll",
    "cudnn_cnn64_9.dll",
    "cudnn_engines_precompiled64_9.dll",
    "cudnn_engines_runtime_compiled64_9.dll",
    "cudnn_engines_tensor_ir64_9.dll",
    "cudnn_ext64_9.dll",
    "cudnn_graph64_9.dll",
    "cudnn_heuristic64_9.dll",
    "cudnn_ops64_9.dll",
];

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CudaAddon {
    pub version: String,
    pub url: String,
    /// download size in bytes
    pub size: u64,
    pub sha256: String,
    /// where it is (or will be) installed
    pub dir: PathBuf,
    pub installed: bool,
    /// an NVIDIA GPU is present but no usable CUDA runtime was found: install it
    pub needed: bool,
}

/// `%LOCALAPPDATA%\Cliper\addons\cuda-v1` (or `$CLIPER_ADDON_DIR\cuda-v1`).
pub fn cuda_addon_dir() -> PathBuf {
    let base = std::env::var_os("CLIPER_ADDON_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Cliper").join("addons")))
        .unwrap_or_else(|| std::env::temp_dir().join("Cliper").join("addons"));
    base.join(CUDA_ADDON_VERSION)
}

/// Does `dir` hold every DLL the CUDA path needs?
pub fn has_cuda_runtime(dir: &Path) -> bool {
    CUDA_REQUIRED.iter().all(|f| dir.join(f).is_file())
}

pub fn cuda_status(gpu_present: bool, runtime_dirs: &[PathBuf]) -> CudaAddon {
    let dir = cuda_addon_dir();
    let installed = has_cuda_runtime(&dir) && dir.join("addon.json").is_file();
    let elsewhere = runtime_dirs.iter().any(|d| d != &dir && has_cuda_runtime(d));
    CudaAddon {
        version: CUDA_ADDON_VERSION.into(),
        url: CUDA_ADDON_URL.into(),
        size: CUDA_ADDON_SIZE,
        sha256: CUDA_ADDON_SHA256.into(),
        needed: gpu_present && !installed && !elsewhere,
        installed,
        dir,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AddonError {
    #[error("download failed: {0}")]
    Net(String),
    #[error("the downloaded add-on doesn't match the expected checksum")]
    Checksum,
    #[error("the add-on archive is incomplete: missing {0}")]
    Incomplete(String),
    #[error("cancelled")]
    Cancelled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("archive: {0}")]
    Zip(String),
}

/// Download, verify and unpack the CUDA add-on. Returns the install folder.
pub async fn install_cuda(progress: ProgressFn, cancel: CancellationToken) -> Result<PathBuf, AddonError> {
    // QA can point at a local copy; the checksum stays pinned
    let url = std::env::var("CLIPER_CUDA_ADDON_URL").unwrap_or_else(|_| CUDA_ADDON_URL.to_string());
    install_from(&url, CUDA_ADDON_SHA256, &cuda_addon_dir(), progress, cancel).await
}

pub async fn install_from(url: &str, sha256: &str, dir: &Path, progress: ProgressFn, cancel: CancellationToken) -> Result<PathBuf, AddonError> {
    let parent = dir.parent().ok_or_else(|| AddonError::Zip("bad add-on folder".into()))?.to_path_buf();
    std::fs::create_dir_all(&parent)?;
    let part = parent.join(format!("{}.zip.part", CUDA_ADDON_VERSION));

    // 1) download (streamed to disk, hashed on the way); a dropped connection
    //    resumes with a Range request (a ~1 GB download over home Wi-Fi drops)
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(20))
        .read_timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| AddonError::Net(e.to_string()))?;
    let mut file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&part)?);
    let mut hasher = Sha256::new();
    let (mut done, mut total, mut attempt) = (0u64, 0u64, 0u32);
    loop {
        let mut req = client.get(url);
        if done > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={done}-"));
        }
        let resp = match req.send().await {
            Ok(r) if r.status().is_success() => r,
            Ok(r) if attempt >= 5 => return Err(AddonError::Net(format!("HTTP {}", r.status()))),
            Err(e) if attempt >= 5 => return Err(AddonError::Net(e.to_string())),
            _ => {
                attempt += 1;
                tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
                continue;
            }
        };
        if done > 0 && resp.status() != reqwest::StatusCode::PARTIAL_CONTENT {
            // the server ignored the range: start over
            file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&part)?);
            hasher = Sha256::new();
            done = 0;
        }
        if done == 0 {
            total = resp.content_length().unwrap_or(0);
        }
        let mut stream = resp.bytes_stream();
        let mut broke = None;
        loop {
            let next = tokio::select! {
                _ = cancel.cancelled() => {
                    drop(file);
                    let _ = std::fs::remove_file(&part);
                    return Err(AddonError::Cancelled);
                }
                x = stream.next() => x,
            };
            match next {
                None => break,
                Some(Ok(chunk)) => {
                    hasher.update(&chunk);
                    file.write_all(&chunk)?;
                    done += chunk.len() as u64;
                    progress(Progress { bytes: Some(done), ..Progress::new("addon.download", done, total.max(done)) });
                }
                Some(Err(e)) => {
                    broke = Some(e.to_string());
                    break;
                }
            }
        }
        match broke {
            None if total == 0 || done >= total => break,
            reason => {
                attempt += 1;
                if attempt > 5 {
                    return Err(AddonError::Net(reason.unwrap_or_else(|| "connection closed early".into())));
                }
                tracing::warn!("add-on download interrupted at {done}/{total} ({reason:?}); resuming");
                tokio::time::sleep(std::time::Duration::from_secs(2 * attempt as u64)).await;
            }
        }
    }
    file.flush()?;
    drop(file);
    let got = format!("{:x}", hasher.finalize());
    if !sha256.eq_ignore_ascii_case(&got) {
        let _ = std::fs::remove_file(&part);
        return Err(AddonError::Checksum);
    }

    // 2) unpack next to the target, then swap it in
    let r = place(&part, &got, dir, progress, cancel).await;
    let _ = std::fs::remove_file(&part);
    r
}

/// Install from a zip the user already has (e.g. downloaded from a private
/// release in the browser). Same pinned checksum as the download.
pub async fn install_cuda_from_file(zip: &Path, progress: ProgressFn, cancel: CancellationToken) -> Result<PathBuf, AddonError> {
    let dir = cuda_addon_dir();
    std::fs::create_dir_all(dir.parent().ok_or_else(|| AddonError::Zip("bad add-on folder".into()))?)?;
    let (zc, pc, cc) = (zip.to_path_buf(), progress.clone(), cancel.clone());
    let got = tokio::task::spawn_blocking(move || -> Result<String, AddonError> {
        let total = std::fs::metadata(&zc)?.len();
        let mut f = std::fs::File::open(&zc)?;
        let mut h = Sha256::new();
        let (mut buf, mut done) = (vec![0u8; 1 << 20], 0u64);
        loop {
            if cc.is_cancelled() {
                return Err(AddonError::Cancelled);
            }
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            done += n as u64;
            pc(Progress { bytes: Some(done), ..Progress::new("addon.verify", done, total) });
        }
        Ok(format!("{:x}", h.finalize()))
    })
    .await
    .map_err(|e| AddonError::Zip(e.to_string()))??;
    if !CUDA_ADDON_SHA256.eq_ignore_ascii_case(&got) {
        return Err(AddonError::Checksum);
    }
    place(zip, &got, &dir, progress, cancel).await
}

/// Unpack a verified add-on zip beside `dir`, check it, and swap it in.
async fn place(zip: &Path, sha: &str, dir: &Path, progress: ProgressFn, cancel: CancellationToken) -> Result<PathBuf, AddonError> {
    let parent = dir.parent().ok_or_else(|| AddonError::Zip("bad add-on folder".into()))?.to_path_buf();
    let got = sha.to_string();
    let staging = parent.join(format!("{}.staging", CUDA_ADDON_VERSION));
    let (zipc, stagingc, cancelc) = (zip.to_path_buf(), staging.clone(), cancel.clone());
    let r = tokio::task::spawn_blocking(move || unpack(&zipc, &stagingc, &progress, &cancelc))
        .await
        .map_err(|e| AddonError::Zip(e.to_string()))?;
    if let Err(e) = r {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if let Some(missing) = CUDA_REQUIRED.iter().find(|f| !staging.join(f).is_file()) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(AddonError::Incomplete(missing.to_string()));
    }
    std::fs::write(
        staging.join("addon.json"),
        serde_json::to_vec_pretty(&serde_json::json!({ "version": CUDA_ADDON_VERSION, "sha256": got, "installed_at": crate::project::now() })).unwrap(),
    )?;
    let _ = std::fs::remove_dir_all(dir);
    std::fs::rename(&staging, dir)?;
    Ok(dir.to_path_buf())
}

fn unpack(zip_path: &Path, out: &Path, progress: &ProgressFn, cancel: &CancellationToken) -> Result<(), AddonError> {
    let _ = std::fs::remove_dir_all(out);
    std::fs::create_dir_all(out)?;
    let mut z = zip::ZipArchive::new(std::fs::File::open(zip_path)?).map_err(|e| AddonError::Zip(e.to_string()))?;
    let n = z.len();
    for i in 0..n {
        if cancel.is_cancelled() {
            return Err(AddonError::Cancelled);
        }
        let mut f = z.by_index(i).map_err(|e| AddonError::Zip(e.to_string()))?;
        // flat archive of DLLs: no directories, no path components
        let Some(name) = f.enclosed_name().and_then(|p| p.file_name().map(|n| n.to_owned())) else { continue };
        if f.is_dir() {
            continue;
        }
        let mut w = std::io::BufWriter::new(std::fs::File::create(out.join(&name))?);
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let k = f.read(&mut buf)?;
            if k == 0 {
                break;
            }
            w.write_all(&buf[..k])?;
        }
        w.flush()?;
        progress(Progress::new("addon.extract", (i + 1) as u64, n as u64));
    }
    Ok(())
}

/// Remove the installed add-on (takes effect at the next engine start).
pub fn remove_cuda() -> std::io::Result<()> {
    let dir = cuda_addon_dir();
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_bad_checksum_and_unpacks_good_archive() {
        let root = std::env::temp_dir().join(format!("cliper-addon-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        // a zip with every required name
        let zp = root.join("a.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
            for f in CUDA_REQUIRED {
                z.start_file(*f, zip::write::SimpleFileOptions::default()).unwrap();
                z.write_all(f.as_bytes()).unwrap();
            }
            z.finish().unwrap();
        }
        let bytes = std::fs::read(&zp).unwrap();
        let sha = format!("{:x}", Sha256::digest(&bytes));
        // serve it over a tiny local HTTP server
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let mut req = [0u8; 1024];
                let _ = s.read(&mut req).await;
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len());
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(&bytes).await;
            }
        });
        let url = format!("http://{addr}/a.zip");
        let dir = root.join("cuda-v1");
        let noop = crate::progress::noop();
        let bad = install_from(&url, &"0".repeat(64), &dir, noop.clone(), CancellationToken::new()).await;
        assert!(matches!(bad, Err(AddonError::Checksum)));
        assert!(!dir.exists());
        install_from(&url, &sha, &dir, noop, CancellationToken::new()).await.unwrap();
        assert!(has_cuda_runtime(&dir) && dir.join("addon.json").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
}
