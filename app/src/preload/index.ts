// Sandboxed preload: exposes a narrow, typed bridge. No Node APIs reach the renderer.
import { contextBridge, ipcRenderer, webUtils } from "electron";
import type { CliperBridge, EngineStatus } from "../shared/bridge";

function subscribe<T extends unknown[]>(channel: string, cb: (...args: T) => void): () => void {
  const h = (_e: Electron.IpcRendererEvent, ...args: unknown[]) => cb(...(args as T));
  ipcRenderer.on(channel, h);
  return () => ipcRenderer.removeListener(channel, h);
}

const bridge: CliperBridge = {
  call: (method, params) => ipcRenderer.invoke("engine:call", String(method), params ?? {}),
  onNotify: (cb) => subscribe<[string, unknown]>("engine:notify", cb),
  engineStatus: () => ipcRenderer.invoke("engine:status"),
  onEngineStatus: (cb) => subscribe<[EngineStatus]>("engine:status", cb),
  restartEngine: () => ipcRenderer.invoke("engine:restart"),

  getSettings: () => ipcRenderer.invoke("settings:get"),
  setSettings: (patch) => ipcRenderer.invoke("settings:set", patch),
  getState: (key) => ipcRenderer.invoke("state:get", String(key)),
  setState: (key, value) => ipcRenderer.invoke("state:set", String(key), value),

  pickFolder: (defaultPath) => ipcRenderer.invoke("dialog:folder", defaultPath),
  pickFile: (opts) => ipcRenderer.invoke("dialog:file", opts),
  pickSaveFile: (opts) => ipcRenderer.invoke("dialog:save", opts),
  readTextFile: (path) => ipcRenderer.invoke("dialog:file:read", path),
  pathForFile: (file) => webUtils.getPathForFile(file),

  openPath: (path) => ipcRenderer.invoke("shell:open", path),
  showItem: (path) => ipcRenderer.invoke("shell:show", path),
  openExternal: (url) => ipcRenderer.invoke("shell:external", url),

  drives: () => ipcRenderer.invoke("fs:drives"),
  checkFolder: (path) => ipcRenderer.invoke("fs:check", path),
  dirSize: (projectId) => ipcRenderer.invoke("fs:projectSize", projectId),
  logDir: () => ipcRenderer.invoke("app:logDir"),

  notify: (title, body) => ipcRenderer.invoke("app:notify", title, body),
  isFocused: () => ipcRenderer.invoke("app:focused"),
  onCloseRequest: (cb) => subscribe<[]>("app:close-request", cb),
  answerClose: (action) => ipcRenderer.send("app:close-answer", action),
  setBusy: (busy) => ipcRenderer.send("app:busy", !!busy),

  platform: process.platform,
  versions: { electron: process.versions.electron ?? "", chrome: process.versions.chrome ?? "", node: process.versions.node ?? "" },
};

contextBridge.exposeInMainWorld("cliper", bridge);
