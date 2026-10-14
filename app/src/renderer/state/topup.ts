// Boundary top-ups (기획서 14.4), app-wide: watches every loaded project's song ranges (not only
// the open workspace, review A-9), and 3 s after the last edit of a project applies the pure
// rule `planTopUp` (lib/coverage.ts). Request ids are remembered per song so shrinking a song
// cancels only that song's own unstarted top-ups (U-14).
import type { Project } from "@ipc/generated/Project";
import { AUTO_TOPUP_MAX_SEC, marginOf, planTopUp, vodGone, type Range } from "@/lib/coverage";
import { estimate, fetchVideo } from "@/lib/video";
import { call } from "@/lib/rpc";
import { useProjects, getProject, onProjectReset } from "./projects";
import { useApp } from "./app";
import { videoOf } from "./video";
import { useWs } from "./ws";

const DELAY_MS = 3000;

/** per project: last seen ranges, the first range of each song changed in this batch, the timer */
const watch = new Map<string, { ranges: Map<string, Range>; changed: Map<string, Range>; timer: ReturnType<typeof setTimeout> | null }>();
/** song id → top-up request ids this app created for it */
const ownTopUps = new Map<string, Set<string>>();

function observe(p: Project) {
  let w = watch.get(p.id);
  if (!w) {
    // first sight (project opened): just remember the ranges
    watch.set(p.id, { ranges: new Map(p.songs.map((s) => [s.id, [s.start, s.end]])), changed: new Map(), timer: null });
    return;
  }
  let any = false;
  for (const s of p.songs) {
    const r = w.ranges.get(s.id);
    if (r && (Math.abs(r[0] - s.start) > 0.01 || Math.abs(r[1] - s.end) > 0.01)) {
      if (!w.changed.has(s.id)) w.changed.set(s.id, r);
      any = true;
    }
    w.ranges.set(s.id, [s.start, s.end]);
  }
  if (!any) return;
  if (w.timer) clearTimeout(w.timer);
  const pid = p.id;
  w.timer = setTimeout(() => void flush(pid), DELAY_MS);
}

/** Run the pending top-ups of a project now (also used by tests via the QA handle). */
export async function flush(pid: string) {
  const w = watch.get(pid);
  if (!w) return;
  if (w.timer) clearTimeout(w.timer);
  w.timer = null;
  const items = [...w.changed.entries()];
  w.changed.clear();
  const p = getProject(pid);
  const vs = videoOf(pid);
  if (!p || !vs || vs.local || vodGone(p)) return;
  const m = marginOf(p);
  const auto = useApp.getState().settings.autoTopUp;
  for (const [id, was] of items) {
    const s = p.songs.find((x) => x.id === id);
    if (!s) continue;
    const own = [...(ownTopUps.get(id) ?? [])];
    const plan = planTopUp({ song: s, was, margin: m, vs, ownRequests: own });
    for (const rid of plan.cancel) {
      await call("video.cancel", { id: pid, request_id: rid }).catch(() => {});
      ownTopUps.get(id)?.delete(rid);
    }
    if (!auto || !plan.fetch.length) continue; // [영상 맞추기] covers it when auto is off
    const separated = (p.stems ?? []).some((sp) => sp.end > s.start && sp.start < s.end);
    if (plan.sec <= AUTO_TOPUP_MAX_SEC) {
      const r = await fetchVideo(pid, { ranges: plan.fetch, priority: "auto", reason: "topup", process: separated, quiet: true });
      if (r?.request_id) (ownTopUps.get(id) ?? ownTopUps.set(id, new Set()).get(id)!).add(r.request_id);
    } else {
      const est = await estimate(pid, { ranges: plan.fetch });
      if (est && est.missing_sec > 0.5) {
        const ask = { songId: id, ranges: plan.fetch, sec: est.missing_sec, bytes: est.estimated_bytes };
        useWs.setState((st) => ({ topupAsk: [...st.topupAsk.filter((x) => x.songId !== id), ask] }));
      }
    }
  }
}

let wired = false;
/** Start watching (once, at boot). */
export function wireAutoTopUp() {
  if (wired) return;
  wired = true;
  onProjectReset((id) => forgetTopUps(id));
  let last = useProjects.getState().projects;
  useProjects.subscribe((st) => {
    if (st.projects === last) return;
    for (const [id, p] of Object.entries(st.projects)) if (p && p !== last[id]) observe(p);
    last = st.projects;
  });
}

/** A project's cached state was replaced by another folder's (QA-5): start over. */
export function forgetTopUps(pid: string) {
  const w = watch.get(pid);
  if (w?.timer) clearTimeout(w.timer);
  watch.delete(pid);
}
