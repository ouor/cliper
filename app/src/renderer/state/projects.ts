// Projects: summaries, full projects and song editing.
//
// The engine is the source of truth (`project.changed`). Edits are applied optimistically as
// *pending ops* on top of the latest engine state, then synced with race-free per-item calls
// (song.update / song.add / song.remove / mute.upsert / mute.remove) carrying `base_revision`.
// On CONFLICT the op is re-applied to the fresh state and retried. Undo restores only the songs
// an op touched, so it never wipes mute suggestions a job added in the meantime.
import { create } from "zustand";
import type { Mute } from "@ipc/generated/Mute";
import type { Project } from "@ipc/generated/Project";
import type { ProjectSummary } from "@ipc/generated/ProjectSummary";
import type { Settings } from "@ipc/generated/Settings";
import type { Song } from "@ipc/generated/Song";
import { call, EngineError } from "@/lib/rpc";
import { friendly } from "@/lib/errors";
import { toast } from "./app";
import { useWs } from "./ws";
import { len, separatedIn } from "@/lib/video";
import { loadVideo } from "./video";

/** App-only per-project state, kept in userData by `Project.uid` (protocol 4). "Listened" and
 *  "video wanted" live in the project itself now (`Song.listened`, `Song.video_wanted`). */
export interface ProjectUi {
  /** the project this belongs to (Project.uid) */
  uid?: string;
  lastSongId?: string;
  firstExportHintShown?: boolean;
}

/** what older versions stored under `project:<id>` (imported once into the project) */
interface LegacyUi {
  listened?: string[];
  videoSongs?: string[];
  lastSongId?: string;
  firstExportHintShown?: boolean;
}

/** One undoable step: the touched songs before and after (null = absent). */
interface HistoryEntry {
  label: string;
  before: Map<string, Song | null>;
  after: Map<string, Song | null>;
}
interface History {
  undo: HistoryEntry[];
  redo: HistoryEntry[];
}

interface ProjectsStore {
  summaries: ProjectSummary[];
  listLoaded: boolean;
  /** displayed state = engine state + pending ops */
  projects: Record<string, Project>;
  /** number of edits not yet confirmed by the engine */
  pending: Record<string, number>;
  history: Record<string, History>;
  ui: Record<string, ProjectUi>;
}

export const useProjects = create<ProjectsStore>(() => ({
  summaries: [],
  listLoaded: false,
  projects: {},
  pending: {},
  history: {},
  ui: {},
}));

interface PendingOp {
  label: string;
  apply: (songs: Song[]) => Song[];
  /** the undo entry this op created (dropped if the op fails) */
  history?: HistoryEntry;
}

const engineState: Record<string, Project> = {};
/** QA counters (exposed on window.__cliper) */
export const syncStats = { ops: 0, calls: 0, conflicts: 0 };
const pendingOps: Record<string, PendingOp[]> = {};
const chains: Record<string, Promise<void>> = {};
/** client id → engine id (song.add / mute.upsert assign their own ids) */
const aliases: Record<string, Map<string, string>> = {};

function clone<T>(v: T): T {
  return structuredClone(v);
}

/** Resolve a (possibly client-made) song or mute id to the engine's id. */
export function rid(pid: string, id: string): string {
  const m = aliases[pid];
  let cur = id;
  for (let i = 0; m && i < 8 && m.has(cur); i++) cur = m.get(cur)!;
  return cur;
}

function addAlias(pid: string, from: string, to: string) {
  if (!from || from === to) return;
  (aliases[pid] ??= new Map()).set(from, to);
  // keep the UI pointing at the same item
  const ws = useWs.getState();
  const patch: Partial<ReturnType<typeof useWs.getState>> = {};
  if (ws.selected === from) patch.selected = to;
  if (ws.selectedMute === from) patch.selectedMute = to;
  if (ws.multi.includes(from)) patch.multi = ws.multi.map((x) => (x === from ? to : x));
  if (Object.keys(patch).length) useWs.setState(patch);
}

