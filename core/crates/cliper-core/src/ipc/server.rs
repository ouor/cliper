//! `cliper serve`: newline-delimited JSON-RPC 2.0 on stdin/stdout.
//!
//! * Requests are handled concurrently; responses carry the request `id`.
//! * Engine events become notifications (`job.started`, `job.progress`,
//!   `job.done`, `job.failed`, `project.changed`), plus `engine.ready` at start.
//! * Logs go to stderr only; stdout carries protocol lines exclusively.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::analysis::detect::Params;
use crate::engine::{ApiError, Engine, EngineConfig, Event};
use crate::project::{Mute, Settings, Song};

/// 3: segment-based video (`video.*`, `stems.ensure`, `storage.*`, `media.drop`,
/// `vod.check`; `Project.clips` is gone, see docs/ipc.md "What changed").
pub const PROTOCOL_VERSION: u32 = 3;
/// Oldest client protocol this engine still serves (clients built for an
/// older protocol should warn).
pub const MIN_CLIENT_PROTOCOL: u32 = 3;

fn rpc_error(code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({ "code": code, "message": message });
    if let Some(d) = data {
        e["data"] = d;
    }
    e
}

fn api_error(e: &ApiError) -> Value {
    let code = if e.code == "INVALID_PARAMS" { -32602 } else { -32000 };
    let mut data = json!({ "code": e.code });
    if let Some(Value::Object(extra)) = &e.data {
        for (k, v) in extra {
            data[k] = v.clone();
        }
    }
    rpc_error(code, &e.message, Some(data))
}

