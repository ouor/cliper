// Song and mute edits. Each helper is a pure edit of the song list; state/projects.ts applies it
// optimistically and syncs it with per-item calls (one song) or an atomic songs.set (several).
import type { Mute } from "@ipc/generated/Mute";
import type { Song } from "@ipc/generated/Song";
import { editSong, editSongs, getProject, rid, undo } from "@/state/projects";
import { useApp, toast, ask } from "@/state/app";
import { useWs } from "@/state/ws";
import { newId, songTitle } from "./song";
import { fmtDurationKo, fmtTime } from "./format";

export const MIN_SONG = 1;
/** 여기서 곡 끝내기: the song's own fade-out (기획 5.6-7) */
export const END_FADE = 1.5;

export function makeSong(start: number, end: number, title = ""): Song {
  return {
    id: newId("s"),
    title,
    artist: "",
    start,
    end,
    origin: "manual",
    enabled: true,
    reviewed: false,
    note: "",
    confidence: null,
    clip_id: null,
    mutes: [],
    mute_enabled: true,
    fade_in: null,
    fade_out: null,
    export: null,
  };
}

export function makeMute(start: number, end: number, gainDb: number | null): Mute {
  const s = useApp.getState().settings;
  return { id: newId("m"), start, end, fade_in: s.muteFadeIn, fade_out: s.muteFadeOut, origin: "manual", confirmed: true, gain_db: gainDb };
}

export async function addSong(pid: string, start: number, end: number, title = "") {
  const p = getProject(pid);
  const dur = p?.audio?.duration ?? p?.source?.duration ?? Infinity;
  const a = Math.max(0, Math.min(start, end));
  const b = Math.min(dur, Math.max(start, end));
  if (b - a < MIN_SONG) return;
  const song = makeSong(a, b, title);
  const synced = editSongs(pid, "곡 추가", (songs) => [...songs, song]);
  useWs.setState({ selected: song.id, multi: [], pendingRange: null, candidate: null });
  await synced;
  toast(`곡을 추가했어요 (${fmtTime(a, 0)} ~ ${fmtTime(b, 0)}).`, { kind: "ok" });
}

/** Typed boundary changes that alter the length a lot are confirmed (시/분 착오 방지). */
export async function setEdge(pid: string, songId: string, edge: "start" | "end", t: number, opts: { typed?: boolean; label?: string } = {}) {
  const p = getProject(pid);
  const s = p?.songs.find((x) => x.id === songId);
  if (!p || !s) return;
  const dur = p.audio?.duration ?? p.source?.duration ?? Infinity;
  let v = Math.max(0, Math.min(dur, t));
  if (edge === "start") v = Math.min(v, s.end - MIN_SONG);
  else v = Math.max(v, s.start + MIN_SONG);
  const before = s.end - s.start;
  const after = edge === "start" ? s.end - v : v - s.start;
  if (opts.typed && (after > before * 3 || after < before / 3) && Math.abs(after - before) > 120) {
    const r = await ask({
      title: "곡 길이가 크게 바뀌어요",
      body: `곡 길이가 ${fmtDurationKo(before)}에서 ${fmtDurationKo(after)}(으)로 바뀌어요. 맞나요?`,
      icon: "question",
      buttons: [
        { id: "cancel", label: "다시 입력", kind: "ghost" },
        { id: "ok", label: "바꾸기", kind: "primary" },
      ],
    });
    if (r !== "ok") return;
  }
  useWs.setState({ lastEdge: edge });
  await editSong(pid, songId, opts.label ?? (edge === "start" ? "곡 시작 바꾸기" : "곡 끝 바꾸기"), (x) => {
    x[edge] = v;
  });
}

export function nextUnreviewed(pid: string, after?: string): string | null {
  const p = getProject(pid);
  if (!p) return null;
  const list = [...p.songs].filter((s) => s.enabled).sort((a, b) => a.start - b.start);
  const i = after ? list.findIndex((s) => s.id === after) : -1;
  const rest = [...list.slice(i + 1), ...list.slice(0, Math.max(0, i + 1))];
  return rest.find((s) => !s.reviewed && s.id !== after)?.id ?? null;
}

export async function confirmSong(pid: string, songId: string) {
  const next = nextUnreviewed(pid, songId);
  const synced = editSong(pid, songId, "확인 완료", (s) => {
    s.reviewed = true;
    s.enabled = true;
  });
  if (next) useWs.setState({ selected: next, multi: [], selectedMute: null });
  await synced;
}

