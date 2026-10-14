// Video coverage on the VOD timeline (기획서 14장): what is where, per-song state, stems
// coverage, and the app-side fetch actions (`video.fetch` with priorities and reasons).
import type { Project } from "@ipc/generated/Project";
import type { Song } from "@ipc/generated/Song";
import type { StemSpan } from "@ipc/generated/StemSpan";
import type { VideoState } from "@ipc/generated/VideoState";
import type { CoverageRange } from "@ipc/generated/CoverageRange";
import type { FetchPlan } from "@ipc/generated/FetchPlan";
import type { Priority } from "@ipc/generated/Priority";
import type { StorageUsage } from "@ipc/generated/StorageUsage";
import type { StorageAll } from "@ipc/protocol";
import { call, EngineError } from "./rpc";
import { fmtBytes, fmtDurationKo } from "./format";
import { openSettings, toast } from "@/state/app";

export type Range = [number, number];
/** a point / stretch of the VOD: the engine states plus none (no video) and gone (can't fetch) */
export type Cov = "have" | "fetching" | "queued" | "failed" | "none" | "gone";

/** Margin ("앞뒤 여유") of a project: both sides use the same value (Q-S3). */
export function marginOf(p: Project): number {
  return Math.max(p.settings.margin_before, p.settings.margin_after);
}

export function vodGone(p: Project | undefined): boolean {
  return p?.vod?.status === "gone";
}

export function len(rs: Range[]): number {
  return rs.reduce((n, [a, b]) => n + Math.max(0, b - a), 0);
}

/** State at VOD time t. */
export function covAt(vs: VideoState | undefined, t: number, gone = false): { state: Cov; range: CoverageRange | null } {
  if (vs?.local) return { state: "have", range: null };
  for (const r of vs?.ranges ?? []) {
    if (t >= r.start && t < r.end) return { state: r.state, range: r };
    if (r.start > t) break;
  }
  return { state: gone ? "gone" : "none", range: null };
}

/** [a, b] cut into pieces by state (gaps = none / gone). */
export function piecesIn(vs: VideoState | undefined, a: number, b: number, gone = false): { a: number; b: number; state: Cov; code?: string }[] {
  if (b <= a) return [];
  if (vs?.local) return [{ a, b, state: "have" }];
  const out: { a: number; b: number; state: Cov; code?: string }[] = [];
  let t = a;
  for (const r of vs?.ranges ?? []) {
    if (r.end <= t) continue;
    if (r.start >= b) break;
    if (r.start > t) out.push({ a: t, b: r.start, state: gone ? "gone" : "none" });
    const s = Math.max(t, r.start);
    const e = Math.min(b, r.end);
    if (e > s) out.push({ a: s, b: e, state: r.state, code: r.code });
    t = e;
  }
  if (t < b) out.push({ a: t, b, state: gone ? "gone" : "none" });
  return out;
}

/** Parts of [a, b] in the given states (default: not on disk yet). */
export function partsIn(vs: VideoState | undefined, a: number, b: number, states: Cov[] = ["none", "gone", "failed", "queued", "fetching"]): Range[] {
  const out: Range[] = [];
  for (const p of piecesIn(vs, a, b)) {
    if (!states.includes(p.state)) continue;
    const last = out[out.length - 1];
    if (last && Math.abs(last[1] - p.a) < 1e-6) last[1] = p.b;
    else out.push([p.a, p.b]);
  }
  return out.filter(([x, y]) => y - x > 0.05);
}

/** Has the project any video around this range (have / coming)? → auto top-up applies (14.4). */
export function hasVideoNear(vs: VideoState | undefined, a: number, b: number): boolean {
  if (vs?.local) return true;
  return (vs?.ranges ?? []).some((r) => r.end > a && r.start < b && r.state !== "failed");
}

export type SongVideoState = "none" | "fetching" | "partial" | "ready" | "gone" | "failed";

export interface SongVideo {
  state: SongVideoState;
  /** parts of the song range [start, end] without video on disk */
  missing: Range[];
  missingSec: number;
  /** share of the song range on disk */
  fraction: number;
  /** some of the song range is fetching / queued */
  pending: boolean;
  /** the song has any video at all (have / coming): boundary edits top it up */
  touched: boolean;
  /** row text (14.3), "" when nothing to say */
  label: string;
}

