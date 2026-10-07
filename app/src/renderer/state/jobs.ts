// Jobs: engine jobs (job.* notifications) plus an app-side wait list for BUSY
// ("앞 작업이 끝나면 시작해요"), ETA, stall detection, per-song progress, completion
// notifications, and persistence of in-flight requests for "이어서 하기".
import { create } from "zustand";
import type { Method, ParamsOf } from "@ipc/protocol";
import type { Progress } from "@ipc/generated/Progress";
import type { Note } from "@ipc/generated/Note";
import type { QueueEntry } from "@ipc/generated/QueueEntry";
import type { ApiError } from "@ipc/generated/ApiError";
import { call, EngineError, onNotification } from "@/lib/rpc";
import { friendly, type FriendlyError } from "@/lib/errors";
import { getProject } from "./projects";
import { useApp } from "./app";
import { projectTitle } from "@/lib/song";

export type JobKind = "audio.fetch" | "detect.run" | "clips.fetch" | "export.run";
type JobMethod = Extract<Method, JobKind>;

export const KIND_LABEL: Record<string, string> = {
  "audio.fetch": "소리 받기",
  "detect.run": "곡 찾기",
  "clips.fetch": "영상 받기 · 목소리 나누기",
  "export.run": "내보내기",
};

export const STAGE_LABEL: Record<string, string> = {
  "audio.fetch": "소리 받는 중",
  "detect.tag": "노래를 찾는 중",
  "video.fetch": "영상 받는 중",
  "video.remux": "영상 정리 중",
  separate: "목소리와 반주 나누는 중",
  talk: "멘트 찾는 중",
  "clips.fetch": "준비 중",
  "export.run": "내보내기 준비 중",
  "export.render": "소리 만드는 중",
  "export.encode": "영상 만드는 중",
  "export.stems": "편집기용 파일 만드는 중",
};

/** Progress notes by code (docs/ipc.md); unknown codes are ignored. */
export function noteKo(note?: Note | null): string | undefined {
  if (!note) return undefined;
  const args = note.args ?? {};
  switch (note.code) {
    case "copy_audio_track":
      return "녹화 파일에서 소리를 꺼내는 중";
    case "cut_recording":
      return "녹화 파일에서 곡 부분을 자르는 중";
    case "find_talk":
      return "목소리에서 멘트를 찾는 중";
    case "fallback_segments":
      return `일부 구간(${args.count ?? "몇"}개)은 다른 화질의 소리로 채웠어요`;
    case "variant_truncated":
      return `${args.resolution ?? "고른"} 화질 영상이 서버에서 깨져 있어 다른 화질로 받았어요`;
    case "remux":
      return "영상 정리 중";
    case "render_audio":
      return "소리 만드는 중";
    case "measure_loudness":
      return "소리 크기 재는 중";
    case "write_stems":
      return "편집기용 WAV 쓰는 중";
    case "write_reference_video":
      return "참조용 영상 만드는 중";
    case "decode":
      return "소리 푸는 중";
    case "alignment_weak":
      return "영상과 소리의 시간 맞춤이 불확실해요. 이 곡은 시작·끝을 한 번 확인해 주세요";
    case "silent_segments":
      return `받지 못한 구간 ${args.count ?? "일부"}개는 무음으로 채웠어요`;
    default:
      return undefined;
  }
}

const CLIP_STAGE_WEIGHT: Record<string, [number, number]> = {
  // [offset, weight] inside one clip
  "clips.fetch": [0, 0.02],
  "video.fetch": [0.02, 0.45],
  "video.remux": [0.47, 0.05],
  separate: [0.52, 0.4],
  talk: [0.92, 0.08],
};
const EXPORT_STAGE_WEIGHT: Record<string, [number, number]> = {
  "export.run": [0, 0.02],
  "export.render": [0.02, 0.18],
  "export.encode": [0.2, 0.8],
  "export.stems": [0.2, 0.8],
};

export interface Job {
  id: string;
  kind: string;
  projectId: string;
  status: "running" | "done" | "failed" | "cancelled";
  stage: string;
  done: number;
  total: number;
  message?: string;
  item?: string;
  note?: Note;
  /** audio.fetch: overview.peaks covers [0, peaksUntil) */
  peaksUntil?: number;
  /** items (clip ids / song ids) of this job in processing order (job.started) */
  items: string[];
  itemsDone: string[];
  fraction: number | null;
  startedAt: number;
  lastChangeAt: number;
  finishedAt?: number;
  eta: number | null;
  error?: FriendlyError;
  request?: { method: JobMethod; params: unknown; label?: string };
  result?: unknown;
  fromQueue?: boolean;
  samples: [number, number][];
}