function summarize(p: Project): ProjectSummary {
  const enabled = p.songs.filter((s) => s.enabled);
  return {
    id: p.id,
    title: p.source?.title ?? null,
    channel: p.source?.channel ?? null,
    platform: p.source?.platform ?? null,
    duration: p.source?.duration ?? null,
    songs: enabled.length,
    exported: enabled.filter((s) => s.export).length,
    thumbnail: p.source?.catalog.thumbnail ?? null,
    updated_at: p.updated_at,
    unreviewed: enabled.filter((s) => !s.reviewed).length,
    unconfirmed_mutes: enabled.reduce((n, s) => n + s.mutes.filter((m) => !m.confirmed).length, 0),
    separated_songs: enabled.filter((s) => len(separatedIn(p, s.start, s.end)) >= s.end - s.start - 0.5).length,
    pinned: p.pinned,
    vod: p.vod ?? undefined,
    has_audio: !!p.audio && !p.audio.dropped,
    has_detection: !!p.detection,
    last_error: p.last_error,
  };
}

/** Recompute the displayed project from the engine state and the pending ops. */
function display(pid: string) {
  const e = engineState[pid];
  if (!e) return;
  let songs = e.songs;
  for (const op of pendingOps[pid] ?? []) {
    try {
      songs = op.apply(clone(songs));
    } catch {
      /* an op that no longer applies is dropped when it runs */
    }
  }
  useProjects.setState((s) => ({
    projects: { ...s.projects, [pid]: songs === e.songs ? e : { ...e, songs } },
    pending: { ...s.pending, [pid]: pendingOps[pid]?.length ?? 0 },
  }));
}

function setEngine(p: Project) {
  const cur = engineState[p.id];
  // notifications and responses can arrive out of order; never go back in time — unless it is
  // another project with the same id (Project.uid differs): then start over. Copies of a folder
  // share the uid; opening / dropping a location resets explicitly (replaceProject, QA-5)
  if (cur && cur.uid !== p.uid) {
    resetProjectState(p.id);
  } else if (cur && cur.revision > p.revision) return;
  engineState[p.id] = p;
  display(p.id);
}

/** Forget everything cached about a project id (state, pending edits, undo): its folder changed. */
export function resetProjectState(id: string) {
  delete engineState[id];
  pendingOps[id] = [];
  aliases[id]?.clear();
  // everything else keyed by this id (U-12): UI state, video state, top-up watch, ensure keys
  useProjects.setState((s) => {
    const ui = { ...s.ui };
    delete ui[id];
    return { history: { ...s.history, [id]: { undo: [], redo: [] } }, ui };
  });
  for (const l of resetListeners) l(id);
}
const resetListeners = new Set<(id: string) => void>();
/** Other stores keyed by project id forget it here (video state, top-ups, talk editor). */
export function onProjectReset(cb: (id: string) => void): () => void {
  resetListeners.add(cb);
  return () => resetListeners.delete(cb);
}

/** Take this project as the engine's current state, whatever was cached for its id before. */
export function replaceProject(p: Project) {
  resetProjectState(p.id);
  setEngine(p);
}

export async function refreshList() {
  try {
    const summaries = await call("project.list", {});
    summaries.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
    useProjects.setState({ summaries, listLoaded: true });
  } catch {
    useProjects.setState({ listLoaded: true });
  }
}

export async function loadProject(id: string): Promise<Project> {
  const p = await call("project.get", { id });
  setEngine(p);
  void loadVideo(id);
  await loadUi(useProjects.getState().projects[id] ?? p);
  return useProjects.getState().projects[id] ?? p;
}

/** Put a project returned by some call (e.g. project.create) into the store. */
export function putProject(p: Project) {
  setEngine(p);
}

/** project.changed */
export function applyChanged(p: Project) {
  setEngine(p);
  const s = useProjects.getState();
  const idx = s.summaries.findIndex((x) => x.id === p.id);
  // the folder (Project locations) only comes with project.list: keep it
  const summary = { ...summarize(p), dir: idx >= 0 ? s.summaries[idx].dir : undefined };
  const summaries = idx >= 0 ? s.summaries.map((x, i) => (i === idx ? summary : x)) : [summary, ...s.summaries];
  summaries.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
  useProjects.setState({ summaries });
}

export function getProject(id: string): Project | undefined {
  return useProjects.getState().projects[id];
}

// ── sync: diff two song lists into per-item calls ──

const SONG_FIELDS = ["title", "artist", "start", "end", "enabled", "reviewed", "note", "mute_enabled", "fade_in", "fade_out"] as const;

function muteEq(a: Mute, b: Mute): boolean {
  return a.start === b.start && a.end === b.end && a.fade_in === b.fade_in && a.fade_out === b.fade_out && a.confirmed === b.confirmed && a.gain_db === b.gain_db && a.origin === b.origin;
}

