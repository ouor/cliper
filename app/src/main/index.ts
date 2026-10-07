import { app, BrowserWindow, dialog, ipcMain, Notification, protocol, shell, session } from "electron";
import { existsSync, mkdirSync, promises as fsp, readFileSync, writeFileSync, accessSync, constants as fsConst, statfsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Engine } from "./engine";
import { createMediaHandler } from "./media";
import { DEFAULT_SETTINGS, type AppSettings } from "../shared/settings";
import type { CloseAction, DriveInfo, FolderCheck } from "../shared/bridge";

const __dirname = dirname(fileURLToPath(import.meta.url));

protocol.registerSchemesAsPrivileged([
  { scheme: "cliper-media", privileges: { standard: true, secure: true, stream: true, supportFetchAPI: true, corsEnabled: true } },
]);

// ── paths ──
const appRoot = app.getAppPath(); // …/app in dev and preview
const repoRoot = resolve(appRoot, "..");
function firstExisting(...paths: (string | undefined)[]): string {
  for (const p of paths) if (p && existsSync(p)) return p;
  return paths.find(Boolean) as string;
}
const ENGINE_EXE = firstExisting(
  process.env.CLIPER_ENGINE,
  app.isPackaged ? join(process.resourcesPath, "engine", "cliper.exe") : undefined,
  join(repoRoot, "core", "target", "release", "cliper.exe"),
);
const MODELS_DIR = firstExisting(
  process.env.CLIPER_MODELS,
  app.isPackaged ? join(process.resourcesPath, "models") : undefined,
  join(repoRoot, "models"),
);
function findFfmpeg(): string {
  const bundled = app.isPackaged ? join(process.resourcesPath, "ffmpeg", "ffmpeg.exe") : undefined;
  if (process.env.CLIPER_FFMPEG) return process.env.CLIPER_FFMPEG;
  if (bundled && existsSync(bundled)) return bundled;
  return "ffmpeg"; // PATH (the engine uses the same)
}
const FFMPEG = findFfmpeg();

// ── settings & small state store (userData) ──
if (process.env.CLIPER_USER_DATA) app.setPath("userData", resolve(process.env.CLIPER_USER_DATA));
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
let settings: AppSettings = { ...DEFAULT_SETTINGS, ...readJson<Partial<AppSettings>>(SETTINGS_FILE, {}) };
if (process.env.CLIPER_PROJECTS) {
  settings.projectsRoot = resolve(process.env.CLIPER_PROJECTS);
  settings.firstRunDone = settings.firstRunDone || process.env.CLIPER_SKIP_FIRST_RUN === "1";
}
let state: Record<string, unknown> = readJson(STATE_FILE, {});
let stateTimer: NodeJS.Timeout | null = null;
function saveSettings() {
  writeFileSync(SETTINGS_FILE, JSON.stringify(settings, null, 2));
}
function saveStateSoon() {
  if (stateTimer) return;
  stateTimer = setTimeout(() => {
    stateTimer = null;
    writeFileSync(STATE_FILE, JSON.stringify(state));
  }, 300);
}

// ── engine ──
const engine = new Engine({ exe: ENGINE_EXE, models: MODELS_DIR, logDir: LOG_DIR, preferCpu: settings.device === "cpu", lowPriority: settings.lowPriority });
let win: BrowserWindow | null = null;
let busy = false;
let quitting = false;

engine.on("status", (s) => win?.webContents.send("engine:status", s));
engine.on("notify", (method: string, params: unknown) => win?.webContents.send("engine:notify", method, params));