export interface Waiting {
  key: string;
  method: JobMethod;
  params: unknown;
  label?: string;
  projectId: string;
  addedAt: number;
}

export interface InterruptedJob {
  jobId: string;
  method: JobMethod;
  params: unknown;
  label?: string;
  projectId: string;
}

interface JobsStore {
  jobs: Record<string, Job>;
  order: string[];
  waiting: Waiting[];
  queue: QueueEntry[];
  /** jobs that were running when the app last closed (지난번에 멈춘 작업) */
  interrupted: InterruptedJob[];
}

export const useJobs = create<JobsStore>(() => ({ jobs: {}, order: [], waiting: [], queue: [], interrupted: [] }));

type Then = (result: unknown) => void;
const thens = new Map<string, Then>();
const pendingThen = new Map<string, Then>(); // waiting key → then

function upsert(id: string, patch: Partial<Job> & { kind?: string; projectId?: string }) {
  useJobs.setState((s) => {
    const cur = s.jobs[id];
    const now = Date.now();
    const base: Job = cur ?? {
      id,
      kind: patch.kind ?? "",
      projectId: patch.projectId ?? "",
      status: "running",
      stage: "",
      done: 0,
      total: 0,
      items: [],
      itemsDone: [],
      fraction: null,
      startedAt: now,
      lastChangeAt: now,
      eta: null,
      samples: [],
    };
    const order = cur ? s.order : [id, ...s.order].slice(0, 200);
    return { jobs: { ...s.jobs, [id]: { ...base, ...patch } }, order };
  });
  syncBusy();
}

function computeFraction(j: Job, pr: Progress): number | null {
  const stageFrac = pr.total > 0 ? Math.min(1, pr.done / pr.total) : null;
  if (j.kind === "audio.fetch" || j.kind === "detect.run") return stageFrac;
  const table = j.kind === "clips.fetch" ? CLIP_STAGE_WEIGHT : j.kind === "export.run" ? EXPORT_STAGE_WEIGHT : null;
  const n = Math.max(1, pr.items_total ?? j.items.length);
  if (!table) return stageFrac;
  const [off, w] = table[pr.stage] ?? [0, 0];
  const inItem = off + w * (stageFrac ?? 0);
  return Math.min(0.999, ((pr.item_index ?? j.itemsDone.length) + inItem) / n);
}

const lastFlush = new Map<string, number>();
const queuedProgress = new Map<string, [string, string, Progress]>();
let flushTimer: ReturnType<typeof setTimeout> | null = null;

/** Store updates at most ~4 Hz per job, so subscribers don't re-render on every engine tick. */
function onProgress(jobId: string, kind: string, projectId: string, pr: Progress) {
  const j = useJobs.getState().jobs[jobId];
  const now = Date.now();
  const important = !j || pr.stage !== j.stage || (pr.item ?? j.item) !== j.item || pr.note?.code !== j.note?.code;
  if (!important && now - (lastFlush.get(jobId) ?? 0) < 250) {
    queuedProgress.set(jobId, [kind, projectId, pr]);
    if (!flushTimer)
      flushTimer = setTimeout(() => {
        flushTimer = null;
        const all = [...queuedProgress.entries()];
        queuedProgress.clear();
        for (const [id, [k, pid, p]] of all) {
          lastFlush.set(id, Date.now());
          applyProgress(id, k, pid, p);
        }
      }, 250);
    return;
  }
  queuedProgress.delete(jobId);
  lastFlush.set(jobId, now);
  applyProgress(jobId, kind, projectId, pr);
}

