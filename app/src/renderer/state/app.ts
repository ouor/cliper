// App-level UI state: engine status, settings, navigation, overlays, toasts, dialogs.
import { create } from "zustand";
import type { EngineStatus } from "../../shared/bridge";
import { DEFAULT_SETTINGS, type AppSettings } from "../../shared/settings";
import type { SystemInfo } from "@ipc/generated/SystemInfo";

export type WorkspaceMode = "review" | "talk" | "export";
export type Route = { name: "home" } | { name: "workspace"; projectId: string; mode: WorkspaceMode };
export type SettingsTab = "general" | "detect" | "video" | "talk" | "export" | "keys" | "advanced";

export interface Toast {
  id: number;
  kind: "ok" | "info" | "warn" | "error";
  text: string;
  action?: { label: string; run: () => void };
  ttl?: number;
}

export interface DialogButton {
  id: string;
  label: string;
  kind?: "primary" | "danger" | "ghost" | "subtle";
  hotkey?: string;
}
export interface DialogSpec {
  title: string;
  body?: string | string[];
  icon?: "warn" | "info" | "error" | "question";
  buttons: DialogButton[];
  detail?: string;
}

interface AppStore {
  engine: EngineStatus;
  system: SystemInfo | null;
  settings: AppSettings;
  settingsLoaded: boolean;
  route: Route;
  overlay: null | "settings" | "help" | "cookies" | "setlist";
  settingsTab: SettingsTab;
  jobsOpen: boolean;
  toasts: Toast[];
  dialog: (DialogSpec & { resolve: (id: string) => void }) | null;
}

export const useApp = create<AppStore>(() => ({
  engine: { state: "starting", system: null, restarts: 0, projectsRoot: null },
  system: null,
  settings: DEFAULT_SETTINGS,
  settingsLoaded: false,
  route: { name: "home" },
  overlay: null,
  settingsTab: "general",
  jobsOpen: false,
  toasts: [],
  dialog: null,
}));

export async function updateSettings(patch: Partial<AppSettings>) {
  useApp.setState((s) => ({ settings: { ...s.settings, ...patch } }));
  const saved = await window.cliper.setSettings(patch);
  useApp.setState({ settings: saved });
}

let toastId = 0;
export function toast(text: string, opts: Partial<Omit<Toast, "id" | "text">> = {}) {
  const t: Toast = { id: ++toastId, kind: opts.kind ?? "info", text, action: opts.action, ttl: opts.ttl ?? (opts.action ? 8000 : 4000) };
  useApp.setState((s) => ({ toasts: [...s.toasts.slice(-3), t] }));
  setTimeout(() => dismissToast(t.id), t.ttl);
}
export function dismissToast(id: number) {
  useApp.setState((s) => ({ toasts: s.toasts.filter((t) => t.id !== id) }));
}

export function ask(spec: DialogSpec): Promise<string> {
  return new Promise((resolve) => {
    useApp.setState({
      dialog: {
        ...spec,
        resolve: (id) => {
          useApp.setState({ dialog: null });
          resolve(id);
        },
      },
    });
  });
}

export function go(route: Route) {
  useApp.setState({ route });
}

export function setMode(mode: WorkspaceMode) {
  const r = useApp.getState().route;
  if (r.name === "workspace") useApp.setState({ route: { ...r, mode } });
}

export function openSettings(tab: SettingsTab = "general") {
  useApp.setState({ overlay: "settings", settingsTab: tab });
}

export function applyTheme(theme: AppSettings["theme"]) {
  const dark = theme === "dark" || (theme === "system" && window.matchMedia("(prefers-color-scheme: dark)").matches);
  document.documentElement.dataset.theme = dark ? "dark" : "light";
}
