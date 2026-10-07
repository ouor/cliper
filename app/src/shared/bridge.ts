// Contract of the preload bridge (window.cliper). Shared by main, preload and renderer.
import type { AppSettings } from "./settings";

export type EngineState = "stopped" | "starting" | "ready" | "restarting" | "failed" | "no-root";

export interface EngineStatus {
  state: EngineState;
  /** engine.ready params.system (null until ready) */
  system: unknown | null;
  /** user-facing reason when crashed/failed */
  reason?: string;
  /** last stderr lines (for [자세히]) */
  detail?: string;
  restarts: number;
  projectsRoot: string | null;
  /** engine.ready protocol version */
  protocol?: number;
}

export interface RpcFailure {
  /** JSON-RPC code (-32000 …) or 0 for transport problems */
  code: number;
  message: string;
  /** engine ApiError code (BUSY, NETWORK, …) or ENGINE_DOWN */
  engineCode: string;
}

export type CallResult = { ok: true; result: unknown } | { ok: false; error: RpcFailure };

export interface DriveInfo {
  path: string;
  free: number;
  total: number;
}

export interface FolderCheck {
  ok: boolean;
  free: number;
  total: number;
  reason?: string;
}

export type CloseAction = "minimize" | "stop" | "cancel";

export interface CliperBridge {
  call(method: string, params: unknown): Promise<CallResult>;
  onNotify(cb: (method: string, params: unknown) => void): () => void;
  engineStatus(): Promise<EngineStatus>;
  onEngineStatus(cb: (s: EngineStatus) => void): () => void;
  restartEngine(): Promise<EngineStatus>;

  getSettings(): Promise<AppSettings>;
  setSettings(patch: Partial<AppSettings>): Promise<AppSettings>;
  getState<T = unknown>(key: string): Promise<T | null>;
  setState(key: string, value: unknown): Promise<void>;

  pickFolder(defaultPath?: string): Promise<string | null>;
  pickFile(opts: { title?: string; filters?: { name: string; extensions: string[] }[] }): Promise<string | null>;
  pickSaveFile(opts: { title?: string; defaultPath?: string; filters?: { name: string; extensions: string[] }[] }): Promise<string | null>;
  readTextFile(path: string): Promise<string>;
  pathForFile(file: File): string;

  openPath(path: string): Promise<void>;
  showItem(path: string): Promise<void>;
  openExternal(url: string): Promise<void>;

  drives(): Promise<DriveInfo[]>;
  checkFolder(path: string): Promise<FolderCheck>;
  dirSize(projectId: string): Promise<{ total: number; clips: number }>;
  logDir(): Promise<string>;

  notify(title: string, body: string): Promise<void>;
  isFocused(): Promise<boolean>;
  onCloseRequest(cb: () => void): () => void;
  answerClose(action: CloseAction): void;
  setBusy(busy: boolean): void;

  platform: string;
  versions: { electron: string; chrome: string; node: string };
}
