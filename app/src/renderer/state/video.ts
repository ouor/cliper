// Video coverage per project (protocol 3, docs/ipc.md "Video: what is on disk, what is coming").
// Coverage is not in the project: it comes from `video.status` and the throttled
// `video.changed` notification (≤ 4/s per project), plus `video.request_done`.
import { create } from "zustand";
import type { VideoState } from "@ipc/generated/VideoState";
import { call, onNotification } from "@/lib/rpc";
import { toast } from "./app";
import { getProject, onProjectReset } from "./projects";
import { fmtDurationKo } from "@/lib/format";

interface VideoStore {
  byProject: Record<string, VideoState>;
  /** app-wide pause (video.pause), remembered by the engine */
  paused: boolean;
}

export const useVideo = create<VideoStore>(() => ({ byProject: {}, paused: false }));

function put(v: VideoState) {
  useVideo.setState((s) => ({ byProject: { ...s.byProject, [v.project_id]: v }, paused: v.paused }));
}

export async function loadVideo(projectId: string): Promise<VideoState | null> {
  try {
    const v = await call("video.status", { id: projectId });
    put(v);
    return v;
  } catch {
    return null;
  }
}

/** Every loaded project + the pause flag (engine start / restart). */
export async function loadAllVideo() {
  const r = await window.cliper.call("video.status", {});
  if (!r.ok) return;
  const all = r.result as { paused: boolean; projects: Record<string, VideoState> };
  useVideo.setState((s) => ({ byProject: { ...s.byProject, ...all.projects }, paused: all.paused }));
}

export function videoOf(projectId: string): VideoState | undefined {
  return useVideo.getState().byProject[projectId];
}

type DoneListener = (p: { project_id: string; request_id: string; status: string; failed: [number, number][]; code: string | null }) => void;
const doneListeners = new Set<DoneListener>();
export function onRequestDone(cb: DoneListener): () => void {
  doneListeners.add(cb);
  return () => doneListeners.delete(cb);
}

export function wireVideo() {
  // a different folder took this project id over (QA-5 / U-12): drop its coverage and reload
  onProjectReset((id) => {
    useVideo.setState((s) => {
      const byProject = { ...s.byProject };
      delete byProject[id];
      return { byProject };
    });
    void loadVideo(id);
  });
  onNotification("video.changed", (p) => put(p.video));
  onNotification("video.request_done", (p) => {
    for (const l of doneListeners) l(p);
    if (p.status !== "failed") return;
    const req = useVideo.getState().byProject[p.project_id]?.requests.find((r) => r.id === p.request_id);
    if (req?.reason === "topup") return;
    if (p.code === "VOD_GONE") return; // the replay banner says it (project.vod)
    const sec = p.failed.reduce((n, [a, b]) => n + (b - a), 0);
    const pid = p.project_id;
    toast(`영상 ${sec > 0 ? fmtDurationKo(sec) : "일부"}를 받지 못했어요${p.code === "NETWORK" ? " · 연결이 끊겼어요" : ""}. 받은 부분은 그대로예요.`, {
      kind: "warn",
      ttl: 8000,
      action: getProject(pid)?.vod?.status === "gone" ? undefined : { label: "다시 시도", run: () => void call("video.retry", { id: pid, ranges: p.failed.length ? p.failed : null }).catch(() => {}) },
    });
  });
}

export async function setPaused(paused: boolean) {
  useVideo.setState({ paused });
  try {
    await call("video.pause", { paused });
  } catch {
    useVideo.setState({ paused: !paused });
  }
}
