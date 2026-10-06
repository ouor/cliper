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
    /// Download the video of a VOD time range (segments covering it, stream copy)
    FetchVideo {
        video: String,
        #[arg(long)]
        start: f64,
        #[arg(long)]
        end: f64,
        #[arg(short, long)]
        out: PathBuf,
        /// target height (default: highest)
        #[arg(long)]
        height: Option<u32>,
        #[arg(short, long)]
        cookies: Option<PathBuf>,
    },
    /// Separate a clip into orig.wav / vocals.wav (MDX-Net)
    Separate {
        clip: PathBuf,
        #[arg(long)]
        out_dir: PathBuf,
        #[arg(long, default_value = "models")]
        models: PathBuf,
        #[arg(long)]
        cpu: bool,
    },
    /// Detect song sections in an audio/video file (PANNs + v6 segmenter)
    Detect {
        input: PathBuf,
        /// panns_sed.onnx (labels json next to it)
        #[arg(long, default_value = "models/panns_sed.onnx")]
        model: PathBuf,
        /// write probabilities (CLPROBS1) here / reuse if it exists
        #[arg(long)]
        probs: Option<PathBuf>,
        #[arg(long)]
        cpu: bool,
        /// write probabilities as JSON (for golden comparisons)
        #[arg(long)]
        probs_json: Option<PathBuf>,
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
        Cmd::FetchVideo { video, start, end, out, height, cookies } => {
            use cliper_core::hls::video_fetch::{self, VideoOptions};
            let src = cliper_core::platforms::resolve(&video, load_cookies(&cookies)?.as_ref()).await?;
            let progress = Arc::new(|p: Progress| {
                if let Some(m) = p.message {
                    eprintln!("\n{}: {m}", p.stage);
                } else {
                    eprint!("\r{} {}/{} ({:.1} MB)", p.stage, p.done, p.total, p.bytes.unwrap_or(0) as f64 / 1e6);
                }
            });
            let t0 = std::time::Instant::now();
            let clip = video_fetch::fetch_video(&src, start, end, &out, &VideoOptions { height, workers: None }, progress, CancellationToken::new()).await?;
            eprintln!("\ndone in {:.1}s", t0.elapsed().as_secs_f64());
            println!("{}", serde_json::to_string_pretty(&clip)?);
        }
        Cmd::Separate { clip, out_dir, models, cpu } => {
            let progress: cliper_core::progress::ProgressFn = Arc::new(|p: Progress| {
                if let Some(m) = p.message {
                    eprintln!("{}: {m}", p.stage);
                } else {
                    eprint!("\r{} {}/{}", p.stage, p.done, p.total);
                }
            });
            let st = tokio::task::spawn_blocking(move || {
                cliper_core::stems::separate_clip(&clip, &out_dir, &models, cliper_core::ml::separator::ModelSpec::voc_ft(), cpu, &progress, &CancellationToken::new())
            })
            .await??;
            eprintln!();
            println!("{}", serde_json::to_string_pretty(&st)?);
        }
        Cmd::Detect { input, model, probs, cpu, probs_json } => {
            use cliper_core::analysis::detect;
            use cliper_core::ml::panns::{self, Probs, Tagger};
            let cached = probs.as_ref().and_then(|p| std::fs::read(p).ok()).and_then(|b| Probs::from_bytes(&b));
            let pr = match cached {
                Some(p) => p,
                None => {
                    let t0 = std::time::Instant::now();
                    let mut tagger = Tagger::open(&model, cpu)?;
                    eprintln!("PANNs on {:?}", tagger.device);
                    let mut rd = cliper_core::audio::ffmpeg::decode(&input, panns::SAMPLE_RATE, 1, None, None)?;
                    let progress: cliper_core::progress::ProgressFn = Arc::new(|_p: Progress| {});
                    let pr = tagger.probs_streaming(
                        |buf| rd.read(buf).map_err(std::io::Error::other),
                        None,
                        &progress,
                        &CancellationToken::new(),
                    )?;
                    let el = t0.elapsed().as_secs_f64();
                    eprintln!("tagged {:.0}s of audio in {:.1}s ({:.0}x)", pr.duration, el, pr.duration / el);
                    if let Some(p) = &probs {
                        std::fs::write(p, pr.to_bytes())?;
                    }
                    pr
                }
            };
            if let Some(p) = probs_json {
                std::fs::write(p, serde_json::to_vec(&pr)?)?;
            }
            let segs = detect::segment(&pr, &detect::Params::default());
            for (i, (s, e)) in segs.iter().enumerate() {
                eprintln!("[{:02}] {} ~ {} ({:.0}s)", i + 1, hms(*s), hms(*e), e - s);
            }
            println!("{}", serde_json::to_string(&segs)?);
        }
    }
    Ok(())
}

fn hms(t: f64) -> String {
    let t = t as u64;
    format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}
