// Main-thread controller for the preview worklet: one AudioContext for the whole app,
// sources (VOD stream / stems window), transport (J/K/L), listening modes, video sync.
import { create } from "zustand";
import workletUrl from "./player.worklet.ts?worker&url";
import type { Mute } from "@ipc/generated/Mute";
import { mediaUrl } from "@/lib/peaks";

export type ListenMode = 0 | 1 | 2 | 3; // 원본 / 결과 / 지운 목소리 / 반주
export const LISTEN_LABEL = ["원본", "결과", "지운 목소리", "반주"] as const;
export const LISTEN_KEY = ["Q", "W", "E", "R"] as const;

export interface PlayerState {
  pos: number;
  playing: boolean;
  buffering: boolean;
  rate: number;
  mode: ListenMode;
  loop: [number, number] | null;
  loopOn: boolean;
  volume: number;
  source: "none" | "stream" | "stems";
  /** stems window currently loaded */
  stems: { projectId: string; clipId: string; start: number; end: number; progress: number; error?: string } | null;
  streamError?: string;
}

export const usePlayer = create<PlayerState>(() => ({
  pos: 0,
  playing: false,
  buffering: false,
  rate: 1,
  mode: 1,
  loop: null,
  loopOn: false,
  volume: 0.9,
  source: "none",
  stems: null,
}));

const SR = 48000;
const CHUNK = 6; // seconds per stream chunk

interface VideoBinding {
  el: HTMLVideoElement;
  /** VOD seconds at video t = 0 */
  origin: number;
  duration: number;
}

class Player {
  private ctx: AudioContext | null = null;
  private node: AudioWorkletNode | null = null;
  private ready: Promise<void> | null = null;
  // reported position + the context time it was sampled at
  private repPos = 0;
  private repCtx = 0;
  private repRate = 1;
  // stream
  private streamProject: string | null = null;
  private streamDuration = 0;
  private sent = new Set<number>();
  private inflight = new Map<number, AbortController>();
  // stems
  private stemsAbort: AbortController | null = null;
  private stemsKey = "";
  private video: VideoBinding | null = null;

  private listeners = new Set<(pos: number) => void>();

  private async init() {
    if (this.ready) return this.ready;
    this.ready = (async () => {
      const ctx = new AudioContext({ sampleRate: SR, latencyHint: "interactive" });
      await ctx.audioWorklet.addModule(workletUrl);
      const node = new AudioWorkletNode(ctx, "cliper-player", { numberOfInputs: 0, numberOfOutputs: 1, outputChannelCount: [2] });
      node.connect(ctx.destination);
      node.port.onmessage = (ev) => this.onWorklet(ev.data);
      this.ctx = ctx;
      this.node = node;
      const st = usePlayer.getState();
      this.post({ type: "volume", volume: st.volume });
      this.post({ type: "mode", mode: st.mode });
      this.tick();
    })();
    return this.ready;
  }

  private post(msg: unknown, transfer?: Transferable[]) {
    this.node?.port.postMessage(msg, transfer ?? []);
  }

  private onWorklet(m: any) {
    if (m.type === "pos") {
      this.repPos = m.pos;
      this.repCtx = m.ctxTime;
      this.repRate = m.rate;
      const st = usePlayer.getState();
      if (st.playing !== m.playing || st.buffering !== m.buffering || m.force) {
        usePlayer.setState({ playing: m.playing, buffering: m.buffering, pos: m.pos });
      }
    } else if (m.type === "ended") {
      usePlayer.setState({ playing: false, rate: 1 });
    }
  }

  /** Interpolated playhead (VOD s). */
  now(): number {
    const st = usePlayer.getState();
    if (!st.playing || st.buffering || !this.ctx) return this.repPos;
    return this.repPos + (this.ctx.currentTime - this.repCtx) * this.repRate;
  }

  onFrame(cb: (pos: number) => void): () => void {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  }

  private tick = () => {
    const pos = this.now();
    const st = usePlayer.getState();
    if (st.playing && Math.abs(st.pos - pos) > 0.03) usePlayer.setState({ pos });
    for (const l of this.listeners) l(pos);
    if (st.source === "stream" && (st.playing || st.buffering)) this.prefetch(pos, st.rate);
    this.syncVideo(pos, st.playing && !st.buffering, st.rate);
    requestAnimationFrame(this.tick);
  };

