// Video coverage on the VOD timeline (기획서 14장): what is where, per-song state, stems
// coverage, and the app-side fetch actions (`video.fetch` with priorities and reasons).
import type { VideoState } from "@ipc/generated/VideoState";
import type { FetchPlan } from "@ipc/generated/FetchPlan";
import type { Priority } from "@ipc/generated/Priority";
import type { StorageUsage } from "@ipc/generated/StorageUsage";
import type { StorageAll } from "@ipc/protocol";
import { call, EngineError } from "./rpc";
import { fmtBytes, fmtDurationKo } from "./format";
import { openSettings, toast } from "@/state/app";

export * from "./coverage";
import type { Range } from "./coverage";

// ── actions ──

export interface FetchOpts {
  song_ids?: string[];
  ranges?: Range[];
  priority?: Priority;
  reason?: string;
  process?: boolean;
  /** no toast on success */
  quiet?: boolean;
  /** internal: VIDEO_BUSY retries so far */
  busyTries?: number;
}

/** Toast for the errors a fetch can hit (NO_SPACE, VOD_GONE, PRECONDITION). */
export function fetchErrorToast(e: unknown) {
  if (!(e instanceof EngineError)) {
    toast("영상을 받지 못했어요. 잠시 뒤 다시 해 주세요.", { kind: "error" });
    return;
  }
  if (e.engineCode === "NO_SPACE") {
    const need = Number(e.data?.need ?? 0);
    const free = Number(e.data?.free ?? 0);
    toast(`저장 공간이 부족해요${need ? ` (필요 약 ${fmtBytes(need)}, 남은 공간 ${fmtBytes(free)})` : ""}.`, { kind: "error", ttl: 10000, action: { label: "정리하기", run: () => openSettings("storage") } });
  } else if (e.engineCode === "VOD_GONE") {
    toast("다시보기를 더 이상 열 수 없어서 영상을 받을 수 없어요. 받아 둔 영상 안에서는 계속 작업할 수 있어요.", { kind: "warn", ttl: 8000 });
  } else if (e.engineCode === "PRECONDITION") {
    toast("먼저 소리를 받아야 영상을 받을 수 있어요.", { kind: "warn" });
  } else if (e.engineCode === "AUTH_REQUIRED") {
    toast("이 다시보기는 로그인이 필요해요. 설정 > 고급에서 로그인 쿠키를 넣어 주세요.", { kind: "warn", action: { label: "쿠키 설정", run: () => openSettings("advanced") } });
  } else {
    toast(`영상을 받지 못했어요 · ${e.message}`, { kind: "error" });
  }
}

/** video.fetch with the app's error handling. Returns the plan, or null on error. */
export async function fetchVideo(pid: string, o: FetchOpts): Promise<FetchPlan | null> {
  try {
    const plan = await call("video.fetch", {
      id: pid,
      song_ids: o.song_ids ?? null,
      ranges: o.ranges ?? null,
      priority: o.priority ?? "user",
      reason: o.reason ?? "user",
      process: !!o.process,
    });
    if (!o.quiet) {
      if (plan.request_id === null) toast("이 범위의 영상은 이미 있어요.", { kind: "info", ttl: 2500 });
      else toast(`영상 ${fmtDurationKo(plan.missing_sec)} 분량을 받아요 · 약 ${fmtBytes(plan.estimated_bytes)}`, { kind: "info", ttl: 3000 });
    }
    return plan;
  } catch (e) {
    if (e instanceof EngineError && e.engineCode === "VIDEO_BUSY" && (o.busyTries ?? 0) < 40) {
      // the project's video is being cleaned up: try again when it is done (protocol 4)
      if (!o.busyTries) toast("정리하는 중이에요. 끝나면 영상을 받을게요.", { kind: "info", ttl: 4000 });
      await new Promise((r) => setTimeout(r, 3000));
      return fetchVideo(pid, { ...o, busyTries: (o.busyTries ?? 0) + 1, quiet: true });
    }
    fetchErrorToast(e);
    return null;
  }
}

export async function estimate(pid: string, o: { song_ids?: string[]; ranges?: Range[] }): Promise<FetchPlan | null> {
  try {
    return await call("video.estimate", { id: pid, song_ids: o.song_ids ?? null, ranges: o.ranges ?? null });
  } catch {
    return null;
  }
}

/** Requests that are fetching around t (for [먼저 받기]). */
export function requestsAt(vs: VideoState | undefined, t: number) {
  return (vs?.requests ?? []).filter((r) => r.remaining_sec > 0 && r.ranges.some(([a, b]) => t >= a - 1 && t <= b + 1));
}

/** [먼저 받기]: move the requests covering t (or those of a song) to the front. */
export async function prioritizeAt(pid: string, vs: VideoState | undefined, t: number) {
  for (const r of requestsAt(vs, t)) if (r.priority !== "now") await call("video.prioritize", { id: pid, request_id: r.id, priority: "now" }).catch(() => {});
}

export async function prioritizeSong(pid: string, vs: VideoState | undefined, songId: string) {
  for (const r of vs?.requests ?? []) {
    if (r.remaining_sec > 0 && r.priority !== "now" && r.song_ids.includes(songId)) await call("video.prioritize", { id: pid, request_id: r.id, priority: "now" }).catch(() => {});
  }
}

/** `storage.usage {}` — every project + free bytes on the projects drive. */
export async function storageAll(): Promise<StorageAll | null> {
  const r = await window.cliper.call("storage.usage", {});
  return r.ok ? (r.result as StorageAll) : null;
}

export type { StorageUsage };

/** "영상 1시간 12분 / 4:02:11 · 4.1GB" */
export function coverageSummary(vs: VideoState | undefined): string {
  if (!vs) return "";
  if (vs.local) return "녹화 파일 전체";
  if (vs.have_sec <= 0) return "받은 영상 없음";
  return `영상 ${fmtDurationKo(vs.have_sec)} · ${fmtBytes(vs.bytes)}`;
}

/** [여기 영상 받기] (V): the playhead ± 1 min, first in line (Q-S4). No song is made. */
export async function fetchHere(pid: string, t: number, duration: number): Promise<FetchPlan | null> {
  const a = Math.max(0, t - 60);
  const b = duration > 0 ? Math.min(duration, t + 60) : t + 60;
  return fetchVideo(pid, { ranges: [[a, b]], priority: "now", reason: "here" });
}
