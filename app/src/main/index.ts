import { app, BrowserWindow, dialog, ipcMain, Menu, Notification, protocol, shell, session, type IpcMainEvent, type IpcMainInvokeEvent } from "electron";
import { existsSync, mkdirSync, promises as fsp, readFileSync, renameSync, writeFileSync, accessSync, constants as fsConst, statfsSync, statSync } from "node:fs";
import { dirname, extname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Engine } from "./engine";
import { createMediaHandler, isInside, isProjectId } from "./media";
import { APP_ORIGIN, createAppHandler } from "./app-protocol";
import { DEFAULT_SETTINGS, type AppSettings } from "../shared/settings";
import type { CloseAction, DriveInfo, FolderCheck } from "../shared/bridge";

const __dirname = dirname(fileURLToPath(import.meta.url));
/** development build (electron-vite dev / preview from the repo); env overrides only apply here */
const DEV = !app.isPackaged;

// ── one instance per user: two engines must never share a projects root ──
if (!app.requestSingleInstanceLock()) {
  app.quit();
  process.exit(0);
}

protocol.registerSchemesAsPrivileged([
  { scheme: "app", privileges: { standard: true, secure: true, supportFetchAPI: true, corsEnabled: true } },
  { scheme: "cliper-media", privileges: { standard: true, secure: true, stream: true, supportFetchAPI: true, corsEnabled: true } },
]);

// ── paths ──
const appRoot = app.getAppPath(); // …/app in dev and preview
const repoRoot = resolve(appRoot, "..");
const env = (name: string) => (DEV ? process.env[name] : undefined);
function firstExisting(...paths: (string | undefined)[]): string {
  for (const p of paths) if (p && existsSync(p)) return p;
  return paths.find(Boolean) as string;
}
// Packaged builds use only what ships in resources/ (layout: app/README.md "패키징");
// dev uses the repo checkout, overridable through env.
const RES = app.isPackaged ? process.resourcesPath : "";
const ENGINE_EXE = app.isPackaged ? join(RES, "engine", "cliper.exe") : firstExisting(env("CLIPER_ENGINE"), join(repoRoot, "core", "target", "release", "cliper.exe"));
const MODELS_DIR = app.isPackaged ? join(RES, "models") : firstExisting(env("CLIPER_MODELS"), join(repoRoot, "models"));
const ENGINE_ENV: Record<string, string> = {};
/** where the engine installs add-ons (CUDA runtime); only opened in Explorer from the app */
const ADDONS_DIR = process.env.LOCALAPPDATA ? join(process.env.LOCALAPPDATA, "Cliper", "addons") : undefined;
if (app.isPackaged) {
  // ffmpeg/ffprobe ship side by side in resources/ffmpeg (the engine looks for ffprobe next to ffmpeg);
  // the CUDA runtime in resources/engine/cuda (the engine also finds <exe dir>/cuda by itself)
  const ff = join(RES, "ffmpeg", "ffmpeg.exe");
  if (existsSync(ff)) ENGINE_ENV.CLIPER_FFMPEG = ff;
  const cuda = join(RES, "engine", "cuda");
  if (existsSync(cuda)) ENGINE_ENV.CLIPER_CUDA_DIR = cuda;
}

const DEV_URL = env("ELECTRON_RENDERER_URL");
const ENTRY_URL = DEV_URL ?? `${APP_ORIGIN}/index.html`;
const ENTRY_ORIGIN = originOf(ENTRY_URL);
function originOf(url: string): string {
  try {
    const u = new URL(url);
    return `${u.protocol}//${u.host}`; // URL.origin is "null" for custom schemes
  } catch {
    return "";
  }
}

// ── settings & small state store (userData) ──
if (env("CLIPER_USER_DATA")) app.setPath("userData", resolve(env("CLIPER_USER_DATA")!));
const userData = app.getPath("userData");
mkdirSync(userData, { recursive: true });
const SETTINGS_FILE = join(userData, "settings.json");
const STATE_FILE = join(userData, "state.json");
const LOG_DIR = join(userData, "logs");

