//! Engine-level tests: project mutations, optimistic concurrency, job
//! bookkeeping, clip planning interplay, IPC dispatch, queue, alignment and
//! audio windows. Tests that need ffmpeg or model files skip when those are
//! missing.

use super::*;
use crate::project::{Clip, Origin};

fn temp_root(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("cliper-test-{tag}-{}-{}", std::process::id(), project::new_id("")));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn engine(tag: &str) -> (Arc<Engine>, tokio::sync::mpsc::UnboundedReceiver<Event>, PathBuf) {
    let root = temp_root(tag);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let cfg = EngineConfig { projects_root: root.clone(), models_dir: root.join("models"), prefer_cpu: true, keep_awake: Some(false) };
    (Engine::new(cfg, tx), rx, root)
}

fn song(start: f64, end: f64) -> Song {
    Song {
        id: String::new(),
        title: String::new(),
        artist: String::new(),
        start,
        end,
        origin: Origin::Auto,
        enabled: true,
        reviewed: false,
        note: String::new(),
        confidence: None,
        clip_id: None,
        mutes: Vec::new(),
        mute_enabled: true,
        fade_in: None,
        fade_out: None,
        export: None,
    }
}

fn mute(start: f64, end: f64, origin: Origin) -> Mute {
    Mute { id: String::new(), start, end, fade_in: 0.2, fade_out: 0.2, origin, confirmed: false, gain_db: None }
}

/// A project with two songs, saved through the store.
async fn project_with_songs(eng: &Arc<Engine>) -> Project {
    let mut p = eng.store.create("t1", "test").unwrap();
    eng.store.save(&mut p).unwrap();
    eng.set_songs("t1", vec![song(100.0, 300.0), song(1000.0, 1200.0)], None).await.unwrap()
}

fn ffmpeg_available() -> bool {
    std::process::Command::new(ffmpeg::ffmpeg_path()).arg("-version").output().is_ok_and(|o| o.status.success())
}

