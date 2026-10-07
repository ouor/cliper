// Song/mute domain helpers: status, labels, clip state, positions, links, templates.
import type { Clip } from "@ipc/generated/Clip";
import type { Mute } from "@ipc/generated/Mute";
import type { Params } from "@ipc/generated/Params";
import type { Project } from "@ipc/generated/Project";
import type { Song } from "@ipc/generated/Song";
import type { DoubtReason } from "@ipc/generated/DoubtReason";
import { fmtDate, fmtStamp } from "./format";
import type { Probs } from "./peaks";

let idCounter = 0;
/** Client-side id (the engine keeps any non-empty id), same shape as the engine's. */
export function newId(prefix: "s" | "m"): string {
  idCounter = (idCounter + 1) & 0xff;
  const t = (Date.now() * 1000 + Math.floor(Math.random() * 1000)).toString(16);
  return `${prefix}${t}${idCounter.toString(16).padStart(2, "0")}`;
}

export type ReviewStatus = "excluded" | "doubt" | "unreviewed" | "reviewed";

export function reviewStatus(s: Song): ReviewStatus {
  if (!s.enabled) return "excluded";
  if (s.reviewed) return "reviewed";
  if (s.confidence && s.confidence.doubts.length > 0) return "doubt";
  return "unreviewed";
}

export function songTitle(s: Song): string {
  return s.title.trim() || "제목 없음";
}

export function sortedSongs(p: Project): Song[] {
  return [...p.songs].sort((a, b) => a.start - b.start);
}

/** Song numbers ("07") among enabled songs in broadcast order. */
export function songNumbers(p: Project): Map<string, number> {
  const m = new Map<string, number>();
  let i = 0;
  for (const s of sortedSongs(p)) if (s.enabled) m.set(s.id, ++i);
  return m;
}

export function clipById(p: Project, id: string | null | undefined): Clip | null {
  if (!id) return null;
  return p.clips.find((c) => c.id === id) ?? null;
}

export function clipCovered(c: Clip): [number, number] | null {
  if (!c.video) return null;
  return [c.video.vod_start + c.offset, c.video.vod_start + c.offset + c.video.duration];
}

/** The fetched clip that fully contains the song (what the engine would use). */
export function hostClip(p: Project, s: Song): Clip | null {
  const own = clipById(p, s.clip_id);
  if (own?.video) {
    const cov = clipCovered(own)!;
    if (cov[0] <= s.start && s.end <= cov[1]) return own;
  }
  for (const c of p.clips) {
    const cov = clipCovered(c);
    if (cov && cov[0] <= s.start && s.end <= cov[1]) return c;
  }
  return null;
}

export type VideoState = "none" | "ready" | "stemsMissing" | "refetch";

export function videoState(p: Project, s: Song): VideoState {
  const host = hostClip(p, s);
  if (host) return host.stems ? "ready" : "stemsMissing";
  // a fetched clip overlaps the song but does not contain it → grew past its margins
  const overlaps = p.clips.some((c) => {
    const cov = clipCovered(c);
    return cov && cov[0] < s.end && s.start < cov[1];
  });
  return overlaps ? "refetch" : "none";
}

/** Song edited by hand: a manual song, or an auto song whose range differs from the detector. */
export function isEdited(p: Project, s: Song): boolean {
  if (s.origin === "manual") return true;
  const segs = p.detection?.segments ?? [];
  return !segs.some(([a, b]) => Math.abs(a - s.start) < 0.05 && Math.abs(b - s.end) < 0.05);
}

const DOUBT_LABEL: Record<DoubtReason, string> = {
  short: "짧음",
  long: "김",
  weak_singing: "노래 약함",
  talk_inside: "중간에 말",
  resplit: "나눠 찾음",
};

export interface Label {
  text: string;
  kind: "doubt" | "info" | "accent" | "plain" | "ok" | "danger" | "suggest";
  tip?: string;
}

export function songLabels(p: Project, s: Song, shortSec: number): Label[] {
  const out: Label[] = [];
  const doubts = s.confidence?.doubts ?? [];
  if (s.enabled && !s.reviewed && doubts.length) out.push({ text: "애매함", kind: "doubt", tip: "자동으로 찾았지만 확실하지 않아요.\n시작과 끝을 들어 보고 확인해 주세요." });
  const dur = s.end - s.start;
  const reasons = new Set(doubts.map((d) => DOUBT_LABEL[d]));
  if (dur < shortSec) reasons.add("짧음");
  for (const r of reasons) out.push({ text: r, kind: r === "짧음" ? "plain" : "doubt", tip: r === "짧음" ? "1절만 부른 곡일 수 있어요" : undefined });
  const title = s.title.trim().toLowerCase();
  if (title) {
    const same = p.songs.filter((o) => o.enabled && o.title.trim().toLowerCase() === title);
    if (same.length > 1) {
      const idx = same.sort((a, b) => a.start - b.start).findIndex((o) => o.id === s.id) + 1;
      out.push({ text: `같은 제목 (${idx})`, kind: "info", tip: "앵콜이나 다시 부른 곡일 수 있어요" });
    }
  }
  const al = clipById(p, s.clip_id)?.alignment;
  if (al && !al.ok) out.push({ text: "타이밍 확인 필요", kind: "doubt", tip: "받은 영상과 소리의 시간을 확인하지 못했어요.\n시작·끝 듣기로 자른 위치를 확인해 주세요." });
  if (isEdited(p, s)) out.push({ text: s.origin === "manual" ? "직접 추가" : "직접 고침", kind: "plain" });
  return out;
}

