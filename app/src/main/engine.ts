// Supervises the Rust engine child process (`cliper serve`): NDJSON JSON-RPC over
// stdio, request/response matching, notification fan-out, crash restart.
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { EventEmitter } from "node:events";
import { appendFileSync, existsSync, mkdirSync, renameSync, statSync } from "node:fs";
import { join } from "node:path";
import { setPriority, constants as osConstants } from "node:os";
import type { CallResult, EngineStatus } from "../shared/bridge";

interface Pending {
  resolve: (r: CallResult) => void;
  method: string;
}

export interface EngineOptions {
  exe: string;
  models: string;
  logDir: string;
  preferCpu: boolean;
  lowPriority: boolean;
}

const MAX_RESTARTS = 3;
const RESTART_WINDOW_MS = 120_000;

export class Engine extends EventEmitter {
  private child: ChildProcessWithoutNullStreams | null = null;
  private nextId = 1;
  private pending = new Map<number, Pending>();
  private buf = "";
  private stderrTail: string[] = [];
  private stopping = false;
  private crashes: number[] = [];
  private restartTimer: NodeJS.Timeout | null = null;
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
    this.emit("status", this.status);
  }

  private log(line: string) {
    const file = join(this.opts.logDir, "engine.log");
    try {
      if (existsSync(file) && statSync(file).size > 5 * 1024 * 1024) renameSync(file, file + ".1");
      appendFileSync(file, line.endsWith("\n") ? line : line + "\n");
    } catch {
      /* logging must never break the app */
    }
  }

  start(projectsRoot: string) {
    this.stopping = false;
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
    const child = spawn(this.opts.exe, args, { stdio: ["pipe", "pipe", "pipe"], windowsHide: true });
    this.child = child;
    this.buf = "";
    child.stdout.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => this.onStdout(chunk));
    child.stderr.setEncoding("utf8");
    child.stderr.on("data", (chunk: string) => {
      this.log(chunk);
      for (const l of chunk.split(/\r?\n/)) if (l.trim()) this.stderrTail.push(l);
      if (this.stderrTail.length > 200) this.stderrTail.splice(0, this.stderrTail.length - 200);
    });
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

  private onStdout(chunk: string) {
    this.buf += chunk;
    let nl: number;
    while ((nl = this.buf.indexOf("\n")) >= 0) {
      const line = this.buf.slice(0, nl).trim();
      this.buf = this.buf.slice(nl + 1);
      if (!line) continue;
      let msg: any;
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
        if ("error" in msg) {
          const e = msg.error ?? {};
          p.resolve({ ok: false, error: { code: e.code ?? -32000, message: e.message ?? "unknown error", engineCode: e.data?.code ?? (e.code === -32601 ? "METHOD" : e.code === -32602 ? "INVALID_PARAMS" : "INTERNAL") } });
        } else {
          p.resolve({ ok: true, result: msg.result });
        }
      } else if (typeof msg.method === "string") {
        if (msg.method === "engine.ready") {
          this.setStatus({ state: "ready", system: msg.params?.system ?? null, protocol: msg.params?.protocol, reason: undefined, detail: undefined });
        }
        this.emit("notify", msg.method, msg.params);
      }
    }
  }

  private onExit(child: ChildProcessWithoutNullStreams, code: number | null, signal?: string) {
    if (this.child !== child) return;
    this.child = null;
    this.log(`=== exit code=${code} signal=${signal ?? ""}`);
    for (const [, p] of this.pending) {
      p.resolve({ ok: false, error: { code: 0, message: "engine exited", engineCode: "ENGINE_DOWN" } });
    }
    this.pending.clear();
    if (this.stopping) {
      this.setStatus({ state: "stopped", system: null });
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
      this.stop().then(() => this.start(root));
    } else this.start(root);
  }

  call(method: string, params: unknown): Promise<CallResult> {
    const child = this.child;
    if (!child || this.status.state !== "ready") {
      return Promise.resolve({ ok: false, error: { code: 0, message: "engine not running", engineCode: "ENGINE_DOWN" } });
    }
    const id = this.nextId++;
    return new Promise((resolve) => {
      this.pending.set(id, { resolve, method });
      child.stdin.write(JSON.stringify({ jsonrpc: "2.0", id, method, params: params ?? {} }) + "\n");
    });
  }

  /** Graceful shutdown: engine.shutdown (cancels jobs), then kill after a grace period. */
  async stop(): Promise<void> {
    this.stopping = true;
    if (this.restartTimer) clearTimeout(this.restartTimer);
    const child = this.child;
    if (!child) return;
    const exited = new Promise<void>((r) => child.once("exit", () => r()));
    if (this.status.state === "ready") {
      void this.call("engine.shutdown", {});
    } else {
      child.stdin.end();
    }
    const t = setTimeout(() => child.kill(), 4000);
    await exited;
    clearTimeout(t);
  }

  get stderr(): string {
    return this.stderrTail.join("\n");
  }
}