export async function toggleExclude(pid: string, songId: string) {
  const s = getProject(pid)?.songs.find((x) => x.id === songId);
  if (!s) return;
  await editSong(pid, songId, s.enabled ? "제외" : "복원", (x) => {
    x.enabled = !x.enabled;
  });
  toast(s.enabled ? `'${songTitle(s)}'을(를) 제외했어요. 목록 아래 "제외됨"에서 언제든 되돌릴 수 있어요.` : "곡을 복원했어요.", {
    kind: "info",
    action: s.enabled ? { label: "되돌리기", run: () => void toggleExclude(pid, songId) } : undefined,
  });
}

export async function deleteSong(pid: string, songId: string) {
  const s = getProject(pid)?.songs.find((x) => x.id === songId);
  if (!s) return;
  const r = await ask({
    title: "곡을 삭제할까요?",
    body: ["노래가 아니라면 [제외]를 추천해요. 제외한 곡은 다시 찾기를 해도 돌아오지 않고, 언제든 되돌릴 수 있어요.", "삭제도 Ctrl+Z로 되돌릴 수 있어요."],
    icon: "warn",
    buttons: [
      { id: "cancel", label: "취소", kind: "ghost" },
      { id: "exclude", label: "제외하기", kind: "subtle" },
      { id: "delete", label: "삭제", kind: "danger" },
    ],
  });
  if (r === "exclude") return toggleExclude(pid, songId);
  if (r !== "delete") return;
  const p = getProject(pid)!;
  const sorted = [...p.songs].sort((a, b) => a.start - b.start);
  const i = sorted.findIndex((x) => x.id === songId);
  const neighbor = sorted[i + 1] ?? sorted[i - 1];
  const synced = editSongs(pid, "곡 삭제", (songs) => songs.filter((x) => x.id !== songId));
  useWs.setState({ selected: neighbor?.id ?? null, multi: [] });
  await synced;
}

/** Split at t: the song ends at t, a new song starts at t. Mutes follow their time. */
export async function splitSong(pid: string, songId: string, t: number) {
  const s = getProject(pid)?.songs.find((x) => x.id === songId);
  if (!s) return;
  if (t <= s.start + MIN_SONG || t >= s.end - MIN_SONG) {
    toast("재생 위치가 곡 안에 있어야 나눌 수 있어요.", { kind: "warn" });
    return;
  }
  const b = makeSong(t, s.end);
  b.mutes = s.mutes.filter((m) => m.start >= t);
  b.enabled = s.enabled;
  const synced = editSongs(pid, "곡 나누기", (songs) => {
    const a = songs.find((x) => x.id === rid(pid, songId))!;
    a.end = t;
    a.mutes = a.mutes.filter((m) => m.start < t);
    a.reviewed = false;
    return [...songs, b];
  });
  useWs.setState({ selected: b.id, multi: [] });
  await synced;
  toast(`곡을 ${fmtTime(t, 1)}에서 나눴어요.`, { kind: "ok" });
}

/** Merge: the first song grows to the last one's end; titles/notes are kept in the note. */
export async function mergeSongs(pid: string, ids: string[]) {
  const p = getProject(pid);
  if (!p) return;
  const list = p.songs.filter((s) => ids.includes(s.id)).sort((a, b) => a.start - b.start);
  if (list.length < 2) {
    toast("합칠 곡을 Shift+클릭으로 두 개 이상 골라 주세요.", { kind: "info" });
    return;
  }
  const first = list[0];
  const rest = list.slice(1);
  const synced = editSongs(pid, "곡 합치기", (songs) => {
    const a = songs.find((x) => x.id === rid(pid, first.id))!;
    a.end = Math.max(...list.map((s) => s.end));
    const extra = rest.map((s) => [s.title && `합친 곡: ${s.title}`, s.note].filter(Boolean).join(" · ")).filter(Boolean);
    if (extra.length) a.note = [a.note, ...extra].filter(Boolean).join(" / ");
    if (!a.title) a.title = rest.find((s) => s.title)?.title ?? "";
    a.mutes = [...a.mutes, ...rest.flatMap((s) => s.mutes)];
    a.reviewed = false;
    return songs.filter((x) => !rest.some((r) => rid(pid, r.id) === x.id));
  });
  useWs.setState({ selected: first.id, multi: [] });
  await synced;
  toast(`곡 ${list.length}개를 합쳤어요.`, { kind: "ok" });
}

// ── mutes ──

export async function addMute(pid: string, songId: string, start: number, end: number) {
  const p = getProject(pid);
  if (!p) return null;
  const a = Math.min(start, end);
  const b = Math.max(start, end);
  if (b - a < 0.05) return null;
  const m = makeMute(a, b, p.settings.mute_gain_db);
  const synced = editSong(pid, songId, "멘트 구간 추가", (s) => {
    s.mutes.push(m);
  });
  useWs.setState({ selectedMute: m.id });
  await synced;
  return m.id;
}