/**
 * Send the difference between the engine's songs and `target`.
 * - one song touched: per-item calls (song.update / mute.* / song.remove / song.add), race-free
 *   against jobs that edit other songs;
 * - several songs (merge, split, multi-song undo): one atomic `songs.set`.
 * Every call carries the revision this op expects (its first snapshot, then the revision our
 * own previous call produced), so any change made by someone else in between → CONFLICT.
 */
async function syncSongs(pid: string, target: Song[]) {
  const start = engineState[pid];
  let expected = start.revision;
  const take = (p: Project) => {
    expected = p.revision;
    setEngine(p);
  };
  const base = start.songs;
  const byId = new Map(base.map((s) => [s.id, s]));
  const targetIds = new Set(target.map((s) => s.id));
  const removed = base.filter((s) => !targetIds.has(s.id));
  const added = target.filter((t) => !byId.has(t.id));
  const changed = target.filter((t) => byId.has(t.id) && JSON.stringify(editable(t)) !== JSON.stringify(editable(byId.get(t.id)!)));
  if (removed.length + added.length + changed.length === 0) return;
  syncStats.calls++;
  if (removed.length + added.length + changed.length > 1) {
    // atomic: the engine keeps our (client) ids for new songs and mutes
    take(await call("songs.set", { id: pid, songs: target, base_revision: expected }));
    return;
  }
  for (const s of removed) take(await call("song.remove", { id: pid, song_id: s.id, base_revision: expected }));
  for (const t of changed) {
    const b = byId.get(t.id)!;
    const patch: Partial<Song> = {};
    for (const k of SONG_FIELDS) if (t[k] !== b[k]) (patch as Record<string, unknown>)[k] = t[k];
    if (Object.keys(patch).length) take(await call("song.update", { id: pid, song_id: t.id, patch, base_revision: expected }));
    const bm = new Map(b.mutes.map((m) => [m.id, m]));
    const tm = new Set(t.mutes.map((m) => m.id));
    for (const m of b.mutes) {
      if (!tm.has(m.id)) take(await call("mute.remove", { id: pid, song_id: t.id, mute_id: m.id, base_revision: expected }));
    }
    for (const m of t.mutes) {
      const old = bm.get(m.id);
      if (old && muteEq(old, m)) continue;
      const r = await call("mute.upsert", { id: pid, song_id: t.id, mute: m, base_revision: expected });
      if (!old) addAlias(pid, m.id, r.mute_id); // before the state update, so no duplicate shows (L1)
      take(r.project);
    }
  }
  for (const t of added) {
    const r = await call("song.add", { id: pid, song: t, base_revision: expected });
    addAlias(pid, t.id, r.song_id);
    take(r.project);
  }
}

/** The fields an edit can change (engine-owned ones excluded). */
function editable(s: Song) {
  const o: Record<string, unknown> = {};
  for (const k of SONG_FIELDS) o[k] = s[k];
  o.mutes = s.mutes.map((m) => [m.id, m.start, m.end, m.fade_in, m.fade_out, m.confirmed, m.gain_db, m.origin]);
  return o;
}

async function runOp(pid: string, op: PendingOp) {
  syncStats.ops++;
  try {
    for (let attempt = 0; ; attempt++) {
      try {
        const target = op.apply(clone(engineState[pid].songs));
        await syncSongs(pid, target);
        return;
      } catch (e) {
        if (e instanceof EngineError && e.engineCode === "CONFLICT" && attempt < 3) {
          // someone (usually a job) changed the project: re-apply onto the fresh state
          syncStats.conflicts++;
          setEngine(await call("project.get", { id: pid }));
          continue;
        }
        throw e;
      }
    }
  } catch (e) {
    const f = friendly(e);
    toast(`${op.label}: ${f.title} ${f.hint ?? ""}`.trim(), {
      kind: "error",
      action: f.code === "TIMEOUT" || f.code === "ENGINE_DOWN" ? { label: "엔진 다시 시작", run: () => void window.cliper.restartEngine() } : undefined,
    });
    // a failed edit is not undoable (its "before" would be stale)
    if (op.history) {
      const h = useProjects.getState().history[pid];
      if (h) useProjects.setState((s) => ({ history: { ...s.history, [pid]: { undo: h.undo.filter((x) => x !== op.history), redo: h.redo.filter((x) => x !== op.history) } } }));
    }
    try {
      setEngine(await call("project.get", { id: pid }));
    } catch {
      /* engine down */
    }
  } finally {
    pendingOps[pid] = (pendingOps[pid] ?? []).filter((x) => x !== op);
    display(pid);
  }
}

