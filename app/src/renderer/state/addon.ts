// CUDA runtime add-on (GPU 가속 구성요소). Release builds ship CPU-only; on a PC with an NVIDIA
// GPU and no usable CUDA runtime the engine reports `system.info.cuda_addon.needed`, and the app
// installs the add-on in the background (engine job `addon.install_cuda`, project "_system"):
// download → SHA-256 check → extract to %LOCALAPPDATA%\Cliper\addons\cuda-v1. Everything keeps
// working on CPU meanwhile. When it is installed the engine is restarted (once no project job is
// running) so it picks the folder up, then `system.probe_gpu` confirms CUDA.
//
// The IPC types are not generated yet (engine side lands separately), so this module carries
// its own minimal types and goes through `callAny`.
import { create } from "zustand";
import { call, EngineError, onNotification } from "@/lib/rpc";
import { useApp, toast } from "@/state/app";
import { useJobs } from "@/state/jobs";
import type { Method } from "@ipc/protocol";
import { friendly } from "@/lib/errors";
import type { ApiError } from "@ipc/generated/ApiError";

export const CUDA_JOB = "addon.install_cuda";
export const SYSTEM_PROJECT = "_system";

export interface CudaAddonInfo {
  version: number | string;
  url: string;
  size: number;
  sha256: string;
  dir: string;
  installed: boolean;
  needed: boolean;
}

export type AddonPhase = "idle" | "download" | "extract" | "restart_wait" | "restarting" | "probing" | "done" | "failed" | "cancelled";

interface AddonState {
  phase: AddonPhase;
  jobId: string | null;
  done: number;
  total: number;
  error: string | null;
  /** auto-install already tried in this app session (a cancel is respected until the next launch) */
  autoTried: boolean;
}

export const useAddon = create<AddonState>(() => ({ phase: "idle", jobId: null, done: 0, total: 0, error: null, autoTried: false }));

// eslint-disable-next-line @typescript-eslint/no-explicit-any
const callAny = (method: string, params: Record<string, unknown>) => call(method as Method, params as never) as Promise<any>;

export function cudaAddon(): CudaAddonInfo | null {
  const sys = useApp.getState().system as unknown as { cuda_addon?: CudaAddonInfo } | null;
  return sys?.cuda_addon ?? null;
}

const running = (p: AddonPhase) => p === "download" || p === "extract";

/** Start (or retry) the install. Safe to call repeatedly. */
export async function installCudaAddon(opts: { reinstall?: boolean } = {}) {
  const st = useAddon.getState();
  if (running(st.phase) || st.phase === "restarting" || st.phase === "probing") return;
  const info = cudaAddon();
  if (!info) return;
  useAddon.setState({ phase: "download", jobId: null, done: 0, total: info.size, error: null, autoTried: true });
  try {
    if (opts.reinstall && info.installed) await callAny("addon.remove_cuda", {});
    const r = await callAny(CUDA_JOB, {});
    useAddon.setState({ jobId: r.job_id });
  } catch (e) {
    const busy = e instanceof EngineError && e.engineCode === "BUSY";
    // BUSY: an install is already running (e.g. started before a renderer reload) — follow it
    if (busy) return;
    useAddon.setState({ phase: "failed", error: e instanceof Error ? e.message : String(e) });
  }
}

export async function cancelCudaAddon() {
  const id = useAddon.getState().jobId;
  if (!id) return;
  try {
    await call("job.cancel", { job_id: id });
  } catch {
    /* already finished */
  }
}

export async function removeCudaAddon() {
  await callAny("addon.remove_cuda", {});
  await refreshSystem();
  useAddon.setState({ phase: "idle" });
  // the engine still has the DLLs loaded: restart so it runs on CPU (or another CUDA) again
  await restartWhenIdle();
}

/** Auto-install when the engine says the add-on is needed (once per app session). */
export function maybeAutoInstall(opts: { firstRun?: boolean } = {}) {
  const info = cudaAddon();
  const st = useAddon.getState();
  if (!info?.needed || info.installed || st.autoTried || st.phase !== "idle") return;
  // during the first-run wizard this starts at the PC check step (FirstRun calls with firstRun)
  if (!opts.firstRun && !useApp.getState().settings.firstRunDone) return;
  if (useApp.getState().settings.device === "cpu") return; // user chose CPU explicitly
  void installCudaAddon();
}