function readJson<T>(file: string, fallback: T): T {
  try {
    return JSON.parse(readFileSync(file, "utf8")) as T;
  } catch {
    return fallback;
  }
}
/** temp file + rename: a crash mid-write never leaves a truncated settings file */
function writeAtomic(file: string, text: string) {
  const tmp = `${file}.tmp`;
  writeFileSync(tmp, text);
  renameSync(tmp, file);
}
const stored = readJson<Partial<AppSettings> & { defaultsRev?: number }>(SETTINGS_FILE, {});
// defaults changed after a settings file was written: 1 = video 1080p (was 최고 화질)
if ((stored.defaultsRev ?? 0) < 1 && stored.videoHeight == null) delete stored.videoHeight;
let settings: AppSettings = { ...DEFAULT_SETTINGS, ...stored };
(settings as AppSettings & { defaultsRev?: number }).defaultsRev = 1;
if (env("CLIPER_PROJECTS")) {
  settings.projectsRoot = resolve(env("CLIPER_PROJECTS")!);
  settings.firstRunDone = settings.firstRunDone || env("CLIPER_SKIP_FIRST_RUN") === "1";
}
let state: Record<string, unknown> = readJson(STATE_FILE, {});
let stateTimer: NodeJS.Timeout | null = null;
function saveSettings() {
  writeAtomic(SETTINGS_FILE, JSON.stringify(settings, null, 2));
}
function flushState() {
  if (stateTimer) clearTimeout(stateTimer);
  stateTimer = null;
  try {
    writeAtomic(STATE_FILE, JSON.stringify(state));
  } catch {
    /* ignore */
  }
}
function saveStateSoon() {
  if (stateTimer) return;
  stateTimer = setTimeout(flushState, 300);
}

// ── engine ──
const engine = new Engine({ exe: ENGINE_EXE, models: MODELS_DIR, logDir: LOG_DIR, preferCpu: settings.device === "cpu", lowPriority: settings.lowPriority, env: ENGINE_ENV });
let win: BrowserWindow | null = null;
let busy = false;
let quitting = false;

/** Never touch a destroyed window (quit path, late notifications). */
function send(channel: string, ...args: unknown[]) {
  if (win && !win.isDestroyed() && !win.webContents.isDestroyed()) win.webContents.send(channel, ...args);
}

// paths the renderer may open: outputs the engine reported, files the user picked or saved
const knownOutputDirs = new Set<string>();
const savedPaths = new Set<string>();
const pickedFolders = new Set<string>();
const pickedFiles = new Set<string>();
function noteProject(p: unknown) {
  const dir = (p as { settings?: { output_dir?: string | null } } | null)?.settings?.output_dir;
  if (typeof dir === "string" && dir) knownOutputDirs.add(resolve(dir));
}
// project id → its folder (ProjectSummary.dir): projects may live outside the projects folder
// (Project locations). Filled from project.list; refreshed on a miss and after location changes.
const projectDirs = new Map<string, string>();
/** previous projects folders (settings change) — the only paths projects.add_root accepts besides picked ones */
const previousRoots = new Set<string>();
/** state keys only main writes (security decisions read them); state:set refuses them (U-1) */
const MAIN_ONLY_STATE = "__main.";
const PENDING_ROOTS = MAIN_ONLY_STATE + "pendingAddRoots";
function pendingRoots(): string[] {
  const v = state[PENDING_ROOTS];
  return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
}
function setPendingRoots(list: string[]) {
  if (list.length) state[PENDING_ROOTS] = [...new Set(list)];
  else delete state[PENDING_ROOTS];
  saveStateSoon();
}
/** other locations the engine lists (projects.locations): the renderer may open them in Explorer */
const locationDirs = new Set<string>();
let dirsLoading: Promise<void> | null = null;
function noteSummaries(list: unknown) {
  if (!Array.isArray(list)) return;
  for (const s of list as { id?: unknown; dir?: unknown }[]) if (typeof s?.id === "string" && typeof s.dir === "string" && s.dir) projectDirs.set(s.id, resolve(s.dir));
}
function refreshProjectDirs(): Promise<void> {
  dirsLoading ??= engine
    .call("project.list", {})
    .then((r) => {
      if (r.ok) {
        projectDirs.clear();
        noteSummaries(r.result);
      }
    })
    .finally(() => {
      dirsLoading = null;
    });
  return dirsLoading;
}
async function projectDir(id: string): Promise<string | null> {
  if (!projectDirs.has(id) && engine.status.state === "ready") await refreshProjectDirs();
  return projectDirs.get(id) ?? null;
}
engine.on("status", (s) => {
  if (s.state === "ready") void refreshProjectDirs();
  send("engine:status", s);
});
engine.on("notify", (method: string, params: unknown) => {
  if (method === "project.changed") noteProject((params as { project?: unknown })?.project);
  send("engine:notify", method, params);
});
engine.on("result", (_method: string, result: unknown) => {
  noteProject(result);
  noteProject((result as { project?: unknown } | null)?.project);
});