function enqueue(pid: string, op: PendingOp): Promise<void> {
  (pendingOps[pid] ??= []).push(op);
  display(pid);
  const next = (chains[pid] ?? Promise.resolve()).then(() => runOp(pid, op));
  chains[pid] = next;
  return next;
}

function touched(before: Song[], after: Song[]): [Map<string, Song | null>, Map<string, Song | null>] {
  const b = new Map(before.map((s) => [s.id, s]));
  const a = new Map(after.map((s) => [s.id, s]));
  const ids = new Set([...b.keys(), ...a.keys()]);
  const mb = new Map<string, Song | null>();
  const ma = new Map<string, Song | null>();
  for (const id of ids) {
    const x = b.get(id) ?? null;
    const y = a.get(id) ?? null;
    if (JSON.stringify(x && editable(x)) !== JSON.stringify(y && editable(y))) {
      mb.set(id, x && clone(x));
      ma.set(id, y && clone(y));
    }
  }
  return [mb, ma];
}

/**
 * Move the touched songs from `from` to `to` (undo: after → before, redo: before → after),
 * applying only what the op itself changed: the song fields that differ, and the mutes the op
 * added / removed / changed. Mutes that appeared since (job suggestions) are left alone.
 */
function restore(pid: string, to: Map<string, Song | null>, from: Map<string, Song | null>, songs: Song[]): Song[] {
  let out = songs;
  for (const [key, want] of to) {
    const was = from.get(key) ?? null;
    const id = rid(pid, key);
    const cur = out.find((s) => s.id === id);
    if (!want) {
      out = out.filter((s) => s.id !== id);
      continue;
    }
    if (!cur || !was) {
      // re-add (undo of a removal / redo of an add)
      if (!cur) out = [...out, { ...clone(want), id, mutes: want.mutes.map((m) => ({ ...m, id: rid(pid, m.id) })) }];
      continue;
    }
    const next: Song = { ...cur, mutes: [...cur.mutes] };
    for (const k of SONG_FIELDS) if (want[k] !== was[k]) (next as Record<string, unknown>)[k] = want[k];
    const wantM = new Map(want.mutes.map((m) => [rid(pid, m.id), m]));
    const wasM = new Map(was.mutes.map((m) => [rid(pid, m.id), m]));
    for (const [mid, m] of wantM) {
      const prev = wasM.get(mid);
      if (prev && muteEq(prev, m)) continue; // the op didn't touch it
      const i = next.mutes.findIndex((x) => x.id === mid);
      if (i >= 0) next.mutes[i] = { ...m, id: mid };
      else next.mutes.push({ ...m, id: mid });
    }
    for (const mid of wasM.keys()) if (!wantM.has(mid)) next.mutes = next.mutes.filter((x) => x.id !== mid);
    out = out.map((s) => (s.id === id ? next : s));
  }
  return out;
}

/**
 * Edit the song list optimistically. `fn` receives a deep copy of the current songs and returns
 * the new list; it may run again on a fresher state (CONFLICT), so it must be a pure edit.
 */
export function editSongs(id: string, label: string, fn: (songs: Song[]) => Song[]): Promise<void> {
  const p = getProject(id);
  if (!p) return Promise.resolve();
  const before = p.songs;
  const after = fn(clone(before));
  const [hb, ha] = touched(before, after);
  if (hb.size === 0) return Promise.resolve();
  const entry: HistoryEntry = { label, before: hb, after: ha };
  useProjects.setState((s) => {
    const h = s.history[id] ?? { undo: [], redo: [] };
    return { history: { ...s.history, [id]: { undo: [...h.undo.slice(-99), entry], redo: [] } } };
  });
  return enqueue(id, { label, apply: fn, history: entry });
}

/** Mutate a single song (ids are resolved at sync time). */
export function editSong(id: string, songId: string, label: string, fn: (s: Song) => void): Promise<void> {
  return editSongs(id, label, (songs) => {
    const sid = rid(id, songId);
    const s = songs.find((x) => x.id === sid || x.id === songId);
    if (s) fn(s);
    return songs;
  });
}