export function unconfirmedMutes(s: Song): number {
  return s.mutes.filter((m) => !m.confirmed).length;
}

export type MutePos = "곡 앞" | "전주" | "간주" | "후주" | "곡 뒤";

export function mutePosition(m: Mute, s: Song): MutePos {
  if (m.end <= s.start + 0.5) return "곡 앞";
  if (m.start >= s.end - 0.5) return "곡 뒤";
  const dur = s.end - s.start;
  const mid = (m.start + m.end) / 2 - s.start;
  const edge = Math.min(40, dur * 0.18);
  if (mid < edge) return "전주";
  if (mid > dur - edge) return "후주";
  return "간주";
}

export const SING_THRESHOLD = 0.05;

/** Seconds inside the mute where singing is likely (PANNs sing curve). */
export function singOverlap(m: Mute, probs: Probs | null): number {
  if (!probs) return 0;
  const i0 = Math.max(0, Math.floor(m.start * probs.fps));
  const i1 = Math.min(probs.n, Math.ceil(m.end * probs.fps));
  let n = 0;
  for (let i = i0; i < i1; i++) if (probs.sing[i] > SING_THRESHOLD) n++;
  return n / probs.fps;
}

/** Mutes that lie outside the fetched video can't be exported. */
export function muteOutsideClip(m: Mute, clip: Clip | null): boolean {
  const cov = clip ? clipCovered(clip) : null;
  if (!cov) return false;
  return m.start < cov[0] - 0.01 || m.end > cov[1] + 0.01;
}

// ── detection sensitivity (1 확실한 곡만 … 5 애매한 곡도) ──
const HI = [0.36, 0.28, 0.2, 0.12, 0.05];
const LO = [0.1, 0.05, 0.0, -0.05, -0.1];
const SING = [0.03, 0.022, 0.015, 0.009, 0.004];

export function sensitivityParams(defaults: Params, level: number, minSongSec: number): Params {
  const i = Math.max(0, Math.min(4, Math.round(level) - 1));
  return { ...defaults, pann_hi: HI[i], pann_lo: LO[i], seg_sing_min: SING[i], min_song_sec: minSongSec };
}

export function sensitivityFromParams(p: Params | null | undefined): number {
  if (!p) return 3;
  let best = 2;
  let bestD = Infinity;
  HI.forEach((v, i) => {
    const d = Math.abs(v - p.pann_hi);
    if (d < bestD) (bestD = d), (best = i);
  });
  return best + 1;
}

export const SENSITIVITY_LABEL = ["확실한 곡만", "조금 엄격하게", "기본", "조금 넉넉하게", "애매한 곡도"];

// ── links & description text (앱이 만듦) ──
export function vodLink(p: Project, sec?: number): string {
  const src = p.source;
  if (!src || src.platform === "local") return "";
  const base = src.url;
  if (sec === undefined) return base;
  const t = Math.max(0, Math.floor(sec));
  if (src.platform === "chzzk") return `${base}${base.includes("?") ? "&" : "?"}currentTime=${t}`;
  if (src.platform === "soop") return `${base}${base.includes("?") ? "&" : "?"}change_second=${t}`;
  return base;
}

export function channelLink(p: Project): string {
  const src = p.source;
  const id = src?.catalog.channel.id;
  if (!src || !id) return "";
  if (src.platform === "chzzk") return `https://chzzk.naver.com/${id}`;
  if (src.platform === "soop") return `https://ch.sooplive.co.kr/${id}`;
  return "";
}

export function streamerName(p: Project): string {
  return p.source?.catalog.channel.name || p.source?.channel || "";
}

export function broadcastDate(p: Project): string {
  return fmtDate(p.source?.catalog.published_at ?? p.created_at);
}

export function renderDescription(template: string, p: Project, s: Song): string {
  const vars: Record<string, string> = {
    스트리머: streamerName(p),
    곡명: songTitle(s),
    원곡자: s.artist,
    방송일: broadcastDate(p),
    방송제목: p.source?.title ?? "",
    원본링크: vodLink(p),
    원본링크_시점포함: vodLink(p, s.start),
    채널링크: channelLink(p),
    시작시각: fmtStamp(s.start),
  };
  return template
    .replace(/\{([^{}]+)\}/g, (m, k: string) => (k in vars ? vars[k] : m))
    .split("\n")
    .filter((line) => !/:\s*$/.test(line.trim()) || line.trim() === "")
    .join("\n")
    .trim();
}

export const DESC_VARS = ["{스트리머}", "{곡명}", "{원곡자}", "{방송일}", "{방송제목}", "{원본링크}", "{원본링크_시점포함}", "{채널링크}", "{시작시각}"];

export function projectTitle(p: Project | null | undefined): string {
  if (!p) return "";
  return p.source?.title || p.input;
}

export function platformLabel(platform: string | null | undefined): string {
  return platform === "chzzk" ? "치지직" : platform === "soop" ? "SOOP" : platform === "local" ? "내 PC 녹화본" : "";
}

/** Characters the engine will replace in file names. */
export function hasUnsafeFileChars(t: string): boolean {
  return /[\\/:*?"<>|]/.test(t);
}
