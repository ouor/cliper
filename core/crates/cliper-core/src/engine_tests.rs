//! Engine-level tests: project mutations, optimistic concurrency, job
//! bookkeeping, cleanup levels, migration of clip projects, IPC dispatch,
//! queue, alignment and audio windows. Tests that need ffmpeg or model files skip when those are
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
    let j2 = eng.spawn_job("vod.check", "t1", Vec::new(), |_e, _c| async { Ok(json!(2)) }).unwrap();
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

fn audio_state(file: &str, frames: Vec<u32>) -> project::AudioState {
    let n = frames.len();
    let dur = frames.iter().map(|&f| f as f64).sum::<f64>() * 1024.0 / 48000.0;
    project::AudioState {
        file: file.into(),
        variant: 0,
        duration: dur,
        index: crate::hls::audio_extract::SegIndex { first_segment: 0, frames, durations: vec![8.0; n], samples_per_frame: 1024, sample_rate: 48000 },
        fetched_bytes: 0,
        full_source_bytes: 0,
        fallback_segments: 0,
        elapsed_sec: 0.0,
        dropped: false,
    }
}

fn source_info(container: crate::source::Container) -> crate::source::SourceInfo {
    crate::source::SourceInfo {
        platform: crate::source::Platform::Chzzk,
        video_id: "x".into(),
        url: String::new(),
        title: String::new(),
        channel: String::new(),
        duration: 80.0,
        container,
        variants: vec![crate::hls::m3u8::Variant { width: 1920, height: 1080, bandwidth: 8_000_000, codecs: None, url: String::new() }],
        audio_variant: 0,
        catalog: Default::default(),
        notes: Vec::new(),
        notices: Vec::new(),
        audio_tracks: Vec::new(),
    }
}

fn fake_span(eng: &Arc<Engine>, pid: &str, id: &str, start: f64, secs: f64) -> project::StemSpan {
    let dir = eng.store.dir(pid).join(format!("stems/{id}"));
    std::fs::create_dir_all(&dir).unwrap();
    let frames = (secs * 48000.0) as u64;
    for f in ["orig.wav", "vocals.wav"] {
        wav::write_f32(&dir.join(f), 48000, 2, &vec![0.1; frames as usize * 2]).unwrap();
    }
    project::StemSpan {
        id: id.into(),
        start,
        end: start + secs,
        frames,
        sample_rate: 48000,
        dir: format!("stems/{id}"),
        model: "m".into(),
        device: runtime::Device::Cpu,
        created_at: project::now(),
    }
}

