// Supervises the Rust engine child process (`cliper serve`): NDJSON JSON-RPC over
// stdio, request/response matching, notification fan-out, crash restart.
import { execFile, spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { EventEmitter } from "node:events";
import { createWriteStream, existsSync, mkdirSync, renameSync, statSync, type WriteStream } from "node:fs";
import { join } from "node:path";
import { setPriority, constants as osConstants } from "node:os";
import type { CallResult, EngineStatus } from "../shared/bridge";

interface Pending {
  resolve: (r: CallResult) => void;
  method: string;
  timer?: NodeJS.Timeout;
}

export interface EngineOptions {
  exe: string;
  models: string;
  logDir: string;
  preferCpu: boolean;
  lowPriority: boolean;
  /** extra environment for the engine (e.g. CLIPER_FFMPEG in packaged builds) */
  env?: Record<string, string>;
}

const MAX_RESTARTS = 3;
const RESTART_WINDOW_MS = 120_000;
/** a single protocol line larger than this is treated as a protocol error */
const MAX_LINE = 64 * 1024 * 1024;
const LOG_ROTATE = 5 * 1024 * 1024;

/** Per-method response timeouts (ms). Jobs return a handle immediately; 0 = no timeout. */
const TIMEOUTS: Record<string, number> = {
  "project.create": 180_000,
  "system.probe_gpu": 180_000,
  "system.check_cookies": 60_000,
  "media.drop": 300_000,
  "storage.usage": 120_000,
  "video.view": 300_000,
  "video.fetch": 120_000,
  "video.estimate": 120_000,
  "vod.check": 120_000,
  "project.delete": 120_000,
  "project.export_metadata": 60_000,
  "detect.rerun": 60_000,
  "engine.shutdown": 10_000,
};
const DEFAULT_TIMEOUT = 30_000;

export class Engine extends EventEmitter {
  private child: ChildProcessWithoutNullStreams | null = null;
  private exitWaiters: (() => void)[] = [];
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private buf = "";
  private stderrTail: string[] = [];
  private stopping = false;
  private crashes: number[] = [];
  private restartTimer: NodeJS.Timeout | null = null;
  private logStream: WriteStream | null = null;
  private logBytes = 0;
  /** the engine sent engine.failed: it won't work by restarting it */
  private fatal = false;
  status: EngineStatus = { state: "no-root", system: null, restarts: 0, projectsRoot: null };

  constructor(private opts: EngineOptions) {
    super();
    mkdirSync(opts.logDir, { recursive: true });
  }

  setOptions(patch: Partial<EngineOptions>) {
    this.opts = { ...this.opts, ...patch };
    if (patch.lowPriority !== undefined) this.applyPriority();
  }

  private setStatus(patch: Partial<EngineStatus>) {
    this.status = { ...this.status, ...patch };
    this.emitSafe("status", this.status);
  }

  /** Listeners must never break the exit/teardown paths. */
  private emitSafe(event: string, ...args: unknown[]) {
    try {
      this.emit(event, ...args);
    } catch (e) {
      this.log(`listener error (${event}): ${(e as Error).message}`);
    }
  }

  /** Asynchronous, size-rotated engine log (never blocks the main thread). */
  private log(line: string) {
    try {
      const file = join(this.opts.logDir, "engine.log");
      if (!this.logStream) {
        this.logBytes = existsSync(file) ? statSync(file).size : 0;
        this.logStream = createWriteStream(file, { flags: "a" });
        this.logStream.on("error", () => (this.logStream = null));
      }
      const text = line.endsWith("\n") ? line : line + "\n";
      this.logStream.write(text);
      this.logBytes += Buffer.byteLength(text);
      if (this.logBytes > LOG_ROTATE) {
        const s = this.logStream;
        this.logStream = null;
        s.end(() => {
          try {
            renameSync(file, file + ".1");
          } catch {
            /* ignore */
          }
        });
        this.logBytes = 0;
      }
    } catch {
      /* logging must never break the app */
    }
  }

  start(projectsRoot: string) {
    this.stopping = false;
    this.fatal = false;
    if (this.restartTimer) clearTimeout(this.restartTimer);
    this.restartTimer = null;
    if (!existsSync(this.opts.exe)) {
      this.setStatus({
        state: "failed",
        projectsRoot,
        system: null,
        reason: "처리 엔진 파일을 찾을 수 없어요. 앱을 다시 설치해 주세요.",
        detail: this.opts.exe,
      });
      return;
    }
    mkdirSync(projectsRoot, { recursive: true });
    const args = ["serve", "--projects", projectsRoot, "--models", this.opts.models];
    if (this.opts.preferCpu) args.push("--cpu");
    this.log(`\n=== ${new Date().toISOString()} spawn ${this.opts.exe} ${args.join(" ")}`);
    this.setStatus({ state: this.status.restarts > 0 ? "restarting" : "starting", projectsRoot, system: null, reason: undefined });
    const child = spawn(this.opts.exe, args, { stdio: ["pipe", "pipe", "pipe"], windowsHide: true, env: { ...process.env, ...(this.opts.env ?? {}) } });
    this.child = child;
    this.buf = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => this.onStdout(child, chunk));
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      this.log(chunk);
      for (const l of chunk.split(/\r?\n/)) if (l.trim()) this.stderrTail.push(l);
      if (this.stderrTail.length > 200) this.stderrTail.splice(0, this.stderrTail.length - 200);
    });
    // writes after the engine died fail asynchronously (EPIPE); exit handling cleans up
    child.stdin.on("error", (e) => this.log(`stdin error: ${e.message}`));
    child.on("error", (err) => {
      this.log(`spawn error: ${err.message}`);
      this.onExit(child, null, err.message);
    });
    child.on("exit", (code, signal) => this.onExit(child, code, signal ?? undefined));
    this.applyPriority();
  }

  private applyPriority() {
    if (!this.child?.pid) return;
    try {
      setPriority(this.child.pid, this.opts.lowPriority ? osConstants.priority.PRIORITY_BELOW_NORMAL : osConstants.priority.PRIORITY_NORMAL);
    } catch {
      /* ignore */
    }
  }

  private onStdout(child: ChildProcessWithoutNullStreams, chunk: string) {
    this.buf += chunk;
    if (this.buf.length > MAX_LINE && this.buf.indexOf("\n") < 0) {
      this.log(`protocol line exceeds ${MAX_LINE} bytes; restarting the engine`);
      this.buf = "";
      child.kill();
      return;
    }
    let nl: number;
    while ((nl = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, nl).trim();
      this.buf = this.buf.slice(nl + 1);
      if (!line) continue;
      let msg: { id?: number | null; result?: unknown; error?: { code?: number; message?: string; data?: { code?: string } }; method?: unknown; params?: Record<string, unknown> };
      try {
        msg = JSON.parse(line);
      } catch {
        this.log(`bad protocol line: ${line.slice(0, 300)}`);
        continue;
      }
      if (msg.id !== undefined && msg.id !== null && ("result" in msg || "error" in msg)) {
        const p = this.pending.get(msg.id);
        if (!p) continue;
        this.pending.delete(msg.id);
        if (p.timer) clearTimeout(p.timer);
        if (msg.error) {
          const e = msg.error;
          p.resolve({ ok: false, error: { code: e.code ?? -32000, message: e.message ?? "unknown error", engineCode: e.data?.code ?? (e.code === -32601 ? "METHOD" : e.code === -32602 ? "INVALID_PARAMS" : "INTERNAL"), data: e.data && typeof e.data === "object" ? e.data : undefined } });
        } else {
          p.resolve({ ok: true, result: msg.result });
          this.emitSafe("result", p.method, msg.result);
        }
      } else if (typeof msg.method === "string") {
        if (msg.method === "engine.failed") {
          // the engine refused to start (protocol 4: ROOT_LOCKED, SCHEMA_NEWER, …): show why and
          // don't restart it in a loop when it exits
          const prm = (msg.params ?? {}) as { code?: string; message?: string; data?: { root?: string } };
          this.fatal = true;
          const root = prm.data?.root ?? this.status.projectsRoot ?? "";
          this.setStatus({
            state: "failed",
            system: null,
            failCode: prm.code,
            reason:
              prm.code === "ROOT_LOCKED"
                ? `다른 Cliper가 이 작업 폴더를 쓰고 있어요 (${root}). 그 창을 닫거나 다른 작업 폴더를 고른 뒤 [다시 시작]을 눌러 주세요.`
                : `처리 엔진을 시작하지 못했어요 · ${prm.message ?? prm.code ?? ""}`,
            detail: `${prm.code ?? ""}: ${prm.message ?? ""}`,
          });
        } else if (msg.method === "engine.ready") {
          const prm = msg.params ?? {};
          this.setStatus({ state: "ready", system: prm.system ?? null, protocol: prm.protocol as number, minClientProtocol: prm.min_client_protocol as number, reason: undefined, detail: undefined, failCode: undefined });
        }
        this.emitSafe("notify", msg.method, msg.params);
      }
    }
  }

  private onExit(child: ChildProcessWithoutNullStreams, code: number | null, signal?: string) {
    if (this.child !== child) return;
    this.child = null;
    const waiters = this.exitWaiters;
    this.exitWaiters = [];
    for (const w of waiters) w();
    this.log(`=== exit code=${code} signal=${signal ?? ""}`);
    for (const [, p] of this.pending) {
      if (p.timer) clearTimeout(p.timer);
      p.resolve({ ok: false, error: { code: 0, message: "engine exited", engineCode: "ENGINE_DOWN" } });
    }
    this.pending.clear();
    if (this.stopping) {
      this.setStatus({ state: "stopped", system: null });
      return;
    }
    if (this.fatal) {
      // engine.failed already said why (e.g. another Cliper holds the folder): no restart loop
      this.log("=== not restarting: engine.failed");
      return;
    }
    const now = Date.now();
    this.crashes = this.crashes.filter((t) => now - t < RESTART_WINDOW_MS);
    this.crashes.push(now);
    const detail = this.stderrTail.slice(-20).join("\n");
    if (this.crashes.length > MAX_RESTARTS) {
      this.setStatus({
        state: "failed",
        system: null,
        reason: "처리 엔진이 계속 멈춰요. 진단 정보를 확인하거나 [다시 시작]을 눌러 주세요.",
        detail,
      });
      return;
    }
    this.setStatus({
      state: "restarting",
      system: null,
      restarts: this.status.restarts + 1,
      reason: "처리 엔진이 예기치 않게 멈춰서 다시 시작하고 있어요. 저장된 작업은 그대로예요.",
      detail,
    });
    const root = this.status.projectsRoot;
    this.restartTimer = setTimeout(() => root && this.start(root), 1000 * this.crashes.length);
  }

  /** Restart on user request (clears the crash budget). */
  restart() {
    this.crashes = [];
    const root = this.status.projectsRoot;
    if (!root) return;
    if (this.child) {
      void this.stop().then(() => this.start(root));
    } else this.start(root);
  }

  call(method: string, params: unknown): Promise<CallResult> {
    const child = this.child;
    if (!child || this.status.state !== "ready" || !child.stdin.writable) {
      return Promise.resolve({ ok: false, error: { code: 0, message: "engine not running", engineCode: "ENGINE_DOWN" } });
    }
    const id = this.nextId++;
    return new Promise((resolve) => {
      const p: Pending = { resolve, method };
      const ms = TIMEOUTS[method] ?? DEFAULT_TIMEOUT;
      if (ms > 0) {
        p.timer = setTimeout(() => {
          if (!this.pending.delete(id)) return;
          this.log(`timeout: ${method} (${ms} ms)`);
          resolve({ ok: false, error: { code: 0, message: `${method} timed out after ${ms} ms`, engineCode: "TIMEOUT" } });
        }, ms);
      }
      this.pending.set(id, p);
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params: params ?? {} }) + "\n");
    });
  }

  /** Kill the engine and every process it started (ffmpeg …). Windows: taskkill /T. */
  private killTree(child: ChildProcessWithoutNullStreams) {
    if (process.platform === "win32" && child.pid) {
      execFile("taskkill", ["/PID", String(child.pid), "/T", "/F"], { windowsHide: true }, (err) => {
        if (err) child.kill();
      });
    } else child.kill();
  }

  /** Graceful shutdown: engine.shutdown (cancels jobs), then kill the tree after a grace period. */
  async stop(): Promise<void> {
    this.stopping = true;
    if (this.restartTimer) clearTimeout(this.restartTimer);
    const child = this.child;
    if (!child) return;
    // register before anything can trigger the exit (independent of listener order)
    const exited = new Promise<void>((r) => this.exitWaiters.push(r));
    const t = setTimeout(() => this.killTree(child), 4000);
    if (this.status.state === "ready") void this.call("engine.shutdown", {});
    else child.stdin.end();
    await exited;
    clearTimeout(t);
  }

  get stderr(): string {
    return this.stderrTail.join("\n");
  }
}
