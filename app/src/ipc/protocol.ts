// Cliper engine IPC contract (JSON-RPC 2.0 over the `cliper serve` child's stdio).
// Data types in ./generated are produced from the Rust sources by ts-rs:
//   cd core && cargo test -p cliper-core --features ts export_bindings
// This file maps every method to its params/result and every notification to its
// params. Keep it in sync with core/crates/cliper-core/src/ipc/server.rs.

import type { ApiError } from "./generated/ApiError";
import type { AudioState } from "./generated/AudioState";
import type { CookieStatus } from "./generated/CookieStatus";
import type { Mute } from "./generated/Mute";
import type { Event } from "./generated/Event";
import type { ExportOptions } from "./generated/ExportOptions";
import type { ExportResult } from "./generated/ExportResult";
import type { JobInfo } from "./generated/JobInfo";
import type { Params } from "./generated/Params";
import type { Project } from "./generated/Project";
import type { ProjectSummary } from "./generated/ProjectSummary";
import type { QueueEntry } from "./generated/QueueEntry";
import type { QueueThrough } from "./generated/QueueThrough";
import type { ReportFormat } from "./generated/ReportFormat";
import type { SetlistMatch } from "./generated/SetlistMatch";
import type { Settings } from "./generated/Settings";
import type { Song } from "./generated/Song";
import type { StemsResult } from "./generated/StemsResult";
import type { SystemInfo } from "./generated/SystemInfo";

export const PROTOCOL_VERSION = 2;

/** Returned by every long-running method; follow it via job.* notifications. */
export interface JobHandle {
  job_id: string;
}

type NoParams = Record<string, never>;

export interface Methods {
  "system.info": { params: NoParams; result: SystemInfo };
  /** Start the CUDA provider now (loads the detection model, a few s); gpu.cuda_ok becomes known. */
  "system.probe_gpu": { params: NoParams; result: SystemInfo };
  /** Login cookies for private/membership VODs (e.g. chzzk NID_AUT/NID_SES); null clears.
   *  Stored encrypted (DPAPI) and kept across restarts. */
  "system.set_cookies": { params: { cookies: Record<string, string> | null }; result: CookieStatus };
  "system.cookies_status": { params: NoParams; result: CookieStatus };
  /** chzzk: is the login still valid? (SOOP: valid stays null) */
  "system.check_cookies": { params: NoParams; result: CookieStatus };
  /** Keep the PC awake while jobs run (persisted). */
  "system.set_keep_awake": { params: { enabled: boolean }; result: true };

  "project.list": { params: NoParams; result: ProjectSummary[] };
  /** Resolve a VOD (URL, chzzk number, "soop:<id>") or open a local recording (absolute
   *  path), creating — or reopening — its project. */
  "project.create": { params: { input: string }; result: Project };
  "project.get": { params: { id: string }; result: Project };
  "project.delete": { params: { id: string }; result: true };
  /** Replaces settings; re-plans clips (margins may have changed). */
  "project.update_settings": { params: { id: string; settings: Settings }; result: Project };
  /** Song list as CSV (UTF-8 BOM) / JSON / YouTube chapters / timestamp text (VOD times);
   *  `path` also writes it to a file. */
  "project.export_metadata": {
    params: { id: string; format: ReportFormat; include_disabled?: boolean; path?: string | null };
    result: { text: string; path: string | null };
  };

  /** Whole-list replacement (bulk edits, undo). New songs/mutes may use id "".
   *  Validated; manual mutes become confirmed; clips are re-planned.
   *  `base_revision` = project.revision the edit was based on (CONFLICT if stale). */
  "songs.set": { params: { id: string; songs: Song[]; base_revision?: number | null }; result: Project };
  /** Change some fields of one song (not id / clip_id / export / confidence). */
  "song.update": {
    params: { id: string; song_id: string; patch: Partial<Song>; base_revision?: number | null };
    result: Project;
  };
  "song.add": { params: { id: string; song: Song; base_revision?: number | null }; result: { project: Project; song_id: string } };
  "song.remove": { params: { id: string; song_id: string; base_revision?: number | null }; result: Project };
  /** Add (id "" or unknown) or replace one mute. */
  "mute.upsert": {
    params: { id: string; song_id: string; mute: Mute; base_revision?: number | null };
    result: { project: Project; mute_id: string };
  };
  "mute.remove": { params: { id: string; song_id: string; mute_id: string; base_revision?: number | null }; result: Project };
  /** Paste a setlist / fan timestamps. `apply: false` previews the matching;
   *  `apply: true` writes titles/artists (empty fields only unless `overwrite`). */
  "songs.match_setlist": {
    params: { id: string; text: string; tolerance?: number | null; apply?: boolean; overwrite?: boolean };
    result: SetlistMatch;
  };
  "clips.plan": { params: { id: string }; result: Project };
  /** Change a clip's VOD range; drops its video/stems so clips.fetch re-downloads it. */
  "clip.extend": { params: { id: string; clip_id: string; start: number; end: number }; result: Project };
  /** Delete downloaded video + stems (songs, mutes and edits stay). */
  "clip.drop_media": { params: { id: string; clip_id: string }; result: { project: Project; freed_bytes: number } };
  "project.drop_media": { params: { id: string }; result: { project: Project; freed_bytes: number } };
  /** Sample-exact float32 WAV of the analysis audio, [start, start+duration) VOD seconds. */
  "audio.window": {
    params: { id: string; start: number; duration: number; sample_rate?: number; channels?: 1 | 2 };
    result: { path: string; file: string; start: number; duration: number; sample_rate: number; channels: number };
  };