function createWindow() {
  win = new BrowserWindow({
    width: 1600,
    height: 960,
    minWidth: 1280,
    minHeight: 720,
    backgroundColor: settings.theme === "light" ? "#f5f6f8" : "#0e1014",
    title: "Cliper",
    // window / taskbar icon (the exe's own icon needs resource editing, which the build skips)
    icon: join(appRoot, "resources", "icon.png"),
    autoHideMenuBar: true,
    show: false,
    webPreferences: {
      preload: join(__dirname, "../preload/index.cjs"),
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: true,
      spellcheck: false,
    },
  });
  win.setMenuBarVisibility(false);
  win.once("ready-to-show", () => win?.show());
  win.on("closed", () => {
    win = null;
  });
  // never open windows or navigate (the app is a single page; a dropped file must not replace it)
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https:\/\//.test(url)) void shell.openExternal(url);
    return { action: "deny" };
  });
  win.webContents.on("will-navigate", (e) => e.preventDefault());
  win.webContents.on("will-frame-navigate", (e) => e.preventDefault());
  win.webContents.on("will-redirect", (e) => e.preventDefault());
  win.on("close", (e) => {
    if (quitting || !busy) return;
    if (settings.closeBehavior === "minimize") {
      e.preventDefault();
      win?.minimize();
      return;
    }
    if (settings.closeBehavior === "stop") return;
    e.preventDefault();
    send("app:close-request");
  });
  void win.loadURL(ENTRY_URL);
}

/** Writable + free space, without creating anything: tests the nearest existing ancestor. */
function folderCheck(path: string): FolderCheck {
  try {
    if (typeof path !== "string" || !/^([a-zA-Z]:[\\/]|\\\\)/.test(path)) throw new Error("not an absolute path");
    let dir = resolve(path);
    while (!existsSync(dir)) {
      const up = dirname(dir);
      if (up === dir) throw new Error("no existing parent");
      dir = up;
    }
    accessSync(dir, fsConst.W_OK);
    const st = statfsSync(dir);
    return { ok: true, free: st.bavail * st.bsize, total: st.blocks * st.bsize };
  } catch (e) {
    return { ok: false, free: 0, total: 0, reason: (e as Error).message };
  }
}

async function dirSize(dir: string): Promise<number> {
  let total = 0;
  let entries: import("node:fs").Dirent[];
  try {
    entries = await fsp.readdir(dir, { withFileTypes: true });
  } catch {
    return 0;
  }
  for (const e of entries) {
    const p = join(dir, e.name);
    if (e.isDirectory()) total += await dirSize(p);
    else {
      try {
        total += (await fsp.stat(p)).size;
      } catch {
        /* ignore */
      }
    }
  }
  return total;
}

// ── IPC: only from our own page, only what the UI needs ──

function trusted(e: IpcMainInvokeEvent | IpcMainEvent): boolean {
  return !!win && e.sender === win.webContents && originOf(e.senderFrame?.url ?? "") === ENTRY_ORIGIN;
}
function handle(channel: string, fn: (e: IpcMainInvokeEvent, ...args: any[]) => unknown) {
  ipcMain.handle(channel, (e, ...args) => {
    if (!trusted(e)) throw new Error("untrusted sender");
    return fn(e, ...args);
  });
}
function on(channel: string, fn: (e: IpcMainEvent, ...args: any[]) => void) {
  ipcMain.on(channel, (e, ...args) => {
    if (trusted(e)) fn(e, ...args);
  });
}

const ENGINE_METHODS = new Set([
  "system.info", "system.probe_gpu", "system.set_cookies", "system.cookies_status", "system.check_cookies", "system.set_keep_awake",
  "project.list", "project.create", "project.get", "project.delete", "project.update_settings", "project.export_metadata", "project.set_pinned",
  "project.open", "projects.add_root", "projects.remove_location", "projects.locations",
  "songs.set", "song.update", "song.add", "song.remove", "mute.upsert", "mute.remove", "songs.match_setlist",
  "audio.window", "detect.default_params", "detect.rerun",
  "audio.fetch", "detect.run", "export.run", "stems.ensure",
  // protocol 3: video by VOD range, storage, replay check
  "video.status", "video.fetch", "video.estimate", "video.cancel", "video.prioritize", "video.retry", "video.pause", "video.view",
  "storage.usage", "media.drop", "vod.check", "system.net_stats",
  "job.list", "job.cancel", "queue.add", "queue.list", "queue.cancel", "queue.clear",
  // CUDA runtime add-on (engine downloads from its own fixed URL and verifies SHA-256)
  "addon.install_cuda", "addon.remove_cuda",
]);