#[tokio::test]
async fn revision_conflict_and_partial_updates() {
    let (eng, _rx, root) = engine("rev");
    let p = project_with_songs(&eng).await;
    let rev = p.revision;
    let sid = p.songs[0].id.clone();

    let p2 = eng.update_song("t1", &sid, json!({ "title": "Melt", "fade_out": 2.5 }), Some(rev)).await.unwrap();
    assert_eq!(p2.songs[0].title, "Melt");
    assert_eq!(p2.songs[0].fade_out, Some(2.5));
    assert_eq!(p2.revision, rev + 1);
    // the old revision is now stale
    let e = eng.update_song("t1", &sid, json!({ "title": "x" }), Some(rev)).await.unwrap_err();
    assert_eq!(e.code, "CONFLICT");
    assert_eq!(eng.get("t1").unwrap().songs[0].title, "Melt");
    // protected fields
    assert_eq!(eng.update_song("t1", &sid, json!({ "clip_id": "c" }), None).await.unwrap_err().code, "INVALID_PARAMS");
    // bad range
    assert_eq!(eng.update_song("t1", &sid, json!({ "end": 50.0 }), None).await.unwrap_err().code, "INVALID_PARAMS");

    // mutes: add (manual → confirmed), replace, remove
    let (p3, mid) = eng.upsert_mute("t1", &sid, mute(110.0, 112.0, Origin::Manual), None).await.unwrap();
    let m = p3.songs[0].mutes.iter().find(|m| m.id == mid).unwrap();
    assert!(m.confirmed);
    let mut m2 = m.clone();
    m2.end = 113.0;
    let (p4, mid2) = eng.upsert_mute("t1", &sid, m2, None).await.unwrap();
    assert_eq!(mid, mid2);
    assert_eq!(p4.songs[0].mutes.len(), 1);
    assert_eq!(p4.songs[0].mutes[0].end, 113.0);
    assert_eq!(eng.remove_mute("t1", &sid, "nope", None).await.unwrap_err().code, "NOT_FOUND");
    let p5 = eng.remove_mute("t1", &sid, &mid, None).await.unwrap();
    assert!(p5.songs[0].mutes.is_empty());

    // add / remove songs; duplicate ids in songs.set are made unique
    let (p6, new_id) = eng.add_song("t1", song(2000.0, 2100.0), None).await.unwrap();
    assert!(p6.songs.iter().any(|s| s.id == new_id));
    let mut dup = p6.songs.clone();
    dup[1].id = dup[0].id.clone();
    let p7 = eng.set_songs("t1", dup, None).await.unwrap();
    let ids: std::collections::HashSet<_> = p7.songs.iter().map(|s| &s.id).collect();
    assert_eq!(ids.len(), p7.songs.len());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn settings_are_validated() {
    let (eng, _rx, root) = engine("settings");
    project_with_songs(&eng).await;
    let s = Settings { mute_fade: -1.0, ..Default::default() };
    assert_eq!(eng.update_settings("t1", s).await.unwrap_err().code, "INVALID_PARAMS");
    let s = Settings { video_codec: "hevc_nvenc".into(), ..Default::default() };
    assert_eq!(eng.update_settings("t1", s).await.unwrap_err().code, "INVALID_PARAMS");
    let s = Settings { margin_before: 10.0, ..Default::default() };
    assert_eq!(eng.update_settings("t1", s).await.unwrap().settings.margin_before, 10.0);
    // path-like ids are rejected
    assert_eq!(eng.get("../t1").unwrap_err().code, "NOT_FOUND");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn jobs_busy_conflicts_wait_and_panics() {
    let (eng, _rx, root) = engine("jobs");
    project_with_songs(&eng).await;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let j1 = eng
        .spawn_job("audio.fetch", "t1", Vec::new(), move |_e, _c| async move {
            let _ = rx.await;
            Ok(json!({ "ok": 1 }))
        })
        .unwrap();
    // same kind, and a conflicting kind, are BUSY
    let busy = |r: ApiResult<String>| r.unwrap_err().code;
    assert_eq!(busy(eng.spawn_job("audio.fetch", "t1", Vec::new(), |_e, _c| async { Ok(json!(null)) })), "BUSY");
    assert_eq!(busy(eng.spawn_job("detect.run", "t1", Vec::new(), |_e, _c| async { Ok(json!(null)) })), "BUSY");
    // an unrelated kind is fine
    let j2 = eng.spawn_job("export.run", "t1", Vec::new(), |_e, _c| async { Ok(json!(2)) }).unwrap();
    assert_eq!(eng.wait_job(&j2).await.unwrap(), json!(2));
    tx.send(()).unwrap();
    assert_eq!(eng.wait_job(&j1).await.unwrap(), json!({ "ok": 1 }));

    // a panicking job fails with INTERNAL and doesn't stay BUSY
    let j3 = eng.spawn_job("detect.run", "t1", Vec::new(), |_e, _c| async { panic!("boom") }).unwrap();
    let e = eng.wait_job(&j3).await.unwrap_err();
    assert_eq!(e.code, "INTERNAL");
    assert!(e.message.contains("boom"));
    let j4 = eng.spawn_job("detect.run", "t1", Vec::new(), |_e, _c| async { Ok(json!(4)) }).unwrap();
    assert_eq!(eng.wait_job(&j4).await.unwrap(), json!(4));
    // the failure was recorded on the project, and cleared by the success
    assert!(eng.get("t1").unwrap().last_error.is_none());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn failed_job_is_recorded() {
    let (eng, _rx, root) = engine("lasterr");
    project_with_songs(&eng).await;
    let j = eng.spawn_job("clips.fetch", "t1", Vec::new(), |_e, _c| async { Err(ApiError::new("NETWORK", "offline")) }).unwrap();
    assert_eq!(eng.wait_job(&j).await.unwrap_err().code, "NETWORK");
    let le = eng.get("t1").unwrap().last_error.unwrap();
    assert_eq!((le.kind.as_str(), le.code.as_str()), ("clips.fetch", "NETWORK"));
    assert_eq!(eng.list()[0].last_error.as_ref().unwrap().code, "NETWORK");
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn drop_media_and_extend_keep_edits() {
    let (eng, _rx, root) = engine("media");
    let p = project_with_songs(&eng).await;
    // pretend the first clip was fetched
    let cid = p.songs[0].clip_id.clone().unwrap();
    let dir = eng.store.dir("t1").join(format!("clips/{cid}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("source.mp4"), vec![0u8; 4096]).unwrap();
    let sid = p.songs[0].id.clone();
    eng.upsert_mute("t1", &sid, mute(120.0, 125.0, Origin::Manual), None).await.unwrap();
    eng.mutate("t1", |p| {
        let c = p.clip_mut(&cid).unwrap();
        c.video = Some(VideoClip {
            path: dir.join("source.mp4"),
            variant: 0,
            resolution: "1x1".into(),
            segments: crate::hls::m3u8::SegRange { first: 0, last: 1, seg_start: c.start, trim: 0.0 },
            vod_start: c.start,
            duration: c.end - c.start,
            bytes: 4096,
            skipped_variants: Vec::new(),
        });
        Ok(())
    })
    .await
    .unwrap();

    let (p2, freed) = eng.drop_media("t1", None).await.unwrap();
    assert!(freed >= 4096);
    assert!(!dir.exists());
    assert!(p2.clips.iter().all(|c| c.video.is_none()));
    assert_eq!(p2.songs[0].mutes.len(), 1, "edits survive dropping media");

    // extend: the clip keeps its new range through re-planning
    let cid = p2.songs[0].clip_id.clone().unwrap();
    let p3 = eng.extend_clip("t1", &cid, 10.0, 400.0).await.unwrap();
    let c = p3.clip(&cid).unwrap();
    assert!(c.pinned && c.start == 10.0 && c.end == 400.0);
    let p4 = eng.update_song("t1", &sid, json!({ "title": "x" }), None).await.unwrap();
    assert_eq!(p4.clip(&cid).map(|c| (c.start, c.end)), Some((10.0, 400.0)));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn ipc_dispatch_shapes() {
    let (eng, _rx, root) = engine("ipc");
    let p = project_with_songs(&eng).await;
    let sid = p.songs[0].id.clone();
    let r = crate::ipc::server::dispatch(&eng, "song.update", json!({ "id": "t1", "song_id": sid, "patch": { "reviewed": true } })).await.unwrap();
    assert_eq!(r["songs"][0]["reviewed"], json!(true));
    let r = crate::ipc::server::dispatch(&eng, "mute.upsert", json!({ "id": "t1", "song_id": sid, "mute": serde_json::to_value(mute(101.0, 102.0, Origin::Auto)).unwrap() }))
        .await
        .unwrap();
    assert!(r["mute_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(r["project"]["songs"][0]["mutes"][0]["confirmed"], json!(false));
    assert_eq!(crate::ipc::server::dispatch(&eng, "nope", json!({})).await.unwrap_err().code, "METHOD_NOT_FOUND");
    assert_eq!(crate::ipc::server::dispatch(&eng, "song.update", json!({ "id": "t1" })).await.unwrap_err().code, "INVALID_PARAMS");
    let n = crate::ipc::server::event_to_notification(Event::JobStarted { job_id: "j".into(), kind: "k".into(), project_id: "p".into(), items: vec!["a".into()] });
    assert_eq!(n["method"], "job.started");
    assert!(n["params"].get("event").is_none());
    assert_eq!(n["params"]["items"], json!(["a"]));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn queue_reports_failures_and_cancels() {
    let (eng, _rx, root) = engine("queue");
    let added = eng.queue_add(vec!["definitely not a vod".into(), "also not".into()], crate::queue::QueueThrough::Detect);
    assert_eq!(added.len(), 2);
    eng.queue_cancel(&added[1].id);
    for _ in 0..100 {
        let q = eng.queue();
        if q.iter().all(|e| e.status != crate::queue::QueueStatus::Waiting && e.status != crate::queue::QueueStatus::Running) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let q = eng.queue();
    assert_eq!(q[0].status, crate::queue::QueueStatus::Failed);
    assert_eq!(q[0].error.as_ref().unwrap().code, "UNRECOGNIZED_INPUT");
    assert_eq!(q[1].status, crate::queue::QueueStatus::Cancelled);
    let _ = std::fs::remove_dir_all(root);
}

/// Noise-like test signal (deterministic), interleaved stereo.
fn noise(seconds: f64, rate: u32) -> Vec<f32> {
    let mut x = 0x1234_5678u32;
    (0..(seconds * rate as f64) as usize * 2)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            (x as f32 / u32::MAX as f32 - 0.5) * 0.5
        })
        .collect()
}

fn encode(src_wav: &Path, out: &Path, args: &[&str]) {
    let mut c = ffmpeg::command();
    c.arg("-y").arg("-i").arg(src_wav).args(args).arg(out);
    ffmpeg::run(c).unwrap();
}

#[tokio::test]
async fn audio_window_is_sample_exact_on_adts() {
    if !ffmpeg_available() {
        return;
    }
    let (eng, _rx, root) = engine("window");
    let mut p = eng.store.create("w1", "test").unwrap();
    let wavp = root.join("src.wav");
    wav::write_f32(&wavp, 48000, 2, &noise(30.0, 48000)).unwrap();
    let aac = eng.store.path("w1", "audio.aac");
    // no perceptual noise substitution: PNS bands are filled from the decoder's
    // random generator, so they can't be bit-identical when decoding starts
    // mid-stream (streams' encoders rarely use PNS; on the real chzzk audio the
    // window is bit-identical)
    encode(&wavp, &aac, &["-c:a", "aac", "-aac_pns", "0", "-b:a", "160k", "-f", "adts"]);
    let frames = adts_index(&aac).unwrap().len() as u32;
    p.source = Some(crate::source::SourceInfo {
        platform: crate::source::Platform::Chzzk,
        video_id: "w1".into(),
        url: String::new(),
        title: String::new(),
        channel: String::new(),
        duration: 30.0,
        container: crate::source::Container::Ts,
        variants: Vec::new(),
        audio_variant: 0,
        catalog: Default::default(),
        notes: Vec::new(),
        notices: Vec::new(),
        audio_tracks: Vec::new(),
    });
    p.audio = Some(project::AudioState {
        file: "audio.aac".into(),
        variant: 0,
        duration: frames as f64 * 1024.0 / 48000.0,
        index: crate::hls::audio_extract::SegIndex { first_segment: 0, frames: vec![frames], durations: vec![30.0], samples_per_frame: 1024, sample_rate: 48000 },
        fetched_bytes: 0,
        full_source_bytes: 0,
        fallback_segments: 0,
        elapsed_sec: 0.0,
    });
    eng.store.save(&mut p).unwrap();
    let full = ffmpeg::decode(&aac, 48000, 2, None, None).unwrap().read_all().unwrap();
    for start in [0.0, 3.21, 17.0] {
        let r = eng.audio_window("w1", start, 2.0, None, None).await.unwrap();
        let w = wav::read_f32(Path::new(r["path"].as_str().unwrap())).unwrap();
        let a = (start * 48000.0).round() as usize * 2;
        assert_eq!(w.samples.len(), 2 * 96000);
        let diff = w.samples.iter().zip(&full[a..]).map(|(x, y)| (x - y).abs()).fold(0f32, f32::max);
        assert!(diff < 1e-6, "window at {start}: max diff {diff}");
    }
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn alignment_measures_known_offset_and_flags_silence() {
    if !ffmpeg_available() {
        return;
    }
    let (eng, _rx, root) = engine("align");
    let rec = root.join("rec.wav");
    wav::write_f32(&rec, 48000, 2, &noise(90.0, 48000)).unwrap();
    // local project: the analysis audio is the recording itself
    let mut p = eng.store.create("a1", "test").unwrap();
    encode(&rec, &eng.store.path("a1", "audio.mka"), &["-c:a", "flac"]);
    p.source = Some(crate::local::source_info(&rec).unwrap());
    p.audio = Some(project::AudioState {
        file: "audio.mka".into(),
        variant: 0,
        duration: 90.0,
        index: crate::hls::audio_extract::SegIndex { first_segment: 0, frames: Vec::new(), durations: Vec::new(), samples_per_frame: 1024, sample_rate: 48000 },
        fetched_bytes: 0,
        full_source_bytes: 0,
        fallback_segments: 0,
        elapsed_sec: 0.0,
    });
    eng.store.save(&mut p).unwrap();
    // a "clip" whose t = 0 is recording t = 20.5 but which claims vod_start 20
    let clip = root.join("clip.m4a");
    let mut c = ffmpeg::command();
    c.arg("-y").args(["-ss", "20.5"]).arg("-i").arg(&rec).args(["-t", "60", "-c:a", "flac"]).arg(root.join("clip.mka"));
    ffmpeg::run(c).unwrap();
    let v = |path: PathBuf| VideoClip {
        path,
        variant: 0,
        resolution: String::new(),
        segments: crate::hls::m3u8::SegRange { first: 0, last: 0, seg_start: 20.0, trim: 0.0 },
        vod_start: 20.0,
        duration: 60.0,
        bytes: 0,
        skipped_variants: Vec::new(),
    };
    let a = eng.align_clip(&p, &v(root.join("clip.mka"))).await;
    assert!(a.ok && a.method == "measured", "{} {}", a.method, a.corr);
    assert!((a.offset - 0.5).abs() < 0.002, "offset {}", a.offset);

    // silent clip: not confirmed, falls back to the prediction (not a fake 0 match)
    let silent = root.join("silent.wav");
    wav::write_f32(&silent, 48000, 2, &vec![0.0; 48000 * 2 * 60]).unwrap();
    let _ = clip;
    let a = eng.align_clip(&p, &v(silent)).await;
    assert!(!a.ok && a.method == "predicted");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn probs_from_truncated_bytes_is_none() {
    let p = Probs { fps: 10.0, duration: 1.0, music: vec![0.1; 10], speech: vec![0.2; 10], sing: vec![0.3; 10] };
    let b = p.to_bytes();
    assert!(Probs::from_bytes(&b).is_some());
    assert!(Probs::from_bytes(&b[..b.len() - 5]).is_none());
    assert!(Probs::from_bytes(&b[..10]).is_none());
}

#[test]
fn resample_grid_matches_linear_interpolation() {
    // 11 frames over 1 s → 10 output frames at 10 fps, a straight ramp
    let frames: Vec<[f32; 3]> = (0..11).map(|i| [i as f32 / 10.0, 0.0, 1.0]).collect();
    let p = panns::resample_grid(&frames, 1.0);
    assert_eq!(p.music.len(), 10);
    for (i, v) in p.music.iter().enumerate() {
        assert!((v - i as f32 / 10.0).abs() < 1e-6);
    }
    assert!(panns::resample_grid(&[], 1.0).music.iter().all(|&v| v == 0.0));
    assert_eq!(panns::resample_grid(&[[0.5, 0.5, 0.5]], 0.5).music, vec![0.5; 5]);
}

#[test]
fn parsers_survive_garbage() {
    let mut x = 0x9e37_79b9u32;
    for len in [0usize, 7, 8, 16, 188, 376, 1000, 4096] {
        for _ in 0..200 {
            let mut b: Vec<u8> = (0..len)
                .map(|_| {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                })
                .collect();
            // sprinkle plausible headers
            if len >= 8 {
                b[4..8].copy_from_slice(b"moof");
                b[0] = 0x47;
            }
            let _ = crate::hls::mp4::iter_boxes(&b, 0, b.len());
            let _ = crate::hls::mp4::iter_segment_runs(&b);
            let _ = crate::hls::ts::extract_audio(&b);
        }
    }
}

#[test]
fn cookies_roundtrip_and_migrate() {
    let root = temp_root("cookies");
    // an older plaintext file is read (and migrated on Windows)
    std::fs::write(root.join("cookies.json"), br#"{"NID_AUT":"a","NID_SES":"b","AuthTicket":"c"}"#).unwrap();
    let jar = CookieJar::load(&root);
    assert_eq!(jar.get().unwrap().len(), 3);
    let st = jar.status();
    assert_eq!(st.platforms, vec!["chzzk".to_string(), "soop".to_string()]);
    if cfg!(windows) {
        assert!(st.encrypted && !root.join("cookies.json").exists());
        assert_eq!(CookieJar::load(&root).get().unwrap()["NID_SES"], "b");
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn separator_streams_like_whole_and_keeps_length() {
    let models = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../models");
    let spec = ModelSpec::voc_ft();
    if !models.join(&spec.file).is_file() {
        return;
    }
    let mut sep = crate::ml::separator::Separator::open(&models, spec.clone(), true).unwrap();
    let gen_len = spec.gen_size();
    let mix = noise(gen_len as f64 * 2.0 / 44100.0 + 0.3, 44100);
    let noop = crate::progress::noop();
    let c = CancellationToken::new();
    let whole = sep.separate(&mix, &noop, &c).unwrap();
    assert_eq!(whole.len(), mix.len());
    // the same through odd-sized reads
    let (mut pos, mut out) = (0usize, Vec::new());
    sep.separate_stream(
        &mut |b: &mut [f32]| {
            let n = b.len().min(2 * 777).min(mix.len() - pos);
            b[..n].copy_from_slice(&mix[pos..pos + n]);
            pos += n;
            Ok(n)
        },
        &mut |x: &[f32]| {
            out.extend_from_slice(x);
            Ok(())
        },
        None,
        &noop,
        &c,
    )
    .unwrap();
    assert_eq!(out, whole);
    // exact multiple of the hop length keeps its length too
    let exact = &mix[..gen_len * 2];
    assert_eq!(sep.separate(exact, &noop, &c).unwrap().len(), exact.len());
}

#[test]
fn clip_type_round_trips() {
    // older project.json files without the new fields still load
    let c: Clip = serde_json::from_value(json!({ "id": "c", "start": 1.0, "end": 2.0, "video": null, "stems": null })).unwrap();
    assert!(!c.pinned && c.alignment.is_none() && c.song_ids.is_empty());
}