export function undo(id: string): string | null {
  const h = useProjects.getState().history[id];
  if (!h || h.undo.length === 0) return null;
  const last = h.undo[h.undo.length - 1];
  useProjects.setState((s) => ({ history: { ...s.history, [id]: { undo: h.undo.slice(0, -1), redo: [...h.redo, last] } } }));
  void enqueue(id, { label: `되돌리기: ${last.label}`, apply: (songs) => restore(id, last.before, last.after, songs) });
  return last.label;
}

export function redo(id: string): string | null {
  const h = useProjects.getState().history[id];
  if (!h || h.redo.length === 0) return null;
  const next = h.redo[h.redo.length - 1];
  useProjects.setState((s) => ({ history: { ...s.history, [id]: { undo: [...h.undo, next], redo: h.redo.slice(0, -1) } } }));
  void enqueue(id, { label: `다시 하기: ${next.label}`, apply: (songs) => restore(id, next.after, next.before, songs) });
  return next.label;
}

/** After server-side bulk changes (다시 찾기, 세트리스트) older history no longer applies. */
export function clearHistory(id: string) {
  useProjects.setState((s) => ({ history: { ...s.history, [id]: { undo: [], redo: [] } } }));
}

export async function updateProjectSettings(id: string, patch: Partial<Settings>) {
  const p = engineState[id] ?? getProject(id);
  if (!p) return;
  const settings = { ...p.settings, ...patch };
  useProjects.setState((s) => ({ projects: { ...s.projects, [id]: { ...s.projects[id], settings } } }));
  try {
    setEngine(await call("project.update_settings", { id, settings }));
  } catch (e) {
    const f = friendly(e);
    toast(f.title, { kind: "error" });
    setEngine(await call("project.get", { id }));
  }
}

// ── app-side per-project state (persisted in userData/state.json) ──
const EMPTY_UI: ProjectUi = {};

async function loadUi(p: Project) {
  const id = p.id;
  if (useProjects.getState().ui[id]?.uid === p.uid) return;
  let v = await window.cliper.getState<ProjectUi>(`projectui:${p.uid}`);
  if (!v) {
    // one-time import of what older versions kept by id: lastSongId / hints stay app-side,
    // listened / videoSongs move into the project (Song.listened / Song.video_wanted)
    const old = await window.cliper.getState<LegacyUi>(`project:${id}`);
    v = { uid: p.uid, lastSongId: old?.lastSongId, firstExportHintShown: old?.firstExportHintShown };
    if (old) {
      await window.cliper.setState(`projectui:${p.uid}`, v);
      await window.cliper.setState(`project:${id}`, null);
      void importLegacy(p, old);
    }
  }
  useProjects.setState((s) => ({ ui: { ...s.ui, [id]: { ...EMPTY_UI, ...v, uid: p.uid } } }));
}

async function importLegacy(p: Project, old: LegacyUi) {
  for (const s of p.songs) {
    const patch: Partial<Song> = {};
    if (old.listened?.includes(s.id) && !s.listened) patch.listened = true;
    if (old.videoSongs?.includes(s.id) && !s.video_wanted) patch.video_wanted = true;
    if (Object.keys(patch).length) await call("song.update", { id: p.id, song_id: s.id, patch }).catch(() => {});
  }
}

/** "Listened in 결과" (export pre-check), written into the project (protocol 4). */
export function markListened(pid: string, songId: string) {
  const s = getProject(pid)?.songs.find((x) => x.id === songId);
  if (!s || s.listened) return;
  void call("song.update", { id: pid, song_id: songId, patch: { listened: true } }).then(setEngine).catch(() => {});
}

export function projectUi(id: string): ProjectUi {
  return useProjects.getState().ui[id] ?? EMPTY_UI;
}

export function setProjectUi(id: string, patch: Partial<ProjectUi>) {
  const next = { ...projectUi(id), ...patch };
  useProjects.setState((s) => ({ ui: { ...s.ui, [id]: next } }));
  if (next.uid) void window.cliper.setState(`projectui:${next.uid}`, next);
}

export function forgetProject(id: string) {
  delete engineState[id];
  useProjects.setState((s) => {
    const projects = { ...s.projects };
    delete projects[id];
    return { projects, summaries: s.summaries.filter((x) => x.id !== id) };
  });
  void window.cliper.setState(`project:${id}`, null);
}