function createWindow() {
  win = new BrowserWindow({
    width: 1600,
    height: 960,
    minWidth: 1280,
    minHeight: 720,
    backgroundColor: settings.theme === "light" ? "#f5f6f8" : "#0e1014",
    title: "Cliper",
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
  // never navigate away or open windows from the renderer
  win.webContents.setWindowOpenHandler(({ url }) => {
    if (/^https:\/\//.test(url)) void shell.openExternal(url);
    return { action: "deny" };
  });
  win.webContents.on("will-navigate", (e, url) => {
    if (!url.startsWith("http://localhost:5733") && !url.startsWith("file://")) e.preventDefault();
  });
  win.on("close", (e) => {
    if (quitting || !busy) return;
    if (settings.closeBehavior === "minimize") {
      e.preventDefault();
      win?.minimize();
      return;
    }
    if (settings.closeBehavior === "stop") return;
    e.preventDefault();
    win?.webContents.send("app:close-request");
  });
  if (process.env.ELECTRON_RENDERER_URL) void win.loadURL(process.env.ELECTRON_RENDERER_URL);
  else void win.loadFile(join(__dirname, "../renderer/index.html"));
}

/** Writable + free space, without creating anything: tests the nearest existing ancestor. */
function folderCheck(path: string): FolderCheck {
  try {
    if (!path || !/^([a-zA-Z]:[\\/]|\\\\)/.test(path)) throw new Error("not an absolute path");
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

function registerIpc() {
  ipcMain.handle("engine:call", (_e, method: string, params: unknown) => engine.call(method, params));
  ipcMain.handle("engine:status", () => engine.status);
  ipcMain.handle("engine:restart", () => {
    engine.restart();
    return engine.status;
  });

  ipcMain.handle("settings:get", () => settings);
  ipcMain.handle("settings:set", (_e, patch: Partial<AppSettings>) => {
    const prev = settings;
    settings = { ...settings, ...patch };
    saveSettings();
    if (patch.lowPriority !== undefined) engine.setOptions({ lowPriority: settings.lowPriority });
    const rootChanged = patch.projectsRoot !== undefined && patch.projectsRoot !== prev.projectsRoot;
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
  ipcMain.handle("state:get", (_e, key: string) => state[key] ?? null);
  ipcMain.handle("state:set", (_e, key: string, value: unknown) => {
    if (value === null || value === undefined) delete state[key];
    else state[key] = value;
    saveStateSoon();
  });

  ipcMain.handle("dialog:folder", async (_e, defaultPath?: string) => {
    const r = await dialog.showOpenDialog(win!, { properties: ["openDirectory", "createDirectory"], defaultPath });
    return r.canceled ? null : r.filePaths[0] ?? null;
  });
  ipcMain.handle("dialog:save", async (_e, opts: { title?: string; defaultPath?: string; filters?: Electron.FileFilter[] }) => {
    const r = await dialog.showSaveDialog(win!, { title: opts.title, defaultPath: opts.defaultPath, filters: opts.filters });
    return r.canceled ? null : r.filePath ?? null;
  });
  // Only files the user just picked in a dialog are read (cookie file import).
  const pickedFiles = new Set<string>();
  ipcMain.handle("dialog:file:read", async (_e, path: string) => {
    if (!pickedFiles.has(path)) throw new Error("not a picked file");
    const st = await fsp.stat(path);
    if (st.size > 1 << 20) throw new Error("file too large");
    return fsp.readFile(path, "utf8");
  });
  ipcMain.handle("dialog:file", async (_e, opts: { title?: string; filters?: Electron.FileFilter[] }) => {
    const r = await dialog.showOpenDialog(win!, { title: opts.title, properties: ["openFile"], filters: opts.filters });
    const p = r.canceled ? null : r.filePaths[0] ?? null;
    if (p) pickedFiles.add(p);
    return p;
  });

  ipcMain.handle("shell:open", async (_e, path: string) => {
    if (typeof path === "string" && existsSync(path)) await shell.openPath(path);
  });
  ipcMain.handle("shell:show", (_e, path: string) => {
    if (typeof path === "string" && existsSync(path)) shell.showItemInFolder(path);
  });
  ipcMain.handle("shell:external", async (_e, url: string) => {
    if (/^https:\/\//.test(url)) await shell.openExternal(url);
  });

  ipcMain.handle("fs:drives", (): DriveInfo[] => {
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
  ipcMain.handle("fs:check", (_e, path: string) => folderCheck(path));
  ipcMain.handle("fs:projectSize", async (_e, projectId: string) => {
    const root = settings.projectsRoot;
    if (!root || !/^[A-Za-z0-9][A-Za-z0-9._-]*$/.test(projectId)) return { total: 0, clips: 0 };
    const dir = join(root, projectId);
    return { total: await dirSize(dir), clips: await dirSize(join(dir, "clips")) };
  });
  ipcMain.handle("app:logDir", () => LOG_DIR);

  ipcMain.handle("app:notify", (_e, title: string, body: string) => {
    if (!Notification.isSupported()) return;
    const n = new Notification({ title, body, silent: false });
    n.on("click", () => {
      win?.show();
      win?.focus();
    });
    n.show();
  });
  ipcMain.handle("app:focused", () => !!win?.isFocused());
  ipcMain.on("app:busy", (_e, b: boolean) => {
    busy = !!b;
  });
  ipcMain.on("app:close-answer", (_e, action: CloseAction) => {
    if (action === "minimize") win?.minimize();
    else if (action === "stop") {
      quitting = true;
      win?.close();
    }
  });
}

app.whenReady().then(() => {
  app.setAppUserModelId("com.cliper.desktop");
  protocol.handle("cliper-media", createMediaHandler({ projectsRoot: () => settings.projectsRoot, ffmpeg: () => FFMPEG }));
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
});
app.on("window-all-closed", async () => {
  await engine.stop();
  app.quit();
});

