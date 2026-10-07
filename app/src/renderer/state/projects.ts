// Projects: summaries, full projects (engine is the source of truth via project.changed),
// optimistic song edits with undo/redo, and app-side per-project state.
import { create } from "zustand";
import type { Project } from "@ipc/generated/Project";
import type { ProjectSummary } from "@ipc/generated/ProjectSummary";
import type { Settings } from "@ipc/generated/Settings";
import type { Song } from "@ipc/generated/Song";
import { call } from "@/lib/rpc";
import { friendly } from "@/lib/errors";
import { toast } from "./app";

/** App-only per-project state (not part of the engine's project.json). */
export interface ProjectUi {
  /** songs whose 멘트 지우기 is switched off (export uses the original audio) */
  muteOff: string[];
  /** songs whose result was listened to in 멘트 다듬기 (export pre-check) */
  listened: string[];
  lastSongId?: string;
  firstExportHintShown?: boolean;
}

interface History {
  undo: { songs: Song[]; label: string }[];
  redo: { songs: Song[]; label: string }[];
}

interface ProjectsStore {
  summaries: ProjectSummary[];
  listLoaded: boolean;
  projects: Record<string, Project>;
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
  setProject(p);
  await loadUi(id);
  return p;
}

function setProject(p: Project) {
  useProjects.setState((s) => ({ projects: { ...s.projects, [p.id]: p } }));
}

/** project.changed: replace, but keep optimistic songs while our edits are in flight. */
export function applyChanged(p: Project) {
  const s = useProjects.getState();
  const pend = s.pending[p.id] ?? 0;
  const cur = s.projects[p.id];
  if (pend > 0 && cur) setProject({ ...p, songs: cur.songs });
  else setProject(p);
  // keep the home list fresh without a round trip
  const idx = s.summaries.findIndex((x) => x.id === p.id);
  const summary: ProjectSummary = {
    id: p.id,
    title: p.source?.title ?? null,
    channel: p.source?.channel ?? null,
    platform: p.source?.platform ?? null,
    duration: p.source?.duration ?? null,
    songs: p.songs.filter((x) => x.enabled).length,
    exported: p.songs.filter((x) => x.enabled && x.export).length,
    thumbnail: p.source?.catalog.thumbnail ?? null,
    updated_at: p.updated_at,
  };
  const summaries = idx >= 0 ? s.summaries.map((x, i) => (i === idx ? summary : x)) : [summary, ...s.summaries];
  summaries.sort((a, b) => b.updated_at.localeCompare(a.updated_at));
  useProjects.setState({ summaries });
}

export function getProject(id: string): Project | undefined {
  return useProjects.getState().projects[id];
}

function clone<T>(v: T): T {
  return structuredClone(v);
}

async function sendSongs(id: string, songs: Song[]) {
  useProjects.setState((s) => ({ pending: { ...s.pending, [id]: (s.pending[id] ?? 0) + 1 } }));
  try {
    const res = await call("songs.set", { id, songs });
    const left = (useProjects.getState().pending[id] ?? 1) - 1;
    useProjects.setState((s) => ({ pending: { ...s.pending, [id]: left } }));
    if (left === 0) setProject(res);
  } catch (e) {
    const left = (useProjects.getState().pending[id] ?? 1) - 1;
    useProjects.setState((s) => ({ pending: { ...s.pending, [id]: left } }));
    const f = friendly(e);
    toast(`${f.title} ${f.hint ?? ""}`.trim(), { kind: "error" });
    try {
      setProject(await call("project.get", { id }));
    } catch {
      /* engine down: keep local */
    }
    throw e;
  }
}

/**
 * Edit the song list optimistically. `fn` receives a deep copy and returns the new list.
 * Every edit is undoable (Ctrl+Z).
 */