  // ── sources ──

  async useStream(projectId: string, duration: number) {
    await this.init();
    if (this.streamProject === projectId && usePlayer.getState().source === "stream") return;
    this.stopStems();
    for (const c of this.inflight.values()) c.abort();
    this.inflight.clear();
    this.sent.clear();
    this.streamProject = projectId;
    this.streamDuration = duration;
    this.post({ type: "stream:init", chunkSec: CHUNK, duration });
    usePlayer.setState({ source: "stream", stems: null, streamError: undefined });
    this.prefetch(this.repPos, 1);
  }

  private prefetch(pos: number, rate: number) {
    if (!this.streamProject) return;
    const k = Math.floor(Math.max(0, pos) / CHUNK);
    const ahead = Math.abs(rate) > 1.5 ? 4 : 2;
    const want = rate >= 0 ? [k, k + 1, ...(ahead > 2 ? [k + 2, k + 3] : [k + 2])] : [k, k - 1, k - 2];
    for (const i of want) {
      if (i < 0 || i * CHUNK >= this.streamDuration) continue;
      if (this.sent.has(i) || this.inflight.has(i)) continue;
      this.fetchChunk(i);
    }
    // keep the worklet's cache bounded
    if (this.sent.size > 24) {
      const lo = k - 8;
      const hi = k + 8;
      for (const i of [...this.sent]) if (i < lo || i > hi) this.sent.delete(i);
      this.post({ type: "stream:keep", lo, hi });
    }
  }

  private fetchChunk(i: number) {
    const pid = this.streamProject!;
    const ac = new AbortController();
    this.inflight.set(i, ac);
    fetch(`cliper-media://pcm/${encodeURIComponent(pid)}?t=${i * CHUNK}&d=${CHUNK}`, { signal: ac.signal })
      .then(async (r) => {
        if (!r.ok) throw new Error(await r.text());
        const buf = await r.arrayBuffer();
        if (this.streamProject !== pid) return;
        const data = new Float32Array(buf, 0, Math.floor(buf.byteLength / 4));
        this.post({ type: "stream:chunk", index: i, data }, [buf]);
        this.sent.add(i);
      })
      .catch((e) => {
        if ((e as Error).name !== "AbortError") usePlayer.setState({ streamError: String((e as Error).message ?? e) });
      })
      .finally(() => this.inflight.delete(i));
  }

  private stopStems() {
    this.stemsAbort?.abort();
    this.stemsAbort = null;
    this.stemsKey = "";
  }