/** Song status against the "have" ranges; the margins never warn (14.3, PM 3-2). */
export function songVideo(p: Project, vs: VideoState | undefined, s: Song): SongVideo {
  const len0 = Math.max(0.01, s.end - s.start);
  const pieces = piecesIn(vs, s.start, s.end, vodGone(p));
  const missing = partsIn(vs, s.start, s.end);
  const missingSec = len(missing);
  const fraction = Math.max(0, Math.min(1, 1 - missingSec / len0));
  const pending = pieces.some((x) => x.state === "fetching" || x.state === "queued");
  const failed = pieces.some((x) => x.state === "failed");
  const m = marginOf(p);
  const touched = hasVideoNear(vs, s.start - m, s.end + m);
  const where = (): string => {
    if (missing.length === 1) {
      const [a, b] = missing[0];
      if (a <= s.start + 0.05 && b < s.end - 0.05) return `앞 ${fmtDurationKo(b - a)}`;
      if (b >= s.end - 0.05 && a > s.start + 0.05) return `뒤 ${fmtDurationKo(b - a)}`;
    }
    return `${fmtDurationKo(missingSec)}`;
  };
  if (missingSec < 0.05) return { state: "ready", missing: [], missingSec: 0, fraction: 1, pending: false, touched: true, label: "영상 ✓" };
  if (pending) {
    const partial = fraction > 0.01 || missing.length > 1 || (missing.length === 1 && (missing[0][0] > s.start + 0.05 || missing[0][1] < s.end - 0.05));
    return { state: "fetching", missing, missingSec, fraction, pending, touched, label: partial && fraction > 0.5 ? `${where()} 받는 중` : `영상 받는 중 ${Math.floor(fraction * 100)}%` };
  }
  if (vodGone(p)) return { state: "gone", missing, missingSec, fraction, pending, touched, label: fraction > 0 ? `${where()} 영상 없음 · 다시보기 없음` : "다시보기 없음" };
  if (failed) return { state: "failed", missing, missingSec, fraction, pending, touched, label: "영상 일부를 받지 못했어요" };
  if (fraction > 0.001) return { state: "partial", missing, missingSec, fraction, pending, touched, label: `${where()} 영상 없음` };
  return { state: "none", missing, missingSec, fraction, pending, touched, label: "" };
}

// ── stems (separated audio) ──

/** The stem span to play for a song: the one overlapping it most. */
export function spanFor(p: Project, a: number, b: number): StemSpan | null {
  let best: StemSpan | null = null;
  let bestO = 0;
  for (const sp of p.stems ?? []) {
    const o = Math.min(b, sp.end) - Math.max(a, sp.start);
    if (o > bestO) {
      bestO = o;
      best = sp;
    }
  }
  return best;
}

/** Parts of [a, b] that have video but aren't separated yet (stems.ensure would do them). */
export function unseparated(p: Project, vs: VideoState | undefined, a: number, b: number): Range[] {
  const have = partsIn(vs, a, b, ["have"]);
  const out: Range[] = [];
  for (const [x, y] of have) {
    let t = x;
    const spans = (p.stems ?? []).filter((sp) => sp.end > x && sp.start < y).sort((u, v) => u.start - v.start);
    for (const sp of spans) {
      if (sp.start > t + 0.5) out.push([t, Math.min(y, sp.start)]);
      t = Math.max(t, sp.end);
    }
    if (y > t + 0.5) out.push([t, y]);
  }
  return out.filter(([x, y]) => y - x > 0.5);
}

/** Is any of [a, b] separated? */
export function separatedIn(p: Project, a: number, b: number): Range[] {
  return (p.stems ?? []).filter((sp) => sp.end > a && sp.start < b).map((sp) => [Math.max(a, sp.start), Math.min(b, sp.end)] as Range);
}

// ── actions ──

export interface FetchOpts {
  song_ids?: string[];
  ranges?: Range[];
  priority?: Priority;
  reason?: string;
  process?: boolean;
  /** no toast on success */
  quiet?: boolean;
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