#[tokio::test]
async fn cleanup_levels_keep_decisions_and_respect_pin() {
    let (eng, _rx, root) = engine("cleanup");
    let mut p = eng.store.create("t2", "test").unwrap();
    p.source = Some(source_info(crate::source::Container::Ts));
    // ten 8 s segments
    p.audio = Some(audio_state("audio.aac", vec![375; 10]));
    p.settings.margin_before = 0.0;
    p.settings.margin_after = 0.0;
    let mut s = song(20.0, 30.0);
    s.id = "s1".into();
    s.mutes.push(mute(21.0, 22.0, Origin::Manual));
    let mut off = song(60.0, 70.0);
    off.id = "s2".into();
    off.enabled = false;
    p.songs = vec![s, off];
    p.pinned = true;
    std::fs::write(eng.store.path("t2", "audio.aac"), vec![1u8; 1000]).unwrap();
    for i in 0..10 {
        let f = vstore::segment_path(&eng.store.dir("t2"), 0, i, crate::source::Container::Ts);
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, vec![0x47u8; 188]).unwrap();
    }
    p.stems.push(fake_span(&eng, "t2", "st1", 16.0, 2.0));
    eng.store.save(&mut p).unwrap();

    let u = eng.storage_usage("t2").unwrap();
    assert_eq!((u.video, u.unused_video, u.audio), (1880, 188 * 8, 1000));
    assert_eq!(u.video_sec, 80.0);
    assert!(u.stems > 0 && u.pinned);

    // pinned: refused unless forced
    assert_eq!(eng.media_drop("t2", DropLevel::Unused, false).await.unwrap_err().code, "PINNED");
    let (_, freed) = eng.media_drop("t2", DropLevel::Unused, true).await.unwrap();
    assert_eq!(freed, 188 * 8);
    // segments 2 and 3 (16–32 s) hold the enabled song; the excluded one doesn't count
    let left: Vec<usize> = eng.with_ps("t2", |ps| ps.have.keys().copied().collect()).unwrap();
    assert_eq!(left, vec![2, 3]);
    assert_eq!(eng.video_state("t2").unwrap().have_sec, 16.0);

    eng.set_pinned("t2", false).await.unwrap();
    let (p, _) = eng.media_drop("t2", DropLevel::Stems, false).await.unwrap();
    assert!(p.stems.is_empty() && !eng.store.dir("t2").join("stems/st1").exists());
    assert_eq!(eng.video_state("t2").unwrap().have_sec, 16.0, "stems level keeps video");
    let (_, freed) = eng.media_drop("t2", DropLevel::Video, false).await.unwrap();
    assert_eq!(freed, 376);
    assert_eq!(eng.video_state("t2").unwrap().have_sec, 0.0);
    let (p, _) = eng.media_drop("t2", DropLevel::Audio, false).await.unwrap();
    assert!(p.audio.as_ref().unwrap().dropped && !eng.store.path("t2", "audio.aac").exists());
    // decisions stay
    assert_eq!(p.songs.len(), 2);
    assert_eq!(p.songs[0].mutes.len(), 1);
    assert!(eng.list()[0].pinned == Some(p.pinned) && !eng.list()[0].has_audio);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn video_requests_need_audio_and_respect_gone_vods() {
    let (eng, _rx, root) = engine("vreq");
    let mut p = eng.store.create("t3", "test").unwrap();
    eng.store.save(&mut p).unwrap();
    let r = crate::ipc::server::dispatch(&eng, "video.fetch", json!({ "id": "t3", "ranges": [[0.0, 10.0]] })).await;
    assert_eq!(r.unwrap_err().code, "PRECONDITION");
    p = eng.get("t3").unwrap();
    p.source = Some(source_info(crate::source::Container::Ts));
    p.audio = Some(audio_state("audio.aac", vec![375; 10]));
    p.vod = Some(project::VodStatus { status: project::VodState::Gone, checked_at: project::now(), code: None });
    eng.store.save(&mut p).unwrap();
    eng.reload_ps("t3").unwrap();
    // estimate works offline: 0–10 s widened by the edge margin → 2 segments
    let e = eng.video_estimate("t3", &FetchArgs { ranges: Some(vec![(0.0, 10.0)]), ..Default::default() }).unwrap();
    assert_eq!((e.total_sec, e.missing_sec), (16.0, 16.0));
    assert_eq!(e.estimated_bytes, 16_000_000);
    assert_eq!(eng.video_fetch("t3", FetchArgs { ranges: Some(vec![(0.0, 10.0)]), ..Default::default() }).unwrap_err().code, "VOD_GONE");
    let r = crate::ipc::server::dispatch(&eng, "video.status", json!({ "id": "t3" })).await.unwrap();
    assert_eq!(r["ranges"], json!([]));
    assert_eq!(crate::ipc::server::dispatch(&eng, "clip.extend", json!({})).await.unwrap_err().code, "INVALID_PARAMS");
    let n = crate::ipc::server::dispatch(&eng, "system.net_stats", serde_json::Value::Null).await.unwrap();
    assert_eq!(n["limit"], json!(4));
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn clip_projects_are_adopted_without_downloading() {
    let (eng, _rx, root) = engine("migrate");
    let dir = root.join("old1");
    std::fs::create_dir_all(dir.join("clips/c1")).unwrap();
    std::fs::write(dir.join("clips/c1/source.mp4"), vec![0u8; 3000]).unwrap();
    for f in ["orig.wav", "vocals.wav"] {
        wav::write_f32(&dir.join("clips/c1").join(f), 48000, 2, &vec![0.0; 48000 * 2 * 3]).unwrap();
    }
    let mut p = Project::new("old1", "x");
    p.schema = "cliper/project@1".into();
    p.source = Some(source_info(crate::source::Container::Fmp4));
    p.audio = Some(audio_state("audio.aac", vec![375; 10]));
    let mut s = song(17.0, 19.0);
    s.id = "s1".into();
    s.clip_id = Some("c1".into());
    p.songs.push(s);
    p.clips.push(Clip {
        id: "c1".into(),
        start: 15.0,
        end: 21.0,
        video: Some(crate::hls::video_fetch::VideoClip {
            path: PathBuf::from("C:/somewhere/else/source.mp4"),
            variant: 0,
            resolution: "1920x1080".into(),
            segments: crate::hls::m3u8::SegRange { first: 1, last: 3, seg_start: 8.0, trim: 0.0 },
            vod_start: 8.0,
            duration: 16.0,
            bytes: 3000,
            skipped_variants: Vec::new(),
        }),
        offset: 0.0123,
        stems: Some(crate::stems::Stems {
            orig: PathBuf::from("C:/somewhere/else/orig.wav"),
            vocals: PathBuf::from("C:/somewhere/else/vocals.wav"),
            sample_rate: 48000,
            frames: 48000 * 3,
            model: "m".into(),
            device: runtime::Device::Cuda,
            elapsed_sec: 1.0,
        }),
        talk: vec![(17.5, 18.0)],
        song_ids: vec!["s1".into()],
        pinned: false,
        alignment: None,
    });
    std::fs::write(dir.join("project.json"), serde_json::to_vec(&p).unwrap()).unwrap();

    let q = eng.get("old1").unwrap();
    assert_eq!(q.schema, project::SCHEMA);
    assert!(q.clips.is_empty() && q.songs[0].clip_id.is_none());
    assert_eq!(q.stems.len(), 1);
    let sp = &q.stems[0];
    assert_eq!(sp.dir, "clips/c1");
    assert_eq!((sp.start * 48000.0).round() as i64, ((8.0123f64) * 48000.0).round() as i64);
    assert_eq!(sp.frames, 48000 * 3);
    assert_eq!(q.talk, vec![(17.5, 18.0)]);
    assert_eq!(q.talk_scanned, vec![(8.0123, 24.0123)]);
    // the clip's two segments count as video on disk; nothing is queued
    let st = eng.video_state("old1").unwrap();
    assert_eq!(st.have_sec, 16.0);
    assert!(st.requests.is_empty());
    assert_eq!(st.ranges[0].height, Some(1080));
    // saved: loading again doesn't migrate twice
    let idx = vstore::VideoIndex::load(&dir);
    assert_eq!(idx.legacy.len(), 1);
    assert_eq!((idx.legacy[0].first, idx.legacy[0].last, idx.legacy[0].file.as_str()), (1, 3, "clips/c1/source.mp4"));
    let _ = eng.get("old1").unwrap();
    assert_eq!(vstore::VideoIndex::load(&dir).legacy.len(), 1);
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
    let n = crate::ipc::server::event_to_notification(Event::VideoRequestDone {
        project_id: "p".into(),
        request_id: "r".into(),
        status: "done".into(),
        failed: vec![],
        code: None,
    });
    assert_eq!(n["method"], "video.request_done");
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
        dropped: false,
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
    let (_eng, _rx, root) = engine("align");
    let rec = root.join("rec.wav");
    let sig = noise(90.0, 48000);
    wav::write_f32(&rec, 48000, 2, &sig).unwrap();
    // a stretch whose media t = 0 is VOD 20.5 (so m = −20.5), predicted as −20.0
    let clip = root.join("clip.mka");
    let mut c = ffmpeg::command();
    c.arg("-y").args(["-ss", "20.5"]).arg("-i").arg(&rec).args(["-t", "60", "-c:a", "flac"]).arg(&clip);
    ffmpeg::run(c).unwrap();
    let analysis = |s: f64, d: f64| -> ApiResult<Vec<f32>> { Ok(ffmpeg::decode(&rec, 8000, 1, Some(s), Some(d))?.read_all()?) };
    let pieces = [crate::video::media::Piece::Local { file: clip.clone(), track: 0 }];
    let a = video_impl::measure_m(&root, crate::source::Container::Fmp4, &pieces, 0.0, -20.0, 0, analysis);
    assert!(a.ok && a.method == "measured", "{} {}", a.method, a.corr);
    assert!((a.m + 20.5).abs() < 0.002, "m {}", a.m);

    // silent stretch: not confirmed, falls back to the prediction (not a fake match)
    let silent = root.join("silent.wav");
    wav::write_f32(&silent, 48000, 2, &vec![0.0; 48000 * 2 * 60]).unwrap();
    let pieces = [crate::video::media::Piece::Local { file: silent, track: 0 }];
    let a = video_impl::measure_m(&root, crate::source::Container::Fmp4, &pieces, 0.0, -20.0, 0, analysis);
    assert!(!a.ok && a.method == "predicted" && a.m == -20.0);
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
