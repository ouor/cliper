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
use crate::project::{Settings, Song};

pub const PROTOCOL_VERSION: u32 = 1;

fn rpc_error(code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({ "code": code, "message": message });
    if let Some(d) = data {
        e["data"] = d;
    }
    e
}

fn api_error(e: &ApiError) -> Value {
    let code = if e.code == "INVALID_PARAMS" { -32602 } else { -32000 };
    rpc_error(code, &e.message, Some(json!({ "code": e.code })))
}

pub fn event_to_notification(e: Event) -> Value {
    let method = match &e {
        Event::JobStarted { .. } => "job.started",
        Event::JobProgress { .. } => "job.progress",
        Event::JobDone { .. } => "job.done",
        Event::JobFailed { .. } => "job.failed",
        Event::ProjectChanged { .. } => "project.changed",
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
}
#[derive(Deserialize)]
struct ExtendArg {
    id: String,
    clip_id: String,
    start: f64,
    end: f64,
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
    clip_ids: Option<Vec<String>>,
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
struct JobArg {
    job_id: String,
}

pub async fn dispatch(eng: &Arc<Engine>, method: &str, params: Value) -> Result<Value, ApiError> {
    match method {
        "system.info" => to_json(eng.system_info()),
        "system.set_cookies" => {
            eng.set_cookies(p::<CookiesArg>(&params)?.cookies)?;
            Ok(json!(true))
        }
        "project.list" => to_json(eng.list()),
        "project.create" => to_json(eng.create(&p::<CreateArg>(&params)?.input).await?),
        "project.get" => to_json(eng.get(&p::<IdArg>(&params)?.id)?),
        "project.delete" => {
            eng.delete(&p::<IdArg>(&params)?.id)?;
            Ok(json!(true))
        }
        "project.update_settings" => {
            let a: SettingsArg = p(&params)?;
            to_json(eng.update_settings(&a.id, a.settings).await?)
        }
        "songs.set" => {
            let a: SongsArg = p(&params)?;
            to_json(eng.set_songs(&a.id, a.songs).await?)
        }
        "songs.match_setlist" => {
            let a: SetlistArg = p(&params)?;
            to_json(eng.match_setlist(&a.id, &a.text, a.tolerance, a.apply, a.overwrite).await?)
        }
        "project.export_metadata" => {
            let a: MetaArg = p(&params)?;
            eng.export_metadata(&a.id, a.format, a.include_disabled, a.path.as_deref())
        }
        "clips.plan" => to_json(eng.plan(&p::<IdArg>(&params)?.id).await?),
        "clip.extend" => {
            let a: ExtendArg = p(&params)?;
            to_json(eng.extend_clip(&a.id, &a.clip_id, a.start, a.end).await?)
        }
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
            job(eng.job_fetch_clips(&a.id, a.clip_ids, a.song_ids))
        }
        "export.run" => {
            let a: ExportArg = p(&params)?;
            job(eng.job_export(&a.id, a.opts))
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
        "params": { "protocol": PROTOCOL_VERSION, "system": eng.system_info() } });
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
            let res = dispatch(&eng, &method, params).await;
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
    drop(out_tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    Ok(())
}
