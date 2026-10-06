//! `cliper` — Cliper engine binary (debug CLI; `serve` IPC mode comes later).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use cliper_core::hls::audio_extract::{self, ExtractOptions};
use cliper_core::progress::{CancellationToken, Progress};

#[derive(Parser)]
#[command(name = "cliper", version, about = "Cliper engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Resolve a VOD and print its metadata / variants as JSON
    Resolve {
        video: String,
        #[arg(short, long)]
        cookies: Option<PathBuf>,
    },
    /// Download the VOD's audio only (lossless ADTS .aac)
    FetchAudio {
        video: String,
        #[arg(short, long)]
        out: PathBuf,
        #[arg(short, long)]
        cookies: Option<PathBuf>,
        #[arg(short, long)]
        workers: Option<usize>,
        /// only segments [first, last)
        #[arg(long, num_args = 2)]
        segments: Option<Vec<usize>>,
        /// take audio from the variant nearest this height
        #[arg(long)]
        variant_height: Option<u32>,
    },
}

fn load_cookies(p: &Option<PathBuf>) -> Result<Option<std::collections::BTreeMap<String, String>>> {
    Ok(match p {
        Some(p) => Some(serde_json::from_slice(&std::fs::read(p)?)?),
        None => None,
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().cmd {
        Cmd::Resolve { video, cookies } => {
            let src = cliper_core::platforms::resolve(&video, load_cookies(&cookies)?.as_ref()).await?;
            println!("{}", serde_json::to_string_pretty(&src.info)?);
            eprintln!("audio variant: {} segments, {:.1}s", src.playlist.segments.len(), src.playlist.total_duration());
        }
        Cmd::FetchAudio { video, out, cookies, workers, segments, variant_height } => {
            let src = cliper_core::platforms::resolve(&video, load_cookies(&cookies)?.as_ref()).await?;
            eprintln!("{} — {} [{:?}, {} segments]", src.info.video_id, src.info.title, src.info.container, src.playlist.segments.len());
            let opts = ExtractOptions { segments: segments.map(|v| (v[0], v[1])), workers, variant_height };
            let progress = Arc::new(|p: Progress| {
                if let Some(m) = p.message {
                    eprintln!("\nnote: {m}");
                } else if p.done % 50 == 0 || p.done == p.total {
                    eprint!("\r{} {}/{} ({:.1} MB)", p.stage, p.done, p.total, p.bytes.unwrap_or(0) as f64 / 1e6);
                }
            });
            let res = audio_extract::extract_audio(&src, &out, &opts, progress, CancellationToken::new()).await?;
            eprintln!();
            println!("{}", serde_json::to_string_pretty(&serde_json::json!({
                "out": out, "duration": res.index.duration(), "config": res.config,
                "fetched_bytes": res.fetched_bytes, "full_source_bytes": res.full_source_bytes,
                "saving": 1.0 - res.fetched_bytes as f64 / res.full_source_bytes.max(1) as f64,
                "fallback_segments": res.fallback_segments, "elapsed_sec": res.elapsed_sec,
            }))?);
        }
    }
    Ok(())
}
