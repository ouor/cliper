// Main-thread controller for the preview worklet: one AudioContext for the whole app,
// sources (VOD stream / stems window), transport (J/K/L), listening modes, video sync.
import { create } from "zustand";
import workletUrl from "./player.worklet.ts?worker&url";

export const WORKLET_URL = new URL(workletUrl, location.href).href;
import type { Mute } from "@ipc/generated/Mute";
import { mediaUrl } from "@/lib/peaks";
import { call } from "@/lib/rpc";

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
  /** stems window currently loaded (`paged`: a long song held a page at a time) */
  stems: { projectId: string; dir: string; start: number; end: number; progress: number; paged?: boolean; error?: string } | null;
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
/** longest stems window held in memory (2 × int16 stereo = 8 bytes/frame → ~115 MB); longer songs page */
const STEMS_MAX_SEC = 300;
/** a new page starts this far before the playhead; paging starts this close to the page end */
const STEMS_LEAD = 30;
const STEMS_EDGE = 15;
/** VideoPane starts the preloaded next file this long before a file boundary (s): a paused
 *  <video> needs a moment after play(). 35 ms measured to land the switch within ±10 ms (QA-9) */
export const VIDEO_START_LEAD = 0.035;

interface VideoBinding {
  el: HTMLVideoElement;
  /** VOD seconds at video t = 0 (`video.view` origin) */
  origin: number;
  /** VOD range the file has pictures for */
  start: number;
  end: number;
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
  private stemsGen = 0;
  /** the requested stems range (frames from the clip start); a long one is held a page at a time */
  private stemsRange: { key: string; projectId: string; dir: string; origin: number; f0: number; f1: number; bust: string; capped: boolean } | null = null;
  private stemsKey = "";
  private video: VideoBinding | null = null;
  private loopTouched = false;
  /** analysis-audio chunks that failed: when and how often (backoff, U-5) */
  private chunkFail = new Map<number, { at: number; n: number }>();
  /** last time the picture was stepped in scrub mode (fast playback, U-7) */
  private lastScrub = 0;
  /** stems mode: project / duration whose analysis audio fills what the span doesn't cover */
  private fallbackProject: string | null = null;
  private fallbackDuration = 0;

  private listeners = new Set<(pos: number) => void>();
  private dbgResolve: ((v: unknown) => void) | null = null;

  private rmsWaiters = new Map<number, (v: (number | null)[]) => void>();
  private rmsId = 0;

  /** Accompaniment level (dBFS) inside each VOD range of the loaded stems window. */
  accompanimentRms(ranges: [number, number][]): Promise<(number | null)[]> {
    const id = ++this.rmsId;
    return new Promise((r) => {
      this.rmsWaiters.set(id, r);
      this.post({ type: "accRms", id, ranges });
    });
  }