async function refreshSystem() {
  try {
    useApp.setState({ system: await call("system.info", {}) });
  } catch {
    /* engine down: status handler refreshes later */
  }
}

let restartUnsub: (() => void) | null = null;
const projectJobsRunning = () => Object.values(useJobs.getState().jobs).some((j) => j.status === "running" && j.projectId !== SYSTEM_PROJECT) || useJobs.getState().waiting.length > 0;

/** Restart the engine now, or as soon as no project job is running (restarting would stop them). */
async function restartWhenIdle() {
  if (!projectJobsRunning()) return restartNow();
  useAddon.setState({ phase: "restart_wait" });
  restartUnsub?.();
  restartUnsub = useJobs.subscribe(() => {
    if (projectJobsRunning()) return;
    restartUnsub?.();
    restartUnsub = null;
    void restartNow();
  });
}

export async function restartNow() {
  restartUnsub?.();
  restartUnsub = null;
  useAddon.setState({ phase: "restarting" });
  await window.cliper.restartEngine();
}

/** Engine became ready (App's status handler): finish a pending restart, or consider auto-install. */
export async function onEngineReady() {
  const st = useAddon.getState();
  if (st.phase === "restarting") {
    useAddon.setState({ phase: "probing" });
    try {
      const info = await call("system.probe_gpu", {});
      useApp.setState({ system: info });
      const ok = !!info.gpu?.cuda_ok;
      useAddon.setState({ phase: ok ? "done" : "failed", error: ok ? null : info.gpu?.reason ?? "그래픽카드 확인에 실패했어요" });
      toast(ok ? "GPU 가속을 켰어요. 멘트 지우기가 빨라져요." : "GPU 가속 구성요소를 설치했지만 그래픽카드를 쓸 수 없어요. CPU로 계속 처리해요.", { kind: ok ? "ok" : "error" });
    } catch (e) {
      useAddon.setState({ phase: "failed", error: e instanceof Error ? e.message : String(e) });
    }
    return;
  }
  maybeAutoInstall();
}

export function wireAddon() {
  onNotification("job.started", (p) => {
    if ((p.kind as string) !== CUDA_JOB) return;
    useAddon.setState({ jobId: p.job_id, phase: "download", error: null, autoTried: true });
  });
  onNotification("job.progress", (p) => {
    if ((p.kind as string) !== CUDA_JOB) return;
    const pr = p.progress;
    const extract = pr.stage === "addon.extract";
    useAddon.setState({
      jobId: p.job_id,
      phase: extract ? "extract" : "download",
      done: extract ? pr.done : pr.bytes ?? pr.done,
      total: pr.total || useAddon.getState().total,
    });
  });
  onNotification("job.done", (p) => {
    if ((p.kind as string) !== CUDA_JOB) return;
    useAddon.setState({ jobId: null });
    void refreshSystem().then(() => restartWhenIdle());
  });
  onNotification("job.failed", (p) => {
    if ((p.kind as string) !== CUDA_JOB) return;
    const cancelled = p.error.code === "CANCELLED";
    useAddon.setState({ jobId: null, phase: cancelled ? "cancelled" : "failed", error: cancelled ? null : friendly(p.error as ApiError).title });
  });
}

/** One-line status for banners / diagnostics. */
export function addonLine(st: AddonState, info: CudaAddonInfo | null): string {
  switch (st.phase) {
    case "download":
      return `GPU 가속 구성요소 받는 중 ${fmtMB(st.done)} / ${fmtMB(st.total || info?.size || 0)}`;
    case "extract":
      return "GPU 가속 구성요소 설치 중…";
    case "restart_wait":
      return "설치를 마쳤어요. 진행 중인 작업이 끝나면 엔진을 다시 시작해 GPU를 켜요.";
    case "restarting":
      return "엔진을 다시 시작하는 중…";
    case "probing":
      return "그래픽카드로 처리할 수 있는지 확인하는 중…";
    case "done":
      return "GPU 가속 구성요소 설치됨";
    case "failed":
      return `GPU 가속 구성요소를 설치하지 못했어요${st.error ? ` · ${st.error}` : ""}`;
    case "cancelled":
      return "GPU 가속 구성요소 설치를 취소했어요. CPU로 처리해요.";
    default:
      return info?.installed ? "GPU 가속 구성요소 설치됨" : info?.needed ? "GPU 가속 구성요소가 필요해요" : "필요 없음";
  }
}

export const fmtMB = (b: number) => `${Math.round(b / 1024 / 1024)}MB`;