function applyProgress(jobId: string, kind: string, projectId: string, pr: Progress) {
  const s = useJobs.getState();
  let j = s.jobs[jobId];
  if (!j) {
    upsert(jobId, { kind, projectId });
    j = useJobs.getState().jobs[jobId];
  }
  const now = Date.now();
  let items = j.items;
  if (pr.item && !items.includes(pr.item)) items = [...items, pr.item];
  // items before item_index are finished (engine order = job.started items)
  let itemsDone = j.itemsDone;
  if (typeof pr.item_index === "number") itemsDone = pr.item_index > 0 ? items.slice(0, pr.item_index) : [];
  else if (pr.item && j.item && pr.item !== j.item && !itemsDone.includes(j.item)) itemsDone = [...itemsDone, j.item];
  const fraction = computeFraction({ ...j, items, itemsDone }, pr);
  const changed = fraction !== j.fraction || pr.stage !== j.stage || pr.done !== j.done || pr.note?.code !== j.note?.code;
  const samples = fraction !== null ? [...j.samples.filter(([t]) => now - t < 20000), [now, fraction] as [number, number]] : j.samples;
  let eta: number | null = j.eta;
  const singleStage = kind === "audio.fetch" || kind === "detect.run" || (pr.items_total ?? items.length) <= 1;
  if (singleStage && typeof pr.eta_sec === "number" && kind !== "clips.fetch" && kind !== "export.run") {
    // the engine's estimate covers the current stage = the whole job here
    eta = pr.eta_sec;
  } else if (fraction !== null && now - j.startedAt > 5000 && samples.length > 2) {
    const [t0, f0] = samples[0];
    const rate = (fraction - f0) / ((now - t0) / 1000);
    if (rate > 1e-5) {
      const raw = (1 - fraction) / rate;
      eta = j.eta === null ? raw : j.eta * 0.8 + raw * 0.2;
    }
  }
  upsert(jobId, {
    stage: pr.stage,
    done: pr.done,
    total: pr.total,
    message: pr.message,
    note: pr.note ?? (pr.stage !== j.stage ? undefined : j.note),
    peaksUntil: pr.peaks_until ?? j.peaksUntil,
    item: pr.item ?? j.item,
    items,
    itemsDone,
    fraction,
    samples,
    eta,
    lastChangeAt: changed ? now : j.lastChangeAt,
  });
}

async function notifyDone(j: Job, ok: boolean) {
  const app = useApp.getState();
  if (!app.settings.notifications) return;
  if (await window.cliper.isFocused()) return;
  const p = getProject(j.projectId);
  const title = projectTitle(p) || "Cliper";
  const label = KIND_LABEL[j.kind] ?? j.kind;
  let body = ok ? `${label} 완료` : `${label} 실패 · ${j.error?.title ?? ""}`;
  if (ok && j.kind === "clips.fetch" && p) {
    const pending = p.songs.reduce((n, s) => n + s.mutes.filter((m) => !m.confirmed).length, 0);
    body = `영상 ${j.items.length}개 받기 완료${pending ? ` · 멘트 확인 ${pending}개 대기` : ""}`;
  }
  if (ok && j.kind === "detect.run" && p) body = `곡 ${p.songs.filter((s) => s.enabled).length}개를 찾았어요`;
  if (ok && j.kind === "export.run") body = `${j.items.length}곡 내보내기 완료`;
  void window.cliper.notify(title.length > 60 ? title.slice(0, 58) + "…" : title, body);
}

export function wireJobs() {
  onNotification("job.started", (p) => {
    upsert(p.job_id, { kind: p.kind, projectId: p.project_id, status: "running", items: p.items ?? [] });
  });
  onNotification("job.progress", (p) => onProgress(p.job_id, p.kind, p.project_id, p.progress));
  onNotification("job.done", (p) => {
    queuedProgress.delete(p.job_id);
    const j = useJobs.getState().jobs[p.job_id];
    upsert(p.job_id, {
      kind: p.kind,
      projectId: p.project_id,
      status: "done",
      fraction: 1,
      eta: 0,
      finishedAt: Date.now(),
      result: p.result,
      itemsDone: j ? [...new Set([...j.itemsDone, ...(j.item ? [j.item] : [])])] : [],
    });
    const done = useJobs.getState().jobs[p.job_id];
    void forgetActive(p.job_id);
    void notifyDone(done, true);
    const t = thens.get(p.job_id);
    thens.delete(p.job_id);
    if (t) t(p.result);
    dispatchWaiting(p.project_id);
  });
  onNotification("job.failed", (p) => {
    queuedProgress.delete(p.job_id);
    const cancelled = p.error.code === "CANCELLED";
    upsert(p.job_id, { kind: p.kind, projectId: p.project_id, status: cancelled ? "cancelled" : "failed", finishedAt: Date.now(), error: friendly(p.error as ApiError) });
    void forgetActive(p.job_id);
    thens.delete(p.job_id);
    if (!cancelled) void notifyDone(useJobs.getState().jobs[p.job_id], false);
    dispatchWaiting(p.project_id);
  });
  onNotification("queue.changed", (p) => useJobs.setState({ queue: p.queue }));
}