pub fn event_to_notification(e: Event) -> Value {
    let method = match &e {
        Event::JobStarted { .. } => "job.started",
        Event::JobProgress { .. } => "job.progress",
        Event::JobDone { .. } => "job.done",
        Event::JobFailed { .. } => "job.failed",
        Event::ProjectChanged { .. } => "project.changed",
        Event::QueueChanged { .. } => "queue.changed",
        Event::VideoChanged { .. } => "video.changed",
        Event::VideoRequestDone { .. } => "video.request_done",
    };
    let mut params = serde_json::to_value(&e).unwrap_or(Value::Null);
    if let Some(o) = params.as_object_mut() {
        o.remove("event");
    }
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

fn p<T: serde::de::DeserializeOwned>(params: &Value) -> Result<T, ApiError> {
    serde_json::from_value(params.clone()).map_err(|e| ApiError::new("INVALID_PARAMS", e))
}

fn to_json<T: serde::Serialize>(v: T) -> Result<Value, ApiError> {
    serde_json::to_value(v).map_err(|e| ApiError::new("INTERNAL", e))
}

fn job(id: Result<String, ApiError>) -> Result<Value, ApiError> {
    id.map(|job_id| json!({ "job_id": job_id }))
}

#[derive(Deserialize)]
struct IdArg {
    id: String,
}
#[derive(Deserialize)]
struct CookiesArg {
    cookies: Option<BTreeMap<String, String>>,
}
#[derive(Deserialize)]
struct CreateArg {
    input: String,
}
#[derive(Deserialize)]
struct SettingsArg {
    id: String,
    settings: Settings,
}
#[derive(Deserialize)]
struct SongsArg {
    id: String,
    songs: Vec<Song>,
    base_revision: Option<u64>,
}
#[derive(Deserialize)]
struct SongUpdateArg {
    id: String,
    song_id: String,
    patch: Value,
    base_revision: Option<u64>,
}
#[derive(Deserialize)]
struct SongAddArg {
    id: String,
    song: Song,
    base_revision: Option<u64>,
}
#[derive(Deserialize)]
struct SongIdArg {
    id: String,
    song_id: String,
    base_revision: Option<u64>,
}
#[derive(Deserialize)]
struct MuteUpsertArg {
    id: String,
    song_id: String,
    mute: Mute,
    base_revision: Option<u64>,
}
#[derive(Deserialize)]
struct MuteRemoveArg {
    id: String,
    song_id: String,
    mute_id: String,
    base_revision: Option<u64>,
}
#[derive(Deserialize)]
struct DropMediaArg {
    id: String,
}
#[derive(Deserialize)]
struct MediaDropArg {
    id: String,
    level: crate::storage::DropLevel,
    #[serde(default)]
    force: bool,
}
#[derive(Deserialize)]
struct OptIdArg {
    id: Option<String>,
}
#[derive(Deserialize)]
struct VideoFetchArg {
    id: String,
    #[serde(flatten)]
    args: crate::engine::FetchArgs,
}
#[derive(Deserialize)]
struct VideoCancelArg {
    id: String,
    request_id: Option<String>,
}
#[derive(Deserialize)]
struct VideoPrioArg {
    id: String,
    request_id: String,
    priority: crate::video::store::Priority,
}
#[derive(Deserialize)]
struct RangesArg {
    id: String,
    ranges: Option<Vec<(f64, f64)>>,
}
#[derive(Deserialize)]
struct PauseArg {
    paused: bool,
}
#[derive(Deserialize)]
struct ViewArg {
    id: String,
    t: f64,
    end: Option<f64>,
}
#[derive(Deserialize)]
struct StemsArg {
    id: String,
    song_ids: Option<Vec<String>>,
    ranges: Option<Vec<(f64, f64)>>,
}
#[derive(Deserialize)]
struct PathArg {
    path: std::path::PathBuf,
}
#[derive(Deserialize)]
struct PinArg {
    id: String,
    pinned: bool,
}
#[derive(Deserialize, Default)]
struct NetArg {
    #[serde(default)]
    reset: bool,
}
#[derive(Deserialize)]
struct WindowArg {
    id: String,
    start: f64,
    duration: f64,
    sample_rate: Option<u32>,
    channels: Option<u32>,
}
#[derive(Deserialize)]
struct KeepAwakeArg {
    enabled: bool,
}
#[derive(Deserialize)]
struct RerunArg {
    id: String,
    params: Params,
}
#[derive(Deserialize)]
struct DetectArg {
    id: String,
    params: Option<Params>,
    #[serde(default)]
    retag: bool,
}
#[derive(Deserialize)]
struct ClipsArg {
    id: String,
    song_ids: Option<Vec<String>>,
}
#[derive(Deserialize)]
struct ExportArg {
    id: String,
    #[serde(flatten)]
    opts: crate::engine::ExportOptions,
}
#[derive(Deserialize)]
struct SetlistArg {
    id: String,
    text: String,
    tolerance: Option<f64>,
    #[serde(default)]
    apply: bool,
    #[serde(default)]
    overwrite: bool,
}
#[derive(Deserialize)]
struct MetaArg {
    id: String,
    format: crate::project::report::ReportFormat,
    #[serde(default)]
    include_disabled: bool,
    path: Option<String>,
}
#[derive(Deserialize)]
struct QueueAddArg {
    inputs: Vec<String>,
    #[serde(default)]
    through: crate::queue::QueueThrough,
}
#[derive(Deserialize)]
struct QueueIdArg {
    entry_id: String,
}
#[derive(Deserialize)]
struct JobArg {
    job_id: String,
}

pub async fn dispatch(eng: &Arc<Engine>, method: &str, params: Value) -> Result<Value, ApiError> {
    match method {
        "system.info" => to_json(eng.system_info()),
        "system.probe_gpu" => to_json(eng.probe_gpu().await?),
        "system.set_cookies" => to_json(eng.set_cookies(p::<CookiesArg>(&params)?.cookies)?),
        "system.cookies_status" => to_json(eng.cookies_status()),
        "system.check_cookies" => to_json(eng.check_cookies().await?),
        "addon.install_cuda" => {
            #[derive(Deserialize, Default)]
            struct A {
                zip: Option<std::path::PathBuf>,
            }
            let a: A = if params.is_null() { A::default() } else { p(&params)? };
            job(eng.job_install_cuda(a.zip))
        }
        "addon.remove_cuda" => {
            eng.remove_cuda_addon()?;
            Ok(json!(true))
        }
        "system.set_keep_awake" => {
            eng.set_keep_awake(p::<KeepAwakeArg>(&params)?.enabled)?;
            Ok(json!(true))
        }
        "project.list" => to_json(eng.list_async().await?),
        "project.create" => to_json(eng.create(&p::<CreateArg>(&params)?.input).await?),
        "project.get" => to_json(eng.get(&p::<IdArg>(&params)?.id)?),
        "project.delete" => {
            eng.delete(&p::<IdArg>(&params)?.id).await?;
            Ok(json!(true))
        }
        "project.update_settings" => {
            let a: SettingsArg = p(&params)?;
            to_json(eng.update_settings(&a.id, a.settings).await?)
        }
        "songs.set" => {
            let a: SongsArg = p(&params)?;
            to_json(eng.set_songs(&a.id, a.songs, a.base_revision).await?)
        }
        "song.update" => {
            let a: SongUpdateArg = p(&params)?;
            to_json(eng.update_song(&a.id, &a.song_id, a.patch, a.base_revision).await?)
        }
        "song.add" => {
            let a: SongAddArg = p(&params)?;
            let (project, song_id) = eng.add_song(&a.id, a.song, a.base_revision).await?;
            Ok(json!({ "project": project, "song_id": song_id }))
        }
        "song.remove" => {
            let a: SongIdArg = p(&params)?;
            to_json(eng.remove_song(&a.id, &a.song_id, a.base_revision).await?)
        }
        "mute.upsert" => {
            let a: MuteUpsertArg = p(&params)?;
            let (project, mute_id) = eng.upsert_mute(&a.id, &a.song_id, a.mute, a.base_revision).await?;
            Ok(json!({ "project": project, "mute_id": mute_id }))
        }
        "mute.remove" => {
            let a: MuteRemoveArg = p(&params)?;
            to_json(eng.remove_mute(&a.id, &a.song_id, &a.mute_id, a.base_revision).await?)
        }
        "project.drop_media" => {
            // protocol 2: video + stems
            let a: DropMediaArg = p(&params)?;
            let (project, freed) = eng.media_drop(&a.id, crate::storage::DropLevel::Video, false).await?;
            Ok(json!({ "project": project, "freed_bytes": freed }))
        }
        "media.drop" => {
            let a: MediaDropArg = p(&params)?;
            let (project, freed) = eng.media_drop(&a.id, a.level, a.force).await?;
            Ok(json!({ "project": project, "freed_bytes": freed }))
        }
        "storage.usage" => {
            let a: OptIdArg = if params.is_null() { OptIdArg { id: None } } else { p(&params)? };
            match a.id {
                Some(id) => {
                    let eng = eng.clone();
                    to_json(tokio::task::spawn_blocking(move || eng.storage_usage(&id)).await.map_err(|e| ApiError::new("INTERNAL", e))??)
                }
                None => eng.storage_all().await,
            }
        }
        "project.set_pinned" => {
            let a: PinArg = p(&params)?;
            to_json(eng.set_pinned(&a.id, a.pinned).await?)
        }
        "vod.check" => to_json(eng.vod_check(&p::<IdArg>(&params)?.id).await?),
        "project.open" => to_json(eng.open_project_at(&p::<PathArg>(&params)?.path).await?),
        "projects.add_root" => eng.add_root(&p::<PathArg>(&params)?.path),
        "projects.remove_location" => Ok(json!(eng.remove_location(&p::<PathArg>(&params)?.path)?)),
        "projects.locations" => Ok(eng.locations()),
        "video.status" => {
            let a: OptIdArg = if params.is_null() { OptIdArg { id: None } } else { p(&params)? };
            match a.id {
                Some(id) => to_json(eng.video_state(&id)?),
                None => to_json(eng.video_states()),
            }
        }
        "video.fetch" => {
            let a: VideoFetchArg = p(&params)?;
            to_json(eng.video_fetch(&a.id, a.args)?)
        }
        "video.estimate" => {
            let a: VideoFetchArg = p(&params)?;
            to_json(eng.video_estimate(&a.id, &a.args)?)
        }
        "video.cancel" => {
            let a: VideoCancelArg = p(&params)?;
            Ok(json!({ "cancelled": eng.video_cancel(&a.id, a.request_id.as_deref())? }))
        }
        "video.prioritize" => {
            let a: VideoPrioArg = p(&params)?;
            eng.video_prioritize(&a.id, &a.request_id, a.priority)?;
            Ok(json!(true))
        }
        "video.retry" => {
            let a: RangesArg = p(&params)?;
            to_json(eng.video_retry(&a.id, a.ranges)?)
        }
        "video.pause" => {
            eng.video_pause(p::<PauseArg>(&params)?.paused)?;
            Ok(json!(true))
        }
        "video.view" => {
            let a: ViewArg = p(&params)?;
            to_json(eng.video_view(&a.id, a.t, a.end).await?)
        }
        "stems.ensure" => {
            let a: StemsArg = p(&params)?;
            job(eng.job_stems(&a.id, a.song_ids, a.ranges))
        }
        "system.net_stats" => {
            let a: NetArg = if params.is_null() { NetArg::default() } else { p(&params)? };
            to_json(crate::http::net_stats(a.reset))
        }
        "clip.drop_media" | "clip.extend" => Err(ApiError::new(
            "INVALID_PARAMS",
            format!("{method} was removed in protocol 3: video is kept per VOD range (video.fetch, media.drop)"),
        )),
        "audio.window" => {
            let a: WindowArg = p(&params)?;
            eng.audio_window(&a.id, a.start, a.duration, a.sample_rate, a.channels).await
        }
        "songs.match_setlist" => {
            let a: SetlistArg = p(&params)?;
            to_json(eng.match_setlist(&a.id, &a.text, a.tolerance, a.apply, a.overwrite).await?)
        }
        "project.export_metadata" => {
            let a: MetaArg = p(&params)?;
            eng.export_metadata(&a.id, a.format, a.include_disabled, a.path.as_deref())
        }
        // protocol 2: nothing to plan any more
        "clips.plan" => to_json(eng.get(&p::<IdArg>(&params)?.id)?),
        "detect.default_params" => to_json(Params::default()),
        "detect.rerun" => {
            let a: RerunArg = p(&params)?;
            to_json(eng.rerun_detection(&a.id, a.params).await?)
        }
        "audio.fetch" => job(eng.job_fetch_audio(&p::<IdArg>(&params)?.id)),
        "detect.run" => {
            let a: DetectArg = p(&params)?;
            job(eng.job_detect(&a.id, a.params, a.retag))
        }
        "clips.fetch" => {
            let a: ClipsArg = p(&params)?;
            job(eng.job_fetch_clips(&a.id, a.song_ids))
        }
        "export.run" => {
            let a: ExportArg = p(&params)?;
            job(eng.job_export(&a.id, a.opts))
        }
        "queue.add" => {
            let a: QueueAddArg = p(&params)?;
            to_json(eng.queue_add(a.inputs, a.through))
        }
        "queue.list" => to_json(eng.queue()),
        "queue.cancel" => Ok(json!(eng.queue_cancel(&p::<QueueIdArg>(&params)?.entry_id))),
        "queue.clear" => {
            eng.queue_clear();
            Ok(json!(true))
        }
        "job.list" => to_json(eng.jobs()),
        "job.cancel" => Ok(json!(eng.cancel(&p::<JobArg>(&params)?.job_id))),
        _ => Err(ApiError::new("METHOD_NOT_FOUND", format!("unknown method {method}"))),
    }
}

/// Run the server until stdin closes or `engine.shutdown` is received.
pub async fn serve(cfg: EngineConfig) -> anyhow::Result<()> {
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<Event>();
    let eng = Engine::new(cfg, ev_tx);

    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(line) = out_rx.recv().await {
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    {
        let out = out_tx.clone();
        tokio::spawn(async move {
            while let Some(e) = ev_rx.recv().await {
                let _ = out.send(event_to_notification(e).to_string());
            }
        });
    }

    let ready = json!({ "jsonrpc": "2.0", "method": "engine.ready",
        "params": { "protocol": PROTOCOL_VERSION, "min_client_protocol": MIN_CLIENT_PROTOCOL, "system": eng.system_info() } });
    let _ = out_tx.send(ready.to_string());

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let err = rpc_error(-32700, &format!("parse error: {e}"), None);
                let _ = out_tx.send(json!({ "jsonrpc": "2.0", "id": null, "error": err }).to_string());
                continue;
            }
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        let bad = if req.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || method.is_empty() {
            Some(rpc_error(-32600, "invalid request (jsonrpc must be \"2.0\" and method a string)", None))
        } else if !(params.is_object() || params.is_null()) {
            Some(rpc_error(-32602, "params must be an object", None))
        } else {
            None
        };
        if let Some(err) = bad {
            if let Some(id) = id {
                let _ = out_tx.send(json!({ "jsonrpc": "2.0", "id": id, "error": err }).to_string());
            }
            continue;
        }
        if method == "engine.shutdown" {
            for j in eng.jobs() {
                eng.cancel(&j.job_id);
            }
            if let Some(id) = id {
                let _ = out_tx.send(json!({ "jsonrpc": "2.0", "id": id, "result": true }).to_string());
            }
            break;
        }
        let (eng, out) = (eng.clone(), out_tx.clone());
        tokio::spawn(async move {
            use futures::FutureExt;
            // a panic while handling a request still answers it
            let res = std::panic::AssertUnwindSafe(dispatch(&eng, &method, params))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| Err(ApiError::new("INTERNAL", format!("{method} panicked"))));
            let Some(id) = id else { return }; // JSON-RPC notification: no response
            let msg = match res {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(e) if e.code == "METHOD_NOT_FOUND" => {
                    json!({ "jsonrpc": "2.0", "id": id, "error": rpc_error(-32601, &e.message, None) })
                }
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": api_error(&e) }),
            };
            let _ = out.send(msg.to_string());
        });
    }
    // stdin closed or engine.shutdown: stop all work (jobs observe the token;
    // blocking work is bounded by the runtime's shutdown timeout in main)
    eng.cancel_all();
    drop(out_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    Ok(())
}
