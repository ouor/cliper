//! `cliper` — Cliper engine binary: `cliper serve` (JSON-RPC for the UI),
//! `cliper process` (headless batch) and step-by-step debug commands.

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
    /// Run the engine as a JSON-RPC 2.0 server on stdin/stdout (for the UI)
    Serve {
        #[arg(long, default_value = "projects")]
        projects: PathBuf,
        #[arg(long, default_value = "models")]
        models: PathBuf,
        #[arg(long)]
        cpu: bool,
        /// don't keep the PC awake while jobs run (overrides the saved preference)
        #[arg(long)]
        no_keep_awake: bool,
    },
    /// Headless batch: create projects for VODs / recordings and run audio +
    /// detection (and optionally clips + export). One JSON line per input on stdout.
    Process {
        /// VOD URLs, chzzk numbers, soop:<id>, or local recording paths
        #[arg(required = true)]
        inputs: Vec<String>,
        #[arg(long, default_value = "projects")]
        projects: PathBuf,
        #[arg(long, default_value = "models")]
        models: PathBuf,
        #[arg(long)]
        cpu: bool,
        /// also fetch video, separate and suggest mutes for every song
        #[arg(long)]
        clips: bool,
        /// also export every song (implies --clips; unconfirmed mutes are not applied)
        #[arg(long)]
        export: bool,
        /// export format: mp4 | wav | flac | mp3
        #[arg(long, default_value = "mp4")]
        format: FormatArg,
    },
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
    /// Talk (speech) regions in a vocal stem → mute suggestions
    Talk {
        /// clip directory holding orig.wav + vocals.wav
        clip_dir: PathBuf,
        /// song ranges inside the clip, "a-b" seconds (repeatable)
        #[arg(long)]
        song: Vec<String>,
        #[arg(long, default_value = "models/panns_sed.onnx")]
        model: PathBuf,
        /// the original heuristic, without edge relaxation / sing veto / refinement
        #[arg(long)]
        legacy: bool,
    },
    /// Export one song from a separated clip
    Export {
        /// clip directory holding source.mp4 (or --video), orig.wav, vocals.wav
        clip_dir: PathBuf,
        #[arg(long)]
        video: Option<PathBuf>,
        /// song in/out (clip seconds)
        #[arg(long = "in")]
        in_t: f64,
        #[arg(long = "out")]
        out_t: f64,
        /// vocal mute "a-b" in clip seconds (repeatable)
        #[arg(long)]
        mute: Vec<String>,
        #[arg(long, default_value_t = 0.3)]
        fade: f64,
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, default_value = "h264_nvenc")]
        codec: String,
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

#[derive(Clone, Copy, clap::ValueEnum)]
enum FormatArg {
    Mp4,
    Wav,
    Flac,
    Mp3,
}

impl FormatArg {
    fn parse_format(self) -> cliper_core::export::OutputFormat {
        use cliper_core::export::OutputFormat as F;
        match self {
            FormatArg::Mp4 => F::Mp4,
            FormatArg::Wav => F::Wav,
            FormatArg::Flac => F::Flac,
            FormatArg::Mp3 => F::Mp3,
        }
    }
}

fn load_cookies(p: &Option<PathBuf>) -> Result<Option<std::collections::BTreeMap<String, String>>> {
    Ok(match p {
        Some(p) => Some(serde_json::from_slice(&std::fs::read(p)?)?),
        None => None,
    })
}

fn main() -> Result<()> {
    kill_children_with_us();
    // DLL search path for ONNX Runtime before any worker thread exists
    cliper_core::ml::runtime::init();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let r = rt.block_on(run());
    // don't wait forever for blocking work (a separation, an encode) that
    // nobody is listening to any more
    rt.shutdown_timeout(std::time::Duration::from_secs(3));
    r
}

/// Put this process in a Job Object that kills every process in it when the
/// last handle closes — i.e. when cliper exits or is killed, its ffmpeg
/// children go too (they inherit the job).
#[cfg(windows)]
fn kill_children_with_us() {
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: plain Win32 calls; `info` lives across the call; the job handle is
    // intentionally leaked (closing it is what kills the children, at exit).
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok != 0 {
            AssignProcessToJobObject(job, GetCurrentProcess());
        }
    }
}