/** The engine went away (crash, restart, root or device change): its running jobs are gone. */
export function markEngineLost() {
  const s = useJobs.getState();
  for (const j of Object.values(s.jobs)) {
    if (j.status === "running") {
      thens.delete(j.id);
      upsert(j.id, { status: "failed", finishedAt: Date.now(), error: { title: "처리 엔진이 다시 시작되어 작업이 멈췄어요.", hint: "끝난 곡은 남아 있어요. [다시 시도]로 이어서 할 수 있어요.", action: "retry", code: "ENGINE_DOWN" } });
    }
  }
}

/** After the engine is ready again: drop store jobs it doesn't know, start what was waiting. */
export async function reconcileJobs() {
  let live: Set<string>;
  try {
    live = new Set((await call("job.list", {})).map((j) => j.job_id));
  } catch {
    return;
  }
  for (const j of Object.values(useJobs.getState().jobs)) {
    if (j.status === "running" && !live.has(j.id)) {
      thens.delete(j.id);
      upsert(j.id, { status: "failed", finishedAt: Date.now(), error: { title: "처리 엔진이 다시 시작되어 작업이 멈췄어요.", hint: "끝난 곡은 남아 있어요. [다시 시도]로 이어서 할 수 있어요.", action: "retry", code: "ENGINE_DOWN" } });
    }
  }
  // waiting entries were blocked by jobs that no longer exist
  for (const pid of new Set(useJobs.getState().waiting.map((w) => w.projectId))) dispatchWaiting(pid);
}

let lastBusy: boolean | null = null;
function syncBusy() {
  const s = useJobs.getState();
  const busy = Object.values(s.jobs).some((j) => j.status === "running") || s.waiting.length > 0 || s.queue.some((q) => q.status === "running" || q.status === "waiting");
  if (busy === lastBusy) return;
  lastBusy = busy;
  window.cliper.setBusy(busy);
}

function waitKey(projectId: string, kind: string) {
  return `${projectId}|${kind}`;
}

/**
 * Start waiting requests of a project whose kind is not running any more. The engine also
 * blocks some kinds against each other (e.g. detect.run / clips.fetch while audio.fetch runs),
 * so this runs whenever any job of the project ends; a request that is still blocked simply
 * comes back BUSY and waits again.
 */
function dispatchWaiting(projectId: string) {
  const s = useJobs.getState();
  const started = new Set<string>();
  const keep: Waiting[] = [];
  const go: Waiting[] = [];
  for (const w of s.waiting) {
    if (w.projectId !== projectId || started.has(w.method) || runningJobs(s, projectId, w.method).length) keep.push(w);
    else {
      started.add(w.method);
      go.push(w);
    }
  }
  if (!go.length) return;
  useJobs.setState({ waiting: keep });
  for (const w of go) {
    const then = pendingThen.get(`${w.key}|${w.addedAt}`);
    pendingThen.delete(`${w.key}|${w.addedAt}`);
    setTimeout(() => void startJob(w.method, w.params as ParamsOf<typeof w.method>, { label: w.label, then }), 200);
  }
}

export function cancelWaiting(w: Waiting) {
  useJobs.setState((s) => ({ waiting: s.waiting.filter((x) => x !== w) }));
  syncBusy();
}

// ── persistence of in-flight requests ("지난번에 멈춘 작업") ──
type ActiveRecord = InterruptedJob;
let active: ActiveRecord[] = [];
async function rememberActive(r: ActiveRecord) {
  active = [...active.filter((a) => a.jobId !== r.jobId), r];
  await window.cliper.setState("activeJobs", active);
}
async function forgetActive(jobId: string) {
  if (!active.some((a) => a.jobId === jobId)) return;
  active = active.filter((a) => a.jobId !== jobId);
  await window.cliper.setState("activeJobs", active);
}
/** Jobs that were running when the app last closed. Called once in boot(); Home reads the store. */
export async function takeInterrupted(): Promise<void> {
  const left = ((await window.cliper.getState<ActiveRecord[]>("activeJobs")) ?? []).filter((r) => r && r.method);
  active = [];
  await window.cliper.setState("activeJobs", []);
  useJobs.setState({ interrupted: left });
}

export function clearInterrupted() {
  useJobs.setState({ interrupted: [] });
}

/**
 * Start an engine job. BUSY → waits in the app-side list and starts when the running
 * job of the same kind finishes. Returns the job id, or null when queued.
 */
