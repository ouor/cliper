// Video coverage on the VOD timeline and the download rules (기획서 14장, review A-9).
// Pure: no app state, no IPC — every rule here is unit-tested (coverage.test.ts, `npm test`).
// lib/video.ts re-exports these and adds the actions (video.fetch, toasts).
import type { Project } from "@ipc/generated/Project";
import type { Song } from "@ipc/generated/Song";
import type { StemSpan } from "@ipc/generated/StemSpan";
import type { VideoState } from "@ipc/generated/VideoState";
import type { CoverageRange } from "@ipc/generated/CoverageRange";
import { fmtDurationKo } from "./format.ts";

export type Range = [number, number];
/** a point / stretch of the VOD: the engine states plus none (no video) and gone (can't fetch) */
export type Cov = "have" | "fetching" | "queued" | "failed" | "none" | "gone";

/** Margin ("앞뒤 여유") of a project: both sides use the same value (Q-S3). */
export function marginOf(p: Pick<Project, "settings">): number {
  return Math.max(p.settings.margin_before, p.settings.margin_after);
}

export function vodGone(p: Pick<Project, "vod"> | undefined): boolean {
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

type SongLike = Pick<Song, "id" | "start" | "end"> & { video_wanted?: boolean };

/**
 * Does this song "have video" (14.4, QA-4, U-15)? Only the song's own range counts — never a
 * neighbour's margin, a [여기 영상 받기] or a range fetch that merely touches it:
 * - a request for this song is pending, or
 * - at least half of [start, end] is on disk or coming, or
 * - the user asked for it (`Song.video_wanted`) and some of its range is still on disk or coming
 *   (after a cleanup that removed it all, it no longer counts: the re-fetch banner applies).
 * Songs that don't are never fetched by boundary edits, [영상 맞추기] or the export defaults.
 */
export function songHasVideo(vs: VideoState | undefined, s: SongLike): boolean {
  if (vs?.local) return true;
  if ((vs?.requests ?? []).some((r) => r.song_ids.includes(s.id) && r.remaining_sec > 0)) return true;
  const len0 = Math.max(0.01, s.end - s.start);
  const covered = len(partsIn(vs, s.start, s.end, ["have", "fetching", "queued"]));
  if (covered >= len0 * 0.5) return true;
  return !!s.video_wanted && covered > 0.05;
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
  /** the song "has video" (songHasVideo): boundary edits top it up */
  touched: boolean;
  /** row text (14.3), "" when nothing to say */
  label: string;
}

/** "앞 20초" / "뒤 15초" / "1분 10초" — where a song's video is missing */
export function missingWhere(s: Pick<Song, "start" | "end">, missing: Range[]): string {
  if (missing.length === 1) {
    const [a, b] = missing[0];
    if (a <= s.start + 0.05 && b < s.end - 0.05) return `앞 ${fmtDurationKo(b - a)}`;
    if (b >= s.end - 0.05 && a > s.start + 0.05) return `뒤 ${fmtDurationKo(b - a)}`;
  }
  return fmtDurationKo(len(missing));
}

/** Song status against the "have" ranges; the margins never warn (14.3, PM 3-2). */
export function songVideo(p: Pick<Project, "vod">, vs: VideoState | undefined, s: SongLike): SongVideo {
  const len0 = Math.max(0.01, s.end - s.start);
  const pieces = piecesIn(vs, s.start, s.end, vodGone(p));
  const missing = partsIn(vs, s.start, s.end);
  const missingSec = len(missing);
  const fraction = Math.max(0, Math.min(1, 1 - missingSec / len0));
  const pending = pieces.some((x) => x.state === "fetching" || x.state === "queued");
  const failed = pieces.some((x) => x.state === "failed");
  const touched = songHasVideo(vs, s);
  const where = () => missingWhere(s, missing);
  if (missingSec < 0.05) return { state: "ready", missing: [], missingSec: 0, fraction: 1, pending: false, touched: true, label: "영상 ✓" };
  if (pending) {
    // the engine's per-song progress of its requests (C2-3), when it has one for this song
    const sp = (vs?.requests ?? []).flatMap((r) => (r.songs ?? []).filter((x) => x.song_id === s.id));
    const tot = sp.reduce((n, x) => n + x.total_sec, 0);
    if (tot > 0) {
      const frac = Math.max(0, Math.min(1, sp.reduce((n, x) => n + x.have_sec, 0) / tot));
      if (!(missing.length === 1 && (missing[0][0] > s.start + 0.05 || missing[0][1] < s.end - 0.05)) || frac < 0.5)
        return { state: "fetching", missing, missingSec, fraction: frac, pending, touched, label: `영상 받는 중 ${Math.floor(frac * 100)}%` };
    }
    const partial = fraction > 0.01 || missing.length > 1 || (missing.length === 1 && (missing[0][0] > s.start + 0.05 || missing[0][1] < s.end - 0.05));
    const w = where();
    return { state: "fetching", missing, missingSec, fraction, pending, touched, label: partial && fraction > 0.5 ? `${w.startsWith("앞") || w.startsWith("뒤") ? w : `영상 ${w}`} 받는 중` : `영상 받는 중 ${Math.floor(fraction * 100)}%` };
  }
  if (vodGone(p)) return { state: "gone", missing, missingSec, fraction, pending, touched, label: fraction > 0 ? `${where()} 영상 없음 · 다시보기 없음` : "다시보기 없음" };
  if (failed) return { state: "failed", missing, missingSec, fraction, pending, touched, label: "영상 일부를 받지 못했어요" };
  if (fraction > 0.001) return { state: "partial", missing, missingSec, fraction, pending, touched, label: `${where()} 영상 없음` };
  return { state: "none", missing, missingSec, fraction, pending, touched, label: "" };
}

// ── download rules ──

export interface TopUpInput {
  song: SongLike & { enabled: boolean };
  /** the song's range before the edits of this batch */
  was: Range;
  margin: number;
  vs: VideoState | undefined;
  /** top-up request ids this app made for this song (U-14: only these may be cancelled) */
  ownRequests: string[];
}

export interface TopUpPlan {
  /** unstarted top-ups of this song that now reach past song ± margin (QA-8) */
  cancel: string[];
  /** missing video of the side(s) that grew, with that side's margin; [] = nothing to fetch */
  fetch: Range[];
  sec: number;
}

/** above this, an automatic top-up asks first (14.4) */
export const AUTO_TOPUP_MAX_SEC = 300;

/**
 * Boundary top-up (14.4): for a song that had video before the edit, fetch only what is missing
 * of the side that grew (+ its margin); shrinking never fetches, and cancels this song's
 * unstarted top-ups that now lie outside. Songs without video are left alone.
 */
export function planTopUp(i: TopUpInput): TopUpPlan {
  const none: TopUpPlan = { cancel: [], fetch: [], sec: 0 };
  const { song: s, was, margin: m, vs } = i;
  if (!s.enabled || vs?.local) return none;
  if (!songHasVideo(vs, { ...s, start: was[0], end: was[1] })) return none;
  const a = s.start - m;
  const b = s.end + m;
  const cancel = (vs?.requests ?? [])
    .filter((r) => i.ownRequests.includes(r.id) && r.reason === "topup" && r.fetching_sec === 0 && r.ranges.some(([x, y]) => x < a - 0.5 || y > b + 0.5))
    .map((r) => r.id);
  const grown: Range[] = [];
  if (s.start < was[0] - 0.01) grown.push([Math.max(0, a), was[0]]);
  if (s.end > was[1] + 0.01) grown.push([was[1], b]);
  const fetch = grown.flatMap(([x, y]) => partsIn(vs, x, y, ["none", "failed"]));
  const sec = len(fetch);
  return { cancel, fetch: sec >= 0.5 ? fetch : [], sec: sec >= 0.5 ? sec : 0 };
}

/** Songs [영상 맞추기] would fetch: they have video but their own range lacks some (14.4). */
export function songsToFit(p: Pick<Project, "vod" | "songs">, vs: VideoState | undefined): { song: Song; sv: SongVideo }[] {
  if (!vs || vs.local || vodGone(p)) return [];
  return p.songs
    .filter((s) => s.enabled)
    .map((s) => ({ song: s, sv: songVideo(p, vs, s) }))
    .filter((x) => x.sv.touched && (x.sv.state === "partial" || x.sv.state === "failed"));
}

/** Export pre-selection (14.11): reviewed songs (all when none is reviewed) that have video. */
export function exportDefaults(p: Pick<Project, "songs">, vs: VideoState | undefined): string[] {
  const songs = p.songs.filter((s) => s.enabled);
  const anyReviewed = songs.some((s) => s.reviewed);
  return songs.filter((s) => (s.reviewed || !anyReviewed) && songHasVideo(vs, s)).map((s) => s.id);
}

// ── stems (separated audio) ──

/** The stem span to play for a song: the one overlapping it most; ties → the later span
 *  (spans may overlap by 20 s at the 30-min cap, protocol 4: the later one is current there). */
export function spanFor(p: Pick<Project, "stems">, a: number, b: number): StemSpan | null {
  let best: StemSpan | null = null;
  let bestO = 0;
  for (const sp of p.stems ?? []) {
    const o = Math.min(b, sp.end) - Math.max(a, sp.start);
    if (o > bestO + 1e-6 || (best && Math.abs(o - bestO) <= 1e-6 && o > 0 && sp.start > best.start)) {
      bestO = Math.max(o, bestO);
      best = sp;
    }
  }
  return best;
}

/** The span to read at VOD time t: the latest-starting span that covers it. */
export function spanAt(p: Pick<Project, "stems">, t: number): StemSpan | null {
  let best: StemSpan | null = null;
  for (const sp of p.stems ?? []) if (t >= sp.start && t <= sp.end && (!best || sp.start > best.start)) best = sp;
  return best;
}

/** Parts of [a, b] that have video but aren't separated yet (stems.ensure would do them). */
export function unseparated(p: Pick<Project, "stems">, vs: VideoState | undefined, a: number, b: number): Range[] {
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
export function separatedIn(p: Pick<Project, "stems">, a: number, b: number): Range[] {
  return (p.stems ?? []).filter((sp) => sp.end > a && sp.start < b).map((sp) => [Math.max(a, sp.start), Math.min(b, sp.end)] as Range);
}