/** Params that would make the engine write to arbitrary places must come from the user's dialogs. */
function checkEngineParams(method: string, params: Record<string, unknown>): string | null {
  if (method === "project.export_metadata" && params.path != null) {
    if (typeof params.path !== "string" || !savedPaths.has(resolve(params.path))) return "path not chosen by the user";
  }
  if (method === "project.update_settings") {
    const dir = (params.settings as { output_dir?: unknown } | undefined)?.output_dir;
    if (dir != null && (typeof dir !== "string" || !(pickedFolders.has(resolve(dir)) || knownOutputDirs.has(resolve(dir))))) return "output folder not chosen by the user";
  }
  if (method === "addon.install_cuda" && params.zip != null) {
    // a local add-on zip: only one the user just picked in the file dialog
    if (typeof params.zip !== "string" || !pickedFiles.has(params.zip) || !/\.zip$/i.test(params.zip)) return "zip not chosen by the user";
  }
  if (method === "project.open") {
    // a project folder from elsewhere: only one the user just picked
    if (typeof params.path !== "string" || !pickedFolders.has(resolve(params.path))) return "folder not chosen by the user";
  }
  if (method === "projects.add_root") {
    // the previous projects folder (after a change) or a folder the user picked
    const p = typeof params.path === "string" ? resolve(params.path) : "";
    // only a previous projects folder main itself recorded (settings change) or a folder the user
    // just picked — never a value the renderer can write (U-1)
    if (!p || !(previousRoots.has(p) || pickedFolders.has(p) || pendingRoots().map((x) => resolve(x)).includes(p))) return "folder not chosen by the user";
  }
  if (method === "export.run") {
    const ov = params.audio_overrides as Record<string, unknown> | undefined;
    if (ov && Object.keys(ov).length) return "audio_overrides is not used by the app";
  }
  return null;
}

/** files the app ever opens (results, reports, logs); folders are always fine (U-1: an allowlist) */
const OPENABLE_EXT = new Set([".mp4", ".m4a", ".wav", ".flac", ".mp3", ".json", ".csv", ".txt", ".log", ".rpp"]);

/**
 * Folders or allowlisted files under the projects folder, a project's own folder, the log folder,
 * known output folders, or saved by the user. Location roots (other folders the engine lists)
 * may be opened as folders only, never files inside them (U-1).
 */
function openable(path: unknown): path is string {
  if (typeof path !== "string" || !isAbsolute(path)) return false;
  const p = resolve(path);
  let dir = false;
  try {
    dir = statSync(p).isDirectory();
  } catch {
    return false;
  }
  if (!dir && !OPENABLE_EXT.has(extname(p).toLowerCase())) return false;
  if (savedPaths.has(p)) return true;
  if (dir && [...locationDirs].some((r) => r === p)) return true;
  const roots = [settings.projectsRoot, LOG_DIR, ADDONS_DIR, ...knownOutputDirs, ...projectDirs.values()].filter((r): r is string => !!r).map((r) => resolve(r));
  return roots.some((r) => p === r || isInside(r, p));
}

/** settings:set accepts known keys with the default value's type only. */
function validSettingsPatch(patch: unknown): Partial<AppSettings> | null {
  if (!patch || typeof patch !== "object") return null;
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(patch as Record<string, unknown>)) {
    if (!(k in DEFAULT_SETTINGS)) return null;
    const def = (DEFAULT_SETTINGS as unknown as Record<string, unknown>)[k];
    if (k === "projectsRoot") {
      if (v !== null && (typeof v !== "string" || !/^([a-zA-Z]:[\\/]|\\\\)/.test(v))) return null;
      if (typeof v === "string" && /^[a-zA-Z]:[\\/](windows|program files)/i.test(v)) return null;
    } else if (k === "videoHeight" || k === "loudness" || k === "muteGainDb" || k === "exportHeight" || k === "exportVideoK") {
      if (v !== null && typeof v !== "number") return null;
    } else if (k === "keymap") {
      if (!v || typeof v !== "object" || Object.values(v).some((x) => typeof x !== "string")) return null;
    } else if (k === "showLanes") {
      if (!v || typeof v !== "object") return null;
    } else if (def !== null && typeof v !== typeof def) return null;
    out[k] = v;
  }
  return out as Partial<AppSettings>;
}