export async function startJob<M extends JobMethod>(method: M, params: ParamsOf<M>, opts: { label?: string; then?: Then } = {}): Promise<string | null> {
  const projectId = (params as { id: string }).id;
  try {
    const { job_id } = (await call(method, params)) as { job_id: string };
    upsert(job_id, {
      kind: method,
      projectId,
      request: { method, params, label: opts.label },
    });
    if (opts.then) {
      // the job may already be done (very fast jobs)
      const j = useJobs.getState().jobs[job_id];
      if (j?.status === "done") opts.then(j.result);
      else thens.set(job_id, opts.then);
    }
    // a very fast job may already be finished: then there is nothing to resume later
    if (useJobs.getState().jobs[job_id]?.status === "running" || !useJobs.getState().jobs[job_id]) void rememberActive({ jobId: job_id, method, params, label: opts.label, projectId });
    return job_id;
  } catch (e) {
    if (e instanceof EngineError && e.engineCode === "BUSY") {
      const w: Waiting = { key: waitKey(projectId, method), method, params, label: opts.label, projectId, addedAt: Date.now() };
      if (opts.then) pendingThen.set(`${w.key}|${w.addedAt}`, opts.then);
      useJobs.setState((s) => ({ waiting: [...s.waiting, w] }));
      syncBusy();
      // the blocking job may have finished before the BUSY answer arrived
      if (runningJobs(useJobs.getState(), projectId).length === 0) setTimeout(() => dispatchWaiting(projectId), 300);
      return null;
    }
    throw e;
  }
}

export async function cancelJob(jobId: string) {
  try {
    await call("job.cancel", { job_id: jobId });
  } catch {
    /* already finished */
  }
}

export async function retryJob(j: Job) {
  if (!j.request) return;
  upsert(j.id, { status: "cancelled" }); // hide the failed row's retry once retried
  useJobs.setState((s) => ({ order: s.order.filter((x) => x !== j.id), jobs: Object.fromEntries(Object.entries(s.jobs).filter(([k]) => k !== j.id)) }));
  await startJob(j.request.method, j.request.params as never, { label: j.request.label });
}

export function dismissJob(id: string) {
  useJobs.setState((s) => ({ order: s.order.filter((x) => x !== id), jobs: Object.fromEntries(Object.entries(s.jobs).filter(([k]) => k !== id)) }));
}

export function clearFinished() {
  useJobs.setState((s) => {
    const keep = Object.values(s.jobs).filter((j) => j.status === "running");
    return { jobs: Object.fromEntries(keep.map((j) => [j.id, j])), order: s.order.filter((id) => keep.some((j) => j.id === id)) };
  });
}

export function runningJobs(s: JobsStore, projectId?: string, kind?: string): Job[] {
  return s.order.map((id) => s.jobs[id]).filter((j) => j && j.status === "running" && (!projectId || j.projectId === projectId) && (!kind || j.kind === kind));
}

/** Per-song progress for list rows: songId → {label, fraction|null}. */
export function songProgress(s: JobsStore, projectId: string): Map<string, { label: string; fraction: number | null; waiting?: boolean }> {
  const out = new Map<string, { label: string; fraction: number | null; waiting?: boolean }>();
  const p = getProject(projectId);
  if (!p) return out;
  for (const j of runningJobs(s, projectId)) {
    if (j.kind === "clips.fetch") {
      for (const clipId of j.items) {
        const ids = p.clips.find((c) => c.id === clipId)?.song_ids ?? [];
        const songs = p.songs.filter((x) => ids.includes(x.id) || x.clip_id === clipId || x.id === clipId);
        const isCur = j.item === clipId;
        const isDone = j.itemsDone.includes(clipId);
        if (isDone) continue;
        const stageFrac = j.total > 0 ? j.done / j.total : null;
        for (const song of songs) {
          out.set(song.id, isCur ? { label: STAGE_LABEL[j.stage] ?? "처리 중", fraction: j.stage === "video.fetch" || j.stage === "separate" ? stageFrac : null } : { label: "대기", fraction: null, waiting: true });
        }
      }
    } else if (j.kind === "export.run") {
      for (const songId of j.items) {
        if (j.itemsDone.includes(songId)) continue;
        out.set(songId, j.item === songId ? { label: "내보내는 중", fraction: j.total > 0 ? j.done / j.total : null } : { label: "내보내기 대기", fraction: null, waiting: true });
      }
    }
  }
  for (const w of s.waiting.filter((x) => x.projectId === projectId)) {
    const ids = (w.params as { song_ids?: string[] }).song_ids ?? [];
    for (const id of ids) if (!out.has(id)) out.set(id, { label: "앞 작업 대기", fraction: null, waiting: true });
  }
  return out;
}