#[cfg(not(windows))]
fn kill_children_with_us() {}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().cmd {
        Cmd::Serve { projects, models, cpu, no_keep_awake } => {
            let cfg = cliper_core::engine::EngineConfig {
                projects_root: projects,
                models_dir: models,
                prefer_cpu: cpu,
                keep_awake: no_keep_awake.then_some(false),
            };
            cliper_core::ipc::server::serve(cfg).await?;
        }
        Cmd::Process { inputs, projects, models, cpu, clips, export, format } => {
            use cliper_core::engine::{Engine, EngineConfig, Event, ExportOptions};
            use cliper_core::queue::{QueueStatus, QueueThrough};
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
            let eng = Engine::new(EngineConfig { projects_root: projects, models_dir: models, prefer_cpu: cpu, keep_awake: None }, tx);
            // progress → stderr (one line per stage change / note)
            tokio::spawn(async move {
                let mut last = String::new();
                while let Some(e) = rx.recv().await {
                    if let Event::JobProgress { project_id, progress, .. } = e {
                        let line = match &progress.message {
                            Some(m) => format!("[{project_id}] {}: {m}", progress.stage),
                            None => format!("[{project_id}] {}", progress.stage),
                        };
                        if line != last {
                            eprintln!("{line}");
                            last = line;
                        }
                    }
                }
            });
            let through = if clips || export { QueueThrough::Clips } else { QueueThrough::Detect };
            let entries = eng.queue_add(inputs, through);
            let mut failed = 0;
            for entry in &entries {
                // wait for this entry
                let done = loop {
                    if let Some(e) = eng.queue().into_iter().find(|e| e.id == entry.id)
                        && matches!(e.status, QueueStatus::Done | QueueStatus::Failed | QueueStatus::Cancelled)
                    {
                        break e;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                };
                let mut out = serde_json::json!({ "input": done.input, "status": done.status, "project_id": done.project_id, "error": done.error });
                if done.status == QueueStatus::Done && export {
                    let pid = done.project_id.clone().unwrap();
                    let opts = ExportOptions { format: Some(format.parse_format()), ..Default::default() };
                    match eng.job_export(&pid, opts) {
                        Ok(j) => out["export"] = eng.wait_job(&j).await.unwrap_or_else(|e| serde_json::json!({ "error": e })),
                        Err(e) => out["export"] = serde_json::json!({ "error": e }),
                    }
                }
                if done.status != QueueStatus::Done {
                    failed += 1;
                }
                if let Some(pid) = &done.project_id
                    && let Ok(p) = eng.get(pid)
                {
                    out["songs"] = serde_json::json!(p.songs.iter().filter(|s| s.enabled).map(|s| (s.start, s.end)).collect::<Vec<_>>());
                }
                println!("{out}");
            }
            if failed > 0 {
                std::process::exit(1);
            }
        }
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
                } else if p.done.is_multiple_of(50) || p.done == p.total {
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
                cliper_core::stems::separate_clip_auto(&clip, &out_dir, &models, cliper_core::ml::separator::ModelSpec::voc_ft(), cpu, &progress, &CancellationToken::new())
            })
            .await??;
            eprintln!();
            println!("{}", serde_json::to_string_pretty(&st)?);
        }
        Cmd::Talk { clip_dir, song, model, legacy } => {
            use cliper_core::analysis::talk;
            let mut tagger = cliper_core::ml::panns::Tagger::open(&model, false)?;
            let c = CancellationToken::new();
            let v = talk::probs_of_file(&mut tagger, &clip_dir.join("vocals.wav"), &c)?;
            let m = talk::probs_of_file(&mut tagger, &clip_dir.join("orig.wav"), &c)?;
            let songs: Vec<(f64, f64)> = song
                .iter()
                .filter_map(|s| s.split_once('-').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
                .collect();
            let regs = if legacy {
                // the original heuristic (for comparisons)
                let tp = talk::TalkParams { edge_sec: 0.0, sing_veto: false, ..Default::default() };
                talk::talk_regions(&v, &m, &songs, &tp)
            } else {
                let tp = talk::TalkParams::default();
                talk::refine_edges(&talk::talk_regions(&v, &m, &songs, &tp), &clip_dir.join("vocals.wav"), &tp)?
            };
            for (a, b) in &regs {
                eprintln!("talk {:7.2} ~ {:7.2} ({:.1}s)", a, b, b - a);
            }
            println!("{}", serde_json::to_string(&regs)?);
        }
        Cmd::Export { clip_dir, video, in_t, out_t, mute, fade, output, codec } => {
            use cliper_core::export::{ExportSpec, export_song};
            use cliper_core::render::MuteSpan;
            let mutes = mute
                .iter()
                .filter_map(|s| s.split_once('-').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?))))
                .map(|(start, end)| MuteSpan { start, end, fade_in: fade, fade_out: fade, gain_db: None })
                .collect();
            let format = match output.extension().and_then(|e| e.to_str()).unwrap_or("mp4") {
                "wav" => cliper_core::export::OutputFormat::Wav,
                "flac" => cliper_core::export::OutputFormat::Flac,
                "mp3" => cliper_core::export::OutputFormat::Mp3,
                _ => cliper_core::export::OutputFormat::Mp4,
            };
            let spec = ExportSpec {
                source_mp4: video.unwrap_or_else(|| clip_dir.join("source.mp4")),
                orig_wav: clip_dir.join("orig.wav"),
                vocals_wav: clip_dir.join("vocals.wav"),
                audio_override: None,
                in_t,
                out_t,
                mutes,
                out: output,
                format,
                video_codec: codec,
                video_quality: 23,
                audio_bitrate_k: 320,
                fade_in: 0.3,
                fade_out: 1.5,
                loudness: None,
                metadata: Vec::new(),
            };
            let progress: cliper_core::progress::ProgressFn = Arc::new(|p: Progress| {
                if let Some(m) = p.message {
                    eprintln!("{}: {m}", p.stage);
                }
            });
            let r = tokio::task::spawn_blocking(move || export_song(&spec, &progress, &CancellationToken::new())).await??;
            println!("{}", serde_json::to_string_pretty(&r)?);
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