  /** Worklet internals (QA). */
  debug(): Promise<unknown> {
    return new Promise((r) => {
      this.dbgResolve = r;
      this.post({ type: "dbg" });
    });
  }

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
      this.post({ type: "loop", loop: st.loopOn && st.loop ? st.loop : null });
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
    } else if (m.type === "accRms") {
      this.rmsWaiters.get(m.id)?.(m.out);
      this.rmsWaiters.delete(m.id);
    } else if (m.type === "dbg") {
      this.dbgResolve?.(m);
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
    if (st.source === "stems" && st.playing) this.pageStems(pos, st.rate);
    if (st.source === "stems" && st.playing) this.prefetchOutsideStems(pos, st.rate);
    this.syncVideo(pos, st.playing && !st.buffering, st.rate);
    requestAnimationFrame(this.tick);
  };

  /** Stems mode: fetch analysis-audio chunks for where the playhead is / goes outside the span. */
  private prefetchOutsideStems(pos: number, rate: number) {
    if (!this.fallbackProject || this.streamProject !== this.fallbackProject) return;
    const st = usePlayer.getState();
    const r = this.stemsRange;
    if (!r) return;
    const rs = r.origin + r.f0 / SR;
    const re = r.origin + r.f1 / SR;
    const targets = [pos];
    if (rate !== 0) targets.push(pos + 6 * Math.sign(rate));
    if (st.loop && st.loopOn) targets.push(st.loop[0]);
    for (const t of targets) if (t < rs || t > re) this.prefetch(t, rate || 1);
  }

  /** Long stems ranges: move the page when the playhead leaves it or nears its end. */
  private pageStems(pos: number, rate: number) {
    const r = this.stemsRange;
    const st = usePlayer.getState().stems;
    if (!r?.capped || !st || st.progress < 1) return;
    const rs = r.origin + r.f0 / SR;
    const re = r.origin + r.f1 / SR;
    if (pos < rs || pos > re) return;
    const outside = pos < st.start || pos > st.end;
    const nearEnd = rate > 0 && st.end < re - 0.01 && pos > st.end - STEMS_EDGE;
    const nearStart = rate < 0 && st.start > rs + 0.01 && pos < st.start + STEMS_EDGE;
    if (outside || nearEnd) void this.loadStemsPage(pos, false);
    else if (nearStart) void this.loadStemsPage(pos - STEMS_MAX_SEC + 2 * STEMS_LEAD, false);
  }

  // ── sources ──

  async useStream(projectId: string, duration: number) {
    await this.init();
    if (this.streamProject === projectId && usePlayer.getState().source === "stream") return;
    this.fallbackProject = null;
    this.stopStems();
    for (const c of this.inflight.values()) c.abort();
    this.inflight.clear();
    this.sent.clear();
    this.streamProject = projectId;
    this.streamDuration = duration;
    this.post({ type: "stream:init", chunkSec: CHUNK, duration });
    this.stemsRange = null;
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
      // a chunk that failed waits 1, 2, 4 … 30 s before the next try (U-5)
      const f = this.chunkFail.get(i);
      if (f && performance.now() - f.at < Math.min(30000, 1000 * 2 ** f.n)) continue;
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

  /** One review-preview chunk: the engine cuts a sample-exact WAV (`audio.window`), we read it. */
  private fetchChunk(i: number) {
    const pid = this.streamProject!;
    const ac = new AbortController();
    this.inflight.set(i, ac);
    const start = i * CHUNK;
    const duration = Math.min(CHUNK, this.streamDuration - start);
    call("audio.window", { id: pid, start, duration })
      .then(async (w) => {
        if (ac.signal.aborted || this.streamProject !== pid) return;
        const r = await fetch(mediaUrl(pid, w.file), { signal: ac.signal });
        if (!r.ok) throw new Error(`HTTP ${r.status}`);
        const buf = await r.arrayBuffer();
        if (this.streamProject !== pid) return;
        const { dataOffset, channels, bits, format, sampleRate } = parseWavHeader(new DataView(buf, 0, Math.min(buf.byteLength, 4096)));
        if (channels !== 2 || bits !== 32 || format !== 3 || sampleRate !== SR) throw new Error(`unexpected window format (${channels}ch ${bits}bit ${sampleRate}Hz)`);
        const frames = Math.floor((buf.byteLength - dataOffset) / 8);
        const data = new Float32Array(buf.slice(dataOffset, dataOffset + frames * 8));
        this.post({ type: "stream:chunk", index: i, data }, [data.buffer]);
        this.sent.add(i);
        this.chunkFail.delete(i);
        if (usePlayer.getState().streamError) usePlayer.setState({ streamError: undefined });
      })
      .catch((e) => {
        if ((e as Error).name === "AbortError") return;
        const busy = (e as { engineCode?: string }).engineCode === "BUSY";
        const f = this.chunkFail.get(i);
        this.chunkFail.set(i, { at: performance.now(), n: (f?.n ?? 0) + 1 });
        const msg = busy ? "소리를 받는 중이라 아직 미리듣기를 할 수 없어요" : String((e as Error).message ?? e);
        if (usePlayer.getState().streamError !== msg) usePlayer.setState({ streamError: msg });
      })
      .finally(() => this.inflight.delete(i));
  }

  private stopStems() {
    this.stemsAbort?.abort();
    this.stemsAbort = null;
    this.stemsKey = "";
  }

  /**
   * Load a window [start, end] (VOD s) of a stem span (`dir` = project-relative folder with
   * orig.wav / vocals.wav). `origin` = VOD time of the span's frame 0; `clipFrames` bounds the
   * window. `fallbackDuration` > 0: outside the span the worklet plays the analysis audio
   * (original only — not separated there yet, 14.8) fetched like the review stream.
   */
  async useStems(projectId: string, dir: string, origin: number, clipFrames: number, start: number, end: number, bust = "", fallbackDuration = 0) {
    await this.init();
    this.fallbackProject = fallbackDuration > 0 ? projectId : null;
    this.fallbackDuration = fallbackDuration;
    this.post({ type: "fallback", on: fallbackDuration > 0, chunkSec: CHUNK, duration: fallbackDuration });
    if (this.stemsKey && this.streamProject !== this.fallbackProject) {
      // the same stems window stays, only the fallback changed (e.g. audio re-fetched, U-4)
      this.streamProject = this.fallbackProject;
      this.streamDuration = this.fallbackDuration;
      this.sent.clear();
    }
    const rf0 = Math.max(0, Math.floor((start - origin) * SR));
    const rf1 = Math.min(clipFrames, Math.ceil((end - origin) * SR));
    const rangeKey = `${projectId}/${dir}/${rf0}/${rf1}/${bust}`;
    if (this.stemsRange?.key === rangeKey) return;
    // reuse when the requested range is inside the loaded (uncapped) one
    const cur = usePlayer.getState().stems;
    const r = this.stemsRange;
    if (r && !r.capped && cur && !cur.error && r.projectId === projectId && r.dir === dir && r.bust === bust && r.f0 <= rf0 && r.f1 >= rf1) return;
    const capped = rf1 - rf0 > STEMS_MAX_SEC * SR;
    this.stemsRange = { key: rangeKey, projectId, dir, origin, f0: rf0, f1: rf1, bust, capped };
    const now = this.now();
    const anchor = now >= start && now <= end ? now : start;
    await this.loadStemsPage(anchor, true);
  }

  /** Load the page of the current stems range around VOD time `anchor` (the whole range when it is short). */
  private async loadStemsPage(anchor: number, fresh: boolean) {
    const r = this.stemsRange;
    if (!r) return;
    let f0 = r.f0;
    let f1 = r.f1;
    if (r.capped) {
      const max = STEMS_MAX_SEC * SR;
      f0 = Math.max(r.f0, Math.floor((anchor - STEMS_LEAD - r.origin) * SR));
      f1 = Math.min(r.f1, f0 + max);
      f0 = Math.max(r.f0, f1 - max);
    }
    const { projectId, dir, origin, bust } = r;
    const key = `${projectId}/${dir}/${f0}/${f1}/${bust}`;
    if (key === this.stemsKey) return;
    this.stopStems();
    this.stemsKey = key;
    // outside the span: analysis audio chunks (fallback), else nothing
    this.streamProject = this.fallbackProject;
    this.streamDuration = this.fallbackDuration;
    for (const c of this.inflight.values()) c.abort();
    this.inflight.clear();
    this.sent.clear();
    const frames = Math.max(0, f1 - f0);
    const wStart = origin + f0 / SR;
    const wEnd = origin + f1 / SR;
    // a new song pauses; paging inside a long song keeps playing (the worklet waits for data)
    if (fresh && usePlayer.getState().playing) this.pause();
    // allocate here (not on the audio thread) and hand the buffers to the worklet
    const gen = ++this.stemsGen;
    const ob = new Int16Array(frames * 2);
    const vb = new Int16Array(frames * 2);
    this.post({ type: "stems:init", gen, origin: wStart, frames, orig: ob, voc: vb }, [ob.buffer, vb.buffer]);
    usePlayer.setState({ source: "stems", stems: { projectId, dir, start: wStart, end: wEnd, progress: 0, paged: r.capped } });
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
      const url = mediaUrl(projectId, `${dir}/${which === 0 ? "orig" : "vocals"}.wav`);
      // header first (small range) to find the data chunk
      const head = await fetch(url, { headers: { range: "bytes=0-4095" }, signal: ac.signal });
      const hb = new DataView(await head.arrayBuffer());
      const { dataOffset, channels, bits, format, sampleRate } = parseWavHeader(hb);
      if (channels !== 2 || bits !== 32 || format !== 3 || sampleRate !== SR) throw new Error(`unsupported wav (${channels}ch ${bits}bit fmt ${format} ${sampleRate}Hz)`);
      const b0 = dataOffset + f0 * 8;
      const b1 = dataOffset + f1 * 8 - 1;
      const r = await fetch(url, { headers: { range: `bytes=${b0}-${b1}` }, signal: ac.signal });
      if (!r.body) throw new Error("no body");
      const reader = r.body.getReader();
      let carry = new Uint8Array(0);
      let samplesDone = 0;
      for (;;) {
        const { done, value } = await reader.read();
        if (done || ac.signal.aborted || gen !== this.stemsGen) break;
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
        const n = i16.length; // read before the transfer detaches the buffer
        this.post({ type: "stems:data", gen, which, offset: samplesDone, data: i16 }, [i16.buffer]);
        samplesDone += n;
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
    this.fallbackProject = null;
    this.stopStems();
    this.stemsRange = null;
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
    if (usePlayer.getState().source === "stems") {
      this.pageStems(p, 0);
      this.prefetchOutsideStems(p, 0);
    }
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
      mutes: mutes.map((m) => ({ s: m.start, e: m.end, fi: m.fade_in, fo: m.fade_out, floor: m.gain_db === null ? 0 : m.gain_db < 0 ? 10 ** (m.gain_db / 20) : 1 })),
    });
    this.post({ type: "muteOn", on: muteOn });
  }

  /**
   * Move the loop range. Never changes whether looping is on: that is only the user's
   * toggle (`toggleLoop`, Ctrl+L), remembered across sessions (#3).
   */
  setLoop(range: [number, number] | null) {
    const loopOn = usePlayer.getState().loopOn;
    usePlayer.setState({ loop: range });
    this.post({ type: "loop", loop: loopOn && range ? range : null });
  }

  toggleLoop() {
    this.loopTouched = true;
    this.applyLoopOn(!usePlayer.getState().loopOn);
    void window.cliper?.setState("loopOn", usePlayer.getState().loopOn);
  }

  private applyLoopOn(on: boolean) {
    const st = usePlayer.getState();
    usePlayer.setState({ loopOn: on });
    this.post({ type: "loop", loop: on && st.loop ? st.loop : null });
  }

  /** Restore the remembered loop on/off (unless the user already toggled it this session). */
  async restoreLoopPref() {
    const v = await window.cliper?.getState<boolean>("loopOn").catch(() => null);
    if (typeof v === "boolean" && !this.loopTouched) this.applyLoopOn(v);
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

  /** Bind a <video> playing a `video.view` file: currentTime = vod − origin, valid in [start, end]. */
  attachVideo(el: HTMLVideoElement | null, origin = 0, start = 0, end = 0) {
    if (!el) {
      this.video = null;
      return;
    }
    this.video = { el, origin, start, end };
    el.muted = true;
  }

  private syncVideo(pos: number, playing: boolean, rate: number) {
    const v = this.video;
    if (!v || v.el.readyState < 1) return;
    const t = pos - v.origin;
    // outside the view the pane covers the picture (VideoPane); just stop the element
    if (pos < v.start - 0.05 || pos > v.end + 0.05) {
      if (!v.el.paused) v.el.pause();
      return;
    }
    if (playing && Math.abs(rate) >= 1.5) {
      // fast playback: a 1440p60 decoder can't keep up and drifts (QA-19, U-7). Show
      // keyframe-near stills on the audio clock instead: no drift, no seek storm.
      if (!v.el.paused) v.el.pause();
      const now = performance.now();
      if (now - this.lastScrub > 120 && !v.el.seeking) {
        this.lastScrub = now;
        const el = v.el as HTMLVideoElement & { fastSeek?: (t: number) => void };
        if (el.fastSeek) el.fastSeek(t);
        else el.currentTime = t;
      }
    } else if (playing && rate > 0) {
      if (v.el.playbackRate !== rate) v.el.playbackRate = rate;
      // (a file switch starts the next element early — VideoPane, QA-9 — because seeking or changing
      // playbackRate to catch up stalls the picture in Chromium for about as long as it saves)
      if (v.el.paused) void v.el.play().catch(() => {});
      if (Math.abs(v.el.currentTime - t) > 0.15 && !v.el.seeking) v.el.currentTime = t;
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
void player.restoreLoopPref();