export async function updateMute(pid: string, songId: string, muteId: string, label: string, fn: (m: Mute) => void) {
  await editSong(pid, songId, label, (s) => {
    const m = s.mutes.find((x) => x.id === rid(pid, muteId));
    if (m) {
      fn(m);
      if (m.end < m.start) [m.start, m.end] = [m.end, m.start];
    }
  });
}

export async function deleteMute(pid: string, songId: string, muteId: string) {
  const s = getProject(pid)?.songs.find((x) => x.id === songId);
  if (!s) return;
  const sorted = [...s.mutes].sort((a, b) => a.start - b.start);
  const i = sorted.findIndex((m) => m.id === muteId);
  const neighbor = sorted[i + 1] ?? sorted[i - 1];
  const synced = editSong(pid, songId, "멘트 구간 삭제", (x) => {
    x.mutes = x.mutes.filter((m) => m.id !== rid(pid, muteId));
  });
  useWs.setState({ selectedMute: neighbor?.id ?? null });
  await synced;
}

export async function splitMute(pid: string, songId: string, muteId: string, t: number) {
  const s = getProject(pid)?.songs.find((x) => x.id === songId);
  const m = s?.mutes.find((x) => x.id === muteId);
  if (!s || !m) return;
  if (t <= m.start + 0.05 || t >= m.end - 0.05) {
    toast("재생 위치가 선택한 구간 안에 있어야 나눌 수 있어요.", { kind: "warn" });
    return;
  }
  const b: Mute = { ...m, id: newId("m"), start: t };
  await editSong(pid, songId, "멘트 구간 나누기", (x) => {
    const a = x.mutes.find((y) => y.id === rid(pid, muteId));
    if (!a) return;
    a.end = t;
    x.mutes.push(b);
  });
}

/** 여기서 곡 끝내기: the song ends where the talk starts (export fades out), the mute goes. */
export async function endSongHere(pid: string, songId: string, muteId: string) {
  const s = getProject(pid)?.songs.find((x) => x.id === songId);
  const m = s?.mutes.find((x) => x.id === muteId);
  if (!s || !m) return;
  if (m.start <= s.start + MIN_SONG) {
    toast("곡 앞쪽 구간에서는 곡을 끝낼 수 없어요.", { kind: "warn" });
    return;
  }
  await editSong(pid, songId, "여기서 곡 끝내기", (x) => {
    x.end = Math.min(x.end, m.start);
    x.fade_out = END_FADE;
    x.mutes = x.mutes.filter((y) => y.id !== rid(pid, muteId) && y.start < x.end);
  });
  toast(`곡 끝을 ${fmtTime(m.start, 1)}로 옮겼어요. 이 곡은 내보낼 때 ${END_FADE}초 동안 페이드 아웃돼요.`, {
    kind: "ok",
    action: { label: "되돌리기 Ctrl+Z", run: () => void undo(pid) },
  });
}

/** A mute the user hasn't accepted yet (job suggestion). Manual mutes are always confirmed. */
export function isSuggestion(m: Mute): boolean {
  return !m.confirmed && m.origin !== "manual";
}

/**
 * Delete the unconfirmed suggestions of `songIds` in one edit, so one undo brings them all back
 * (#2). Confirmed and manual mutes stay. Several songs → one atomic `songs.set`.
 */
export async function deleteSuggestions(pid: string, songIds: string[], label = "제안 모두 지우기") {
  const p = getProject(pid);
  if (!p) return;
  const want = new Set(songIds.flatMap((id) => [id, rid(pid, id)]));
  const gone = p.songs.filter((s) => want.has(s.id)).flatMap((s) => s.mutes.filter(isSuggestion));
  if (!gone.length) return;
  const songsHit = p.songs.filter((s) => want.has(s.id) && s.mutes.some(isSuggestion)).length;
  const synced = editSongs(pid, label, (songs) => {
    for (const s of songs) if (want.has(s.id)) s.mutes = s.mutes.filter((m) => !isSuggestion(m));
    return songs;
  });
  const sel = useWs.getState().selectedMute;
  if (sel && gone.some((m) => m.id === sel)) useWs.setState({ selectedMute: null });
  toast(`확인 전 제안 ${gone.length}개를 지웠어요${songsHit > 1 ? ` (${songsHit}곡)` : ""}. 확인한 구간과 직접 만든 구간은 그대로예요.`, {
    kind: "ok",
    action: { label: "되돌리기 Ctrl+Z", run: () => void undo(pid) },
  });
  await synced;
}
