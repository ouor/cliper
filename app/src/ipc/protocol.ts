// Cliper engine IPC contract (JSON-RPC 2.0 over the `cliper serve` child's stdio).
// Data types in ./generated are produced from the Rust sources by ts-rs:
//   cd core && cargo test -p cliper-core --features ts export_bindings
// This file maps every method to its params/result and every notification to its
// params. Keep it in sync with core/crates/cliper-core/src/ipc/server.rs.

import type { ApiError } from "./generated/ApiError";
import type { AudioState } from "./generated/AudioState";
import type { CookieStatus } from "./generated/CookieStatus";
import type { DropLevel } from "./generated/DropLevel";
import type { EnsureResult } from "./generated/EnsureResult";
import type { FetchArgs } from "./generated/FetchArgs";
import type { FetchPlan } from "./generated/FetchPlan";
import type { NetStats } from "./generated/NetStats";
import type { Priority } from "./generated/Priority";
import type { StorageUsage } from "./generated/StorageUsage";
import type { VideoState } from "./generated/VideoState";
import type { VideoView } from "./generated/VideoView";
import type { VodStatus } from "./generated/VodStatus";
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

// Protocol 3: segment-based video (docs/ipc.md "What changed in protocol 3"). The UI uses
// video.* / stems.ensure / project.stems; Project.clips is always [].
export const PROTOCOL_VERSION = 3;

/** `storage.usage {}`: every project + free bytes on the projects drive. */
export interface StorageAll {
  projects: StorageUsage[];
  free: number;
  root: string;
}

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
  /** Protocol 2 leftover: no-op, returns the project. */
  "clips.plan": { params: { id: string }; result: Project };
  /** Removed in protocol 3 (INVALID_PARAMS): video is kept per VOD range. */
  "clip.extend": { params: { id: string; clip_id: string; start: number; end: number }; result: Project };
  /** Removed in protocol 3 (INVALID_PARAMS): use media.drop. */
  "clip.drop_media": { params: { id: string; clip_id: string }; result: { project: Project; freed_bytes: number } };
  /** Protocol 2 name of media.drop {level: "video"}. */
  "project.drop_media": { params: { id: string }; result: { project: Project; freed_bytes: number } };

  // ── video by VOD range (protocol 3) ──
  /** Coverage (have / fetching / queued / failed ranges) + requests of a project; without
   *  id: every loaded project + the app-wide pause. Also pushed as video.changed. */
  "video.status": { params: { id: string }; result: VideoState };
  /** Queue the video of VOD ranges and/or songs (± margins); only missing segments are
   *  fetched. request_id null = nothing was missing. Errors: VOD_GONE, NO_SPACE, PRECONDITION. */
  "video.fetch": { params: { id: string } & Partial<FetchArgs>; result: FetchPlan };
  /** Same as video.fetch without queuing: missing seconds and estimated bytes (tooltips). */
  "video.estimate": { params: { id: string } & Partial<FetchArgs>; result: FetchPlan };
  /** Cancel one request (or all of the project's); fetched video stays. */
  "video.cancel": { params: { id: string; request_id?: string | null }; result: { cancelled: number } };
  /** Move a request: "now" = fetch it first. */
  "video.prioritize": { params: { id: string; request_id: string; priority: Priority }; result: true };
  /** Retry failed segments (in ranges, or all). */
  "video.retry": { params: { id: string; ranges?: [number, number][] | null }; result: FetchPlan | null };
  /** Pause / resume all video downloads of the app (persisted). */
  "video.pause": { params: { paused: boolean }; result: true };
  /** A playable MP4 with the picture at VOD time t (built on demand, a few s for long
   *  ranges): video.currentTime = vod − origin, valid in [start, end]. VIDEO_MISSING if none. */
  "video.view": { params: { id: string; t: number; end?: number | null }; result: VideoView };
  /** Separate (and suggest mutes for) the parts of songs ± margins / ranges that have video
   *  and aren't separated yet; default every enabled song. */
  "stems.ensure": { params: { id: string; song_ids?: string[] | null; ranges?: [number, number][] | null }; result: JobHandle };
  /** Bytes by kind for one project, or { projects, free, root } for all. */
  "storage.usage": { params: { id: string }; result: StorageUsage };  /** Cleanup: unused < stems < video < audio. Decisions always stay. PINNED unless force. */
  "media.drop": { params: { id: string; level: DropLevel; force?: boolean }; result: { project: Project; freed_bytes: number } };
  "project.set_pinned": { params: { id: string; pinned: boolean }; result: Project };
  /** Open a project folder that lives elsewhere; it joins project.list and is remembered.
   *  CONFLICT (data.dir) if a project with the same id is already listed from another place. */
  "project.open": { params: { path: string }; result: Project };
  /** Also list every project in another folder (e.g. the previous projects folder). */
  "projects.add_root": { params: { path: string }; result: { projects: ProjectSummary[]; skipped: { id: string; dir: string }[] } };
  /** Stop listing a root or project folder added above (nothing is deleted). */
  "projects.remove_location": { params: { path: string }; result: boolean };
  "projects.locations": { params: NoParams; result: { root: string; roots: string[]; projects: string[] } };
  /** Is the replay still online? (call in the background when a project opens) */
  "vod.check": { params: { id: string }; result: VodStatus };
  /** Network counters (the app-wide cap of 4 in-flight requests). */
  "system.net_stats": { params: { reset?: boolean }; result: NetStats };
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
  /** Protocol 2 pipeline on the segment store: fetch songs ± margins (default every enabled
   *  song), wait, then stems.ensure. clip_ids / exact are ignored. */
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
  "clips.fetch": { clips: string[]; video_failed: [number, number][]; stems: EnsureResult };
  "stems.ensure": EnsureResult;
  "export.run": {
    exports: { song_id: string; result: ExportResult }[];
    stems: { song_id: string; stems: StemsResult }[];
    /** songs that couldn't be exported (e.g. VIDEO_MISSING) while others were */
    failed: { song_id: string; error: ApiError }[];
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
  /** A project's video coverage / requests changed (throttled, at most 4/s). */
  "video.changed": Variant<"video_changed">;
  /** A video.fetch request ended: status "done" | "failed" | "cancelled". */
  "video.request_done": Variant<"video_request_done">;
}

export type NotificationMethod = keyof Notifications;

/** JSON-RPC error object; `data.code` carries the engine's ApiError code. */
export interface RpcError {
  code: number;
  message: string;
  /** `code` = the engine code; details next to it (e.g. `ranges` for VIDEO_MISSING) */
  data?: { code: ApiError["code"]; [detail: string]: unknown };
}