const notifications = new Set<Notification>(); // keep a reference so click handlers survive GC

function registerIpc() {
  handle("engine:call", (_e, method: unknown, params: unknown) => {
    if (typeof method !== "string" || !ENGINE_METHODS.has(method)) return { ok: false, error: { code: 0, message: `method not allowed: ${String(method)}`, engineCode: "INVALID_PARAMS" } };
    const prm = (params && typeof params === "object" ? params : {}) as Record<string, unknown>;
    const bad = checkEngineParams(method, prm);
    if (bad) return { ok: false, error: { code: 0, message: bad, engineCode: "INVALID_PARAMS" } };
    const res = engine.call(method, prm);
    if (method === "project.list") void res.then((r) => r.ok && noteSummaries(r.result));
    if (method === "projects.locations")
      void res.then((r) => {
        const l = r.ok ? (r.result as { roots?: unknown; projects?: unknown }) : null;
        if (l) locationDirs.clear(); // rebuilt from the engine's list (U-19)
        for (const p of [...(Array.isArray(l?.roots) ? l.roots : []), ...(Array.isArray(l?.projects) ? l.projects : [])]) if (typeof p === "string") locationDirs.add(resolve(p));
      });
    if (method === "projects.add_root")
      void res.then((r) => {
        if ((r.ok || r.error.engineCode === "NOT_FOUND" || r.error.engineCode === "INVALID_PARAMS") && typeof prm.path === "string") setPendingRoots(pendingRoots().filter((x) => resolve(x) !== resolve(prm.path as string)));
      });
    if (method === "projects.remove_location") void res.then(() => locationDirs.clear());
    if (method === "project.open" || method === "projects.add_root" || method === "projects.remove_location" || method === "project.create" || method === "project.delete") void res.then(() => refreshProjectDirs());
    return res;
  });
  handle("engine:status", () => engine.status);
  handle("engine:restart", () => {
    engine.restart();
    return engine.status;
  });

  handle("settings:get", () => settings);
  handle("settings:set", (_e, raw: unknown) => {
    const patch = validSettingsPatch(raw);
    if (!patch) throw new Error("invalid settings patch");
    const prev = settings;
    settings = { ...settings, ...patch };
    saveSettings();
    if (patch.lowPriority !== undefined) engine.setOptions({ lowPriority: settings.lowPriority });
    const rootChanged = patch.projectsRoot !== undefined && patch.projectsRoot !== prev.projectsRoot;
    if (rootChanged && prev.projectsRoot) {
      previousRoots.add(resolve(prev.projectsRoot));
      // the renderer lists the old folder again once the engine runs on the new one (main-only)
      setPendingRoots([...pendingRoots(), prev.projectsRoot]);
    }
    const deviceChanged = patch.device !== undefined && patch.device !== prev.device;
    if (deviceChanged) engine.setOptions({ preferCpu: settings.device === "cpu" });
    const notRunning = engine.status.state === "no-root" || engine.status.state === "stopped";
    if (settings.projectsRoot && notRunning) {
      engine.start(settings.projectsRoot);
    } else if ((rootChanged || deviceChanged) && settings.projectsRoot) {
      void engine.stop().then(() => engine.start(settings.projectsRoot!));
    }
    return settings;
  });
  handle("state:get", (_e, key: unknown) => (typeof key === "string" ? state[key] ?? null : null));
  handle("state:set", (_e, key: unknown, value: unknown) => {
    if (typeof key !== "string" || key.length > 300) throw new Error("bad key");
    if (key.startsWith(MAIN_ONLY_STATE) || key === "pendingAddRoot") throw new Error("reserved key");
    if (value === null || value === undefined) delete state[key];
    else {
      if (JSON.stringify(value).length > 512 * 1024) throw new Error("value too large");
      state[key] = value;
    }
    saveStateSoon();
  });

  handle("dialog:folder", async (_e, defaultPath?: string) => {
    const r = await dialog.showOpenDialog(win!, { properties: ["openDirectory", "createDirectory"], defaultPath: typeof defaultPath === "string" ? defaultPath : undefined });
    const p = r.canceled ? null : r.filePaths[0] ?? null;
    if (p) pickedFolders.add(resolve(p));
    return p;
  });
  handle("dialog:save", async (_e, opts: { title?: string; defaultPath?: string; filters?: Electron.FileFilter[] }) => {
    const r = await dialog.showSaveDialog(win!, { title: opts?.title, defaultPath: opts?.defaultPath, filters: opts?.filters });
    const p = r.canceled ? null : r.filePath ?? null;
    if (p) savedPaths.add(resolve(p));
    return p;
  });
  // Only files the user just picked in a dialog are read (cookie file import).
  handle("dialog:file:read", async (_e, path: string) => {
    if (!pickedFiles.has(path)) throw new Error("not a picked file");
    const st = await fsp.stat(path);
    if (st.size > 1 << 20) throw new Error("file too large");
    return fsp.readFile(path, "utf8");
  });
  handle("dialog:file", async (_e, opts: { title?: string; filters?: Electron.FileFilter[] }) => {
    const r = await dialog.showOpenDialog(win!, { title: opts?.title, properties: ["openFile"], filters: opts?.filters });
    const p = r.canceled ? null : r.filePaths[0] ?? null;
    if (p) pickedFiles.add(p);
    return p;
  });

  handle("shell:open", async (_e, path: unknown) => {
    if (openable(path) && existsSync(path)) await shell.openPath(path);
  });
  handle("shell:show", (_e, path: unknown) => {
    if (openable(path) && existsSync(path)) shell.showItemInFolder(path);
  });
  handle("shell:external", async (_e, url: unknown) => {
    if (typeof url === "string" && /^https:\/\//.test(url)) await shell.openExternal(url);
  });

  handle("fs:drives", (): DriveInfo[] => {
    const out: DriveInfo[] = [];
    for (let c = 67; c <= 90; c++) {
      const p = String.fromCharCode(c) + ":\\";
      try {
        const st = statfsSync(p);
        out.push({ path: p, free: st.bavail * st.bsize, total: st.blocks * st.bsize });
      } catch {
        /* no such drive */
      }
    }
    return out;
  });
  handle("fs:check", (_e, path: string) => folderCheck(path));
  handle("fs:projectSize", async (_e, projectId: unknown) => {
    const root = settings.projectsRoot;
    if (!root || typeof projectId !== "string" || !isProjectId(projectId)) return { total: 0, clips: 0 };
    const dir = join(root, projectId);
    if (!isInside(root, dir)) return { total: 0, clips: 0 };
    return { total: await dirSize(dir), clips: await dirSize(join(dir, "clips")) };
  });
  handle("app:logDir", () => LOG_DIR);

  handle("app:notify", (_e, title: unknown, body: unknown) => {
    if (!Notification.isSupported()) return;
    const n = new Notification({ title: String(title).slice(0, 120), body: String(body).slice(0, 300), silent: false });
    notifications.add(n);
    const done = () => notifications.delete(n);
    n.on("click", () => {
      done();
      if (win && !win.isDestroyed()) {
        win.show();
        win.focus();
      }
    });
    n.on("close", done);
    n.show();
  });
  handle("app:focused", () => !!win && !win.isDestroyed() && win.isFocused());
  on("app:busy", (_e, b: boolean) => {
    busy = !!b;
  });
  on("app:close-answer", (_e, action: CloseAction) => {
    if (action === "minimize") win?.minimize();
    else if (action === "stop") {
      quitting = true;
      win?.close();
    }
  });
}

app.on("second-instance", () => {
  if (!win || win.isDestroyed()) return;
  if (win.isMinimized()) win.restore();
  win.show();
  win.focus();
});

app.whenReady().then(() => {
  app.setAppUserModelId("com.cliper.desktop");
  // no default menu: its accelerators (Ctrl+Shift+Z, Ctrl+R, …) would swallow the app's shortcuts
  Menu.setApplicationMenu(null);
  protocol.handle("app", createAppHandler(join(__dirname, "../renderer")));
  protocol.handle("cliper-media", createMediaHandler({ projectsRoot: () => settings.projectsRoot, projectDir, allowOrigin: (o) => !!o && originOf(o) === ENTRY_ORIGIN }));
  // deny every permission request except audio output selection and notifications
  session.defaultSession.setPermissionRequestHandler((_wc, permission, cb) => {
    cb(permission === "notifications" || permission === "speaker-selection" || permission === "clipboard-sanitized-write");
  });
  registerIpc();
  createWindow();
  if (settings.projectsRoot) engine.start(settings.projectsRoot);
});

app.on("before-quit", () => {
  quitting = true;
  flushState();
});
app.on("window-all-closed", async () => {
  // armed first: never linger without a window, whatever happens below
  setTimeout(() => app.exit(0), 6000).unref();
  flushState();
  try {
    await engine.stop();
  } finally {
    app.quit();
  }
});