  /**
   * Load a window [start, end] (VOD s) of a clip's stems. `origin` = VOD time of clip
   * t=0 (vod_start + offset); `clipFrames` bounds the window.
   */
  async useStems(projectId: string, clipId: string, origin: number, clipFrames: number, start: number, end: number, bust = "") {
    await this.init();
    const f0 = Math.max(0, Math.floor((start - origin) * SR));
    const f1 = Math.min(clipFrames, Math.ceil((end - origin) * SR));
    const key = `${projectId}/${clipId}/${f0}/${f1}/${bust}`;
    if (key === this.stemsKey) return;
    // reuse when the requested window is inside the loaded one
    const cur = usePlayer.getState().stems;
    if (cur && cur.projectId === projectId && cur.clipId === clipId && !cur.error && cur.start <= start + 0.01 && cur.end >= end - 0.01 && this.stemsKey.endsWith(`/${bust}`)) return;
    this.stopStems();
    this.stemsKey = key;
    this.streamProject = null;
    for (const c of this.inflight.values()) c.abort();
    this.inflight.clear();
    this.sent.clear();
    const frames = Math.max(0, f1 - f0);
    const wStart = origin + f0 / SR;
    const wEnd = origin + f1 / SR;
    const wasPlaying = usePlayer.getState().playing;
    if (wasPlaying) this.pause();
    this.post({ type: "stems:init", origin: wStart, frames });
    usePlayer.setState({ source: "stems", stems: { projectId, clipId, start: wStart, end: wEnd, progress: 0 } });
    const ac = new AbortController();
    this.stemsAbort = ac;
    let doneO = 0;
    let doneV = 0;
    const update = () => {
      const st = usePlayer.getState().stems;
      if (st && this.stemsKey === key) usePlayer.setState({ stems: { ...st, progress: (doneO + doneV) / (2 * frames || 1) } });
    };
    let lastUpd = 0;
    const load = async (which: 0 | 1) => {
      const url = mediaUrl(projectId, `clips/${clipId}/${which === 0 ? "orig" : "vocals"}.wav`);
      // header first (small range) to find the data chunk
      const head = await fetch(url, { headers: { range: "bytes=0-4095" }, signal: ac.signal });
      const hb = new DataView(await head.arrayBuffer());
      const { dataOffset, channels, bits, format } = parseWavHeader(hb);
      if (channels !== 2 || bits !== 32 || format !== 3) throw new Error(`unsupported wav (${channels}ch ${bits}bit fmt ${format})`);
      const b0 = dataOffset + f0 * 8;
      const b1 = dataOffset + f1 * 8 - 1;
      const r = await fetch(url, { headers: { range: `bytes=${b0}-${b1}` }, signal: ac.signal });
      if (!r.body) throw new Error("no body");
      const reader = r.body.getReader();
      let carry = new Uint8Array(0);
      let samplesDone = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        let bytes = value;
        if (carry.length) {
          const merged = new Uint8Array(carry.length + value.length);
          merged.set(carry);
          merged.set(value, carry.length);
          bytes = merged;
        }
        const usable = bytes.length - (bytes.length % 4);
        carry = bytes.slice(usable);
        if (usable === 0) continue;
        const f32 = new Float32Array(bytes.buffer.slice(bytes.byteOffset, bytes.byteOffset + usable));
        const i16 = new Int16Array(f32.length);
        for (let i = 0; i < f32.length; i++) {
          const s = f32[i] * 32767;
          i16[i] = s > 32767 ? 32767 : s < -32768 ? -32768 : s;
        }
        this.post({ type: "stems:data", which, offset: samplesDone, data: i16 }, [i16.buffer]);
        samplesDone += i16.length;
        if (which === 0) doneO = samplesDone / 2;
        else doneV = samplesDone / 2;
        const now = performance.now();
        if (now - lastUpd > 100) {
          lastUpd = now;
          update();
        }
      }
    };
    try {
      await Promise.all([load(0), load(1)]);
      doneO = doneV = frames;
      update();
    } catch (e) {
      if ((e as Error).name === "AbortError") return;
      const st = usePlayer.getState().stems;
      if (st && this.stemsKey === key) usePlayer.setState({ stems: { ...st, error: String((e as Error).message ?? e) } });
    }
  }

  async clearSource() {
    await this.init();
    this.stopStems();
    this.streamProject = null;
    this.post({ type: "none" });
    usePlayer.setState({ source: "none", stems: null, playing: false });
  }

  // ── transport ──

  async play(from?: number, opts: { rate?: number; stopAt?: number } = {}) {
    await this.init();
    if (this.ctx!.state !== "running") await this.ctx!.resume();
    const pos = from ?? this.now();
    const rate = opts.rate ?? 1;
    this.repPos = pos;
    this.repCtx = this.ctx!.currentTime;
    this.repRate = rate;
    usePlayer.setState({ pos, rate, playing: true });
    if (usePlayer.getState().source === "stream") this.prefetch(pos, rate);
    this.post({ type: "play", pos, rate, stopAt: opts.stopAt });
  }

  pause() {
    const pos = this.now();
    this.repPos = pos;
    usePlayer.setState({ playing: false, pos, rate: 1 });
    this.post({ type: "pause" });
    this.post({ type: "seek", pos });
  }

  toggle() {
    if (usePlayer.getState().playing) this.pause();
    else void this.play();
  }

  seek(pos: number) {
    const p = Math.max(0, pos);
    this.repPos = p;
    if (this.ctx) this.repCtx = this.ctx.currentTime;
    usePlayer.setState({ pos: p });
    this.post({ type: "seek", pos: p });
    if (usePlayer.getState().source === "stream") this.prefetch(p, usePlayer.getState().rate);
    for (const l of this.listeners) l(p);
  }

  nudge(dt: number) {
    this.seek(this.now() + dt);
  }

  /** J/K/L: repeated presses step 1x → 2x → 4x in that direction. */
  shuttle(dir: -1 | 1) {
    const st = usePlayer.getState();
    let speed = 1;
    if (st.playing && Math.sign(st.rate) === dir) speed = Math.min(4, Math.abs(st.rate) * 2);
    void this.play(undefined, { rate: dir * speed });
  }

  setMode(mode: ListenMode) {
    usePlayer.setState({ mode });
    this.post({ type: "mode", mode });
  }

  setVolume(v: number) {
    usePlayer.setState({ volume: v });
    this.post({ type: "volume", volume: v });
  }

  setMutes(mutes: Mute[], muteOn: boolean) {
    this.post({
      type: "mutes",
      mutes: mutes.map((m) => ({ s: m.start, e: m.end, fi: m.fade_in, fo: m.fade_out, floor: m.gain_db === null ? 0 : 10 ** (m.gain_db / 20) })),
    });
    this.post({ type: "muteOn", on: muteOn });
  }

  setLoop(range: [number, number] | null, on?: boolean) {
    const loopOn = on ?? usePlayer.getState().loopOn;
    usePlayer.setState({ loop: range, loopOn });
    this.post({ type: "loop", loop: loopOn && range ? range : null });
  }

  toggleLoop() {
    const st = usePlayer.getState();
    this.setLoop(st.loop, !st.loopOn);
  }

  async setSink(id: string) {
    await this.init();
    const ctx = this.ctx as AudioContext & { setSinkId?: (id: string) => Promise<void> };
    if (ctx.setSinkId) {
      try {
        await ctx.setSinkId(id || "");
      } catch {
        /* device gone → keep default */
      }
    }
  }

  // ── video ──

  attachVideo(el: HTMLVideoElement | null, origin = 0, duration = 0) {
    if (!el) {
      this.video = null;
      return;
    }
    this.video = { el, origin, duration };
    el.muted = true;
  }

  private syncVideo(pos: number, playing: boolean, rate: number) {
    const v = this.video;
    if (!v || v.el.readyState < 1) return;
    const t = pos - v.origin;
    const inside = t >= 0 && t <= (v.duration || v.el.duration || Infinity);
    if (!inside) {
      if (!v.el.paused) v.el.pause();
      return;
    }
    if (playing && rate > 0) {
      if (v.el.playbackRate !== rate) v.el.playbackRate = rate;
      if (v.el.paused) void v.el.play().catch(() => {});
      if (Math.abs(v.el.currentTime - t) > 0.15) v.el.currentTime = t;
    } else {
      if (!v.el.paused) v.el.pause();
      if (Math.abs(v.el.currentTime - t) > 0.03 && !v.el.seeking) v.el.currentTime = t;
    }
  }
}

function parseWavHeader(v: DataView): { dataOffset: number; channels: number; bits: number; format: number; sampleRate: number } {
  const tag = (o: number) => String.fromCharCode(v.getUint8(o), v.getUint8(o + 1), v.getUint8(o + 2), v.getUint8(o + 3));
  if (tag(0) !== "RIFF" || tag(8) !== "WAVE") throw new Error("not a wav file");
  let o = 12;
  let channels = 0;
  let bits = 0;
  let format = 0;
  let sampleRate = 0;
  while (o + 8 <= v.byteLength) {
    const id = tag(o);
    const size = v.getUint32(o + 4, true);
    if (id === "fmt ") {
      format = v.getUint16(o + 8, true);
      channels = v.getUint16(o + 10, true);
      sampleRate = v.getUint32(o + 12, true);
      bits = v.getUint16(o + 22, true);
      if (format === 0xfffe && size >= 40) format = v.getUint16(o + 8 + 24, true); // extensible → subformat
    } else if (id === "data") {
      return { dataOffset: o + 8, channels, bits, format, sampleRate };
    }
    o += 8 + size + (size & 1);
  }
  throw new Error("wav data chunk not found");
}

export const player = new Player();