export function editSongs(id: string, label: string, fn: (songs: Song[]) => Song[]): Promise<void> {
  const p = getProject(id);
  if (!p) return Promise.resolve();
  const before = p.songs;
  const after = fn(clone(before));
  setProject({ ...p, songs: after });
  useProjects.setState((s) => {
    const h = s.history[id] ?? { undo: [], redo: [] };
    return { history: { ...s.history, [id]: { undo: [...h.undo.slice(-99), { songs: before, label }], redo: [] } } };
  });
  return sendSongs(id, after).catch(() => {});
}

/** Mutate a single song. */
export function editSong(id: string, songId: string, label: string, fn: (s: Song) => void): Promise<void> {
  return editSongs(id, label, (songs) => {
    const s = songs.find((x) => x.id === songId);
    if (s) fn(s);
    return songs;
  });
}

/** Restore editable fields from a snapshot, keep engine-owned fields from the current state. */
function mergeSnapshot(snapshot: Song[], current: Song[]): Song[] {
  const byId = new Map(current.map((s) => [s.id, s]));
  return snapshot.map((s) => {
    const cur = byId.get(s.id);
    return cur ? { ...clone(s), clip_id: cur.clip_id, export: cur.export, confidence: cur.confidence ?? s.confidence } : clone(s);
  });
}

export function undo(id: string): string | null {
  const p = getProject(id);
  const h = useProjects.getState().history[id];
  if (!p || !h || h.undo.length === 0) return null;
  const last = h.undo[h.undo.length - 1];
  const restored = mergeSnapshot(last.songs, p.songs);
  useProjects.setState((s) => ({
    history: { ...s.history, [id]: { undo: h.undo.slice(0, -1), redo: [...h.redo, { songs: p.songs, label: last.label }] } },
  }));
  setProject({ ...p, songs: restored });
  void sendSongs(id, restored).catch(() => {});
  return last.label;
}

export function redo(id: string): string | null {
  const p = getProject(id);
  const h = useProjects.getState().history[id];
  if (!p || !h || h.redo.length === 0) return null;
  const next = h.redo[h.redo.length - 1];
  const restored = mergeSnapshot(next.songs, p.songs);
  useProjects.setState((s) => ({
    history: { ...s.history, [id]: { undo: [...h.undo, { songs: p.songs, label: next.label }], redo: h.redo.slice(0, -1) } },
  }));
  setProject({ ...p, songs: restored });
  void sendSongs(id, restored).catch(() => {});
  return next.label;
}

export async function updateProjectSettings(id: string, patch: Partial<Settings>) {
  const p = getProject(id);
  if (!p) return;
  const settings = { ...p.settings, ...patch };
  setProject({ ...p, settings });
  try {
    setProject(await call("project.update_settings", { id, settings }));
  } catch (e) {
    const f = friendly(e);
    toast(f.title, { kind: "error" });
    setProject(await call("project.get", { id }));
  }
}

// ── app-side per-project state (persisted in userData/state.json) ──
const EMPTY_UI: ProjectUi = { muteOff: [], listened: [] };

async function loadUi(id: string) {
  if (useProjects.getState().ui[id]) return;
  const v = (await window.cliper.getState<ProjectUi>(`project:${id}`)) ?? EMPTY_UI;
  useProjects.setState((s) => ({ ui: { ...s.ui, [id]: { ...EMPTY_UI, ...v } } }));
}

export function projectUi(id: string): ProjectUi {
  return useProjects.getState().ui[id] ?? EMPTY_UI;
}

export function setProjectUi(id: string, patch: Partial<ProjectUi>) {
  const next = { ...projectUi(id), ...patch };
  useProjects.setState((s) => ({ ui: { ...s.ui, [id]: next } }));
  void window.cliper.setState(`project:${id}`, next);
}

export function forgetProject(id: string) {
  useProjects.setState((s) => {
    const projects = { ...s.projects };
    delete projects[id];
    return { projects, summaries: s.summaries.filter((x) => x.id !== id) };
  });
  void window.cliper.setState(`project:${id}`, null);
}