  "detect.default_params": { params: NoParams; result: Params };
  /** Re-run the segmenter on cached probabilities (instant). */
  "detect.rerun": { params: { id: string; params: Params }; result: Project };

  // ── jobs ──
  /** Audio only (VOD) or copy of the audio track (local file). */
  "audio.fetch": { params: { id: string }; result: JobHandle };
  /** PANNs tagging (+ overview.peaks) then detection. */
  "detect.run": { params: { id: string; params?: Params | null; retag?: boolean }; result: JobHandle };
  /** Per clip: video → align → stems → talk → mute suggestions. Select by clip_ids or
   *  song_ids (their clips); neither = every clip missing video or stems.
   *  `exact` + song_ids: clips covering only those songs (± margins), no neighbours. */
  "clips.fetch": {
    params: { id: string; clip_ids?: string[] | null; song_ids?: string[] | null; exact?: boolean };
    result: JobHandle;
  };
  /** Render + encode enabled songs (default: all), or write editor/DAW stems. */
  "export.run": { params: { id: string } & Partial<ExportOptions>; result: JobHandle };
  "job.list": { params: NoParams; result: JobInfo[] };
  "job.cancel": { params: { job_id: string }; result: boolean };

  // ── batch queue (sequential, unattended) ──
  "queue.add": { params: { inputs: string[]; through?: QueueThrough }; result: QueueEntry[] };
  "queue.list": { params: NoParams; result: QueueEntry[] };
  /** Cancel a waiting entry, or the running one (cancels its current job). */
  "queue.cancel": { params: { entry_id: string }; result: boolean };
  /** Drop finished / failed / cancelled entries. */
  "queue.clear": { params: NoParams; result: true };

  /** Cancels running jobs and exits. */
  "engine.shutdown": { params: NoParams; result: true };
}

export type Method = keyof Methods;
export type ParamsOf<M extends Method> = Methods[M]["params"];
export type ResultOf<M extends Method> = Methods[M]["result"];

/** Job results by kind (the `result` of job.done). */
export interface JobResults {
  "audio.fetch": { audio: AudioState };
  "detect.run": { segments: [number, number][]; songs: number; clips: number; candidates: number };
  "clips.fetch": { clips: string[] };
  "export.run": {
    exports: { song_id: string; result: ExportResult }[];
    stems: { song_id: string; stems: StemsResult }[];
  };
}

type Variant<E extends Event["event"]> = Omit<Extract<Event, { event: E }>, "event">;

/** Server → client notifications (JSON-RPC requests without id). */
export interface Notifications {
  /** Compatible when min_client_protocol <= PROTOCOL_VERSION <= protocol. */
  "engine.ready": { protocol: number; min_client_protocol: number; system: SystemInfo };
  "job.started": Variant<"job_started">;
  /** `progress.item` = clip id (clips.fetch) or song id (export.run); `item_index`/`items_total`
   *  place it in job.started's `items`. Show `note` (code + args), not `message`. */
  "job.progress": Variant<"job_progress">;
  "job.done": Variant<"job_done">;
  "job.failed": Variant<"job_failed">;
  /** Full project state after every mutation (from any request or job). */
  "project.changed": Variant<"project_changed">;
  /** Full queue after every change. */
  "queue.changed": Variant<"queue_changed">;
}

export type NotificationMethod = keyof Notifications;

/** JSON-RPC error object; `data.code` carries the engine's ApiError code. */
export interface RpcError {
  code: number;
  message: string;
  data?: { code: ApiError["code"] };
}
