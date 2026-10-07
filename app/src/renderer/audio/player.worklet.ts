// Cliper preview processor (AudioWorklet).
//
// Sources
//   stems  — a window of the clip's orig.wav / vocals.wav (int16 stereo, 48 kHz),
//            used by 멘트 다듬기 for the four listening modes;
//   stream — decoded windows of the whole-VOD audio (orig only), used by 곡 검수.
// Positions are VOD seconds everywhere.
//
// Listening modes (docs/ipc.md):
//   0 원본         orig
//   1 결과         orig − (1 − g(t))·vocals
//   2 지운 목소리  (1 − g(t))·vocals          (silent outside mutes)
//   3 반주         orig − vocals
// g(t): 1 outside mutes; inside, raised-cosine fade_in from 1 to the floor, the
// floor, raised-cosine fade_out back to 1 (fades inside [start,end]); floor = 0
// for gain_db null else 10^(gain_db/20); overlapping mutes → the smaller gain.

declare const sampleRate: number;
declare const currentTime: number;
declare function registerProcessor(name: string, ctor: unknown): void;
declare class AudioWorkletProcessor {
  readonly port: MessagePort;
  constructor();
}

interface MuteEnv {
  s: number;
  e: number;
  fi: number;
  fo: number;
  floor: number;
}

const XFADE = 256;
const I16 = 1 / 32768;

class CliperPlayer extends AudioWorkletProcessor {
  private src: "none" | "stems" | "stream" = "none";
  // stems window
  private origin = 0;
  private frames = 0;
  private orig: Int16Array = new Int16Array(0);
  private voc: Int16Array = new Int16Array(0);
  private loadedO = 0; // contiguous frames loaded from window start
  private loadedV = 0;
  // stream
  private chunkSec = 6;
  private chunks = new Map<number, Float32Array>();
  private duration = Infinity;

  private pos = 0;
  private playing = false;
  private rate = 1;
  private mode = 0;
  private prevMode = 0;
  private xf = 0;
  private mutes: MuteEnv[] = [];
  private muteOn = true;
  private loop: [number, number] | null = null;
  private stopAt: number | null = null;
  private volume = 1;
  private buffering = false;
  private sinceReport = 0;

  constructor() {
    super();
    this.port.onmessage = (ev: MessageEvent) => this.onMessage(ev.data);
  }

  private onMessage(m: any) {
    switch (m.type) {
      case "stems:init":
        this.src = "stems";
        this.origin = m.origin;
        this.frames = m.frames;
        this.orig = new Int16Array(m.frames * 2);
        this.voc = new Int16Array(m.frames * 2);
        this.loadedO = 0;
        this.loadedV = 0;
        this.chunks.clear();
        break;
      case "stems:data": {
        const arr = m.which === 0 ? this.orig : this.voc;
        const data = m.data as Int16Array;
        arr.set(data.subarray(0, Math.max(0, Math.min(data.length, arr.length - m.offset))), m.offset);
        const end = (m.offset + data.length) / 2;
        if (m.which === 0) this.loadedO = Math.max(this.loadedO, Math.min(this.frames, end));
        else this.loadedV = Math.max(this.loadedV, Math.min(this.frames, end));
        break;
      }
      case "stream:init":
        this.src = "stream";
        this.chunkSec = m.chunkSec;
        this.duration = m.duration || Infinity;
        this.chunks.clear();
        this.orig = new Int16Array(0);
        this.voc = new Int16Array(0);
        break;
      case "stream:chunk":
        this.chunks.set(m.index, m.data as Float32Array);
        break;
      case "stream:keep":
        for (const k of [...this.chunks.keys()]) if (k < m.lo || k > m.hi) this.chunks.delete(k);
        break;
      case "none":
        this.src = "none";
        this.chunks.clear();
        this.orig = new Int16Array(0);
        this.voc = new Int16Array(0);
        this.playing = false;
        break;
      case "play":
        if (typeof m.pos === "number") this.pos = m.pos;
        if (typeof m.rate === "number") this.rate = m.rate;
        this.stopAt = typeof m.stopAt === "number" ? m.stopAt : null;
        this.playing = true;
        this.report(true);
        break;
      case "pause":
        this.playing = false;
        this.stopAt = null;
        this.report(true);
        break;
      case "seek":
        this.pos = m.pos;
        this.report(true);
        break;
      case "rate":
        this.rate = m.rate;
        break;
      case "mode":
        if (m.mode !== this.mode) {
          this.prevMode = this.mode;
          this.mode = m.mode;
          this.xf = XFADE;
        }
        break;
      case "mutes":
        this.mutes = (m.mutes as MuteEnv[]).slice().sort((a, b) => a.s - b.s);
        break;
      case "muteOn":
        this.muteOn = !!m.on;
        break;
      case "loop":
        this.loop = m.loop;
        break;
      case "volume":
        this.volume = m.volume;
        break;
      case "accRms": {
        // exact accompaniment (orig − vocals) RMS in dBFS for each VOD range, null if not loaded
        const out = (m.ranges as [number, number][]).map(([a, b]) => {
          if (this.src !== "stems") return null;
          const f0 = Math.max(0, Math.floor((a - this.origin) * sampleRate));
          const f1 = Math.min(this.frames, Math.ceil((b - this.origin) * sampleRate));
          if (f1 <= f0 || f1 > Math.min(this.loadedO, this.loadedV)) return null;
          let acc = 0;
          for (let f = f0; f < f1; f++) {
            const l = (this.orig[f * 2] - this.voc[f * 2]) * I16;
            const r = (this.orig[f * 2 + 1] - this.voc[f * 2 + 1]) * I16;
            acc += l * l + r * r;
          }
          const rms = Math.sqrt(acc / ((f1 - f0) * 2));
          return rms > 0 ? 20 * Math.log10(rms) : -120;
        });
        this.port.postMessage({ type: "accRms", id: m.id, out });
        break;
      }
      case "dbg":
        this.port.postMessage({ type: "dbg", src: this.src, origin: this.origin, frames: this.frames, loadedO: this.loadedO, loadedV: this.loadedV, pos: this.pos, chunks: [...this.chunks.keys()], playing: this.playing });
        break;
    }
  }

  private report(force = false) {
    this.port.postMessage({ type: "pos", pos: this.pos, playing: this.playing, buffering: this.buffering, ctxTime: currentTime, rate: this.rate, force });
  }

  /** g(t) — the vocal keep factor. */
  private gain(t: number, active: MuteEnv[]): number {
    let g = 1;
    for (const m of active) {
      if (t < m.s || t > m.e) continue;
      let fi = m.fi;
      let fo = m.fo;
      const len = m.e - m.s;
      if (fi + fo > len && fi + fo > 0) {
        const k = len / (fi + fo);
        fi *= k;
        fo *= k;
      }
      let w: number;
      if (fi > 0 && t < m.s + fi) w = 0.5 * (1 + Math.cos((Math.PI * (t - m.s)) / fi));
      else if (fo > 0 && t > m.e - fo) w = 0.5 * (1 - Math.cos((Math.PI * (t - (m.e - fo))) / fo));
      else w = 0;
      const v = m.floor + (1 - m.floor) * w;
      if (v < g) g = v;
    }
    return g;
  }

  private mix(mode: number, oL: number, oR: number, vL: number, vR: number, g: number, out: Float32Array, i: number, right: Float32Array) {
    const k = this.muteOn ? 1 - g : 0;
    switch (mode) {
      case 1:
        out[i] = oL - k * vL;
        right[i] = oR - k * vR;
        break;
      case 2:
        out[i] = k * vL;
        right[i] = k * vR;
        break;
      case 3:
        out[i] = oL - vL;
        right[i] = oR - vR;
        break;
      default:
        out[i] = oL;
        right[i] = oR;
    }
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const L = outputs[0][0];
    const R = outputs[0][1] ?? outputs[0][0];
    const n = L.length;
    if (!this.playing || this.src === "none") {
      L.fill(0);
      R.fill(0);
      this.buffering = false;
      return true;
    }
    const step = this.rate / sampleRate;
    const t0 = this.pos;
    const t1 = this.pos + step * n;
    const lo = Math.min(t0, t1);
    const hi = Math.max(t0, t1);
    const active = this.mutes.filter((m) => m.e >= lo && m.s <= hi);
    const tmpL = new Float32Array(1);
    const tmpR = new Float32Array(1);
    let stalled = false;
    let ended = false;
    for (let i = 0; i < n; i++) {
      const t = this.pos;
      let oL = 0;
      let oR = 0;
      let vL = 0;
      let vR = 0;
      if (this.src === "stems") {
        const f = (t - this.origin) * sampleRate;
        if (f >= this.frames - 1 && !(this.loop && step > 0)) {
          ended = true;
          L.fill(0, i);
          R.fill(0, i);
          break;
        }
        if (f >= 0 && f < this.frames - 1) {
          const fi = Math.floor(f);
          if (fi + 1 >= this.loadedO || fi + 1 >= this.loadedV) {
            stalled = true;
            L.fill(0, i);
            R.fill(0, i);
            break;
          }
          const a = f - fi;
          const j = fi * 2;
          oL = (this.orig[j] + (this.orig[j + 2] - this.orig[j]) * a) * I16;
          oR = (this.orig[j + 1] + (this.orig[j + 3] - this.orig[j + 1]) * a) * I16;
          vL = (this.voc[j] + (this.voc[j + 2] - this.voc[j]) * a) * I16;
          vR = (this.voc[j + 1] + (this.voc[j + 3] - this.voc[j + 1]) * a) * I16;
        } else if (f < 0 && step < 0) {
          ended = true;
          L.fill(0, i);
          R.fill(0, i);
          break;
        }
        // outside the loaded window (e.g. loop padding before it): silence, keep moving
      } else {
        if (t < 0 || t >= this.duration) {
          ended = true;
          L.fill(0, i);
          R.fill(0, i);
          break;
        }
        const k = Math.floor(t / this.chunkSec);
        const c = this.chunks.get(k);
        const local = Math.floor((t - k * this.chunkSec) * sampleRate);
        if (!c || local * 2 + 1 >= c.length) {
          if (c && c.length < this.chunkSec * sampleRate * 2 - 4096) {
            // short final chunk: end of audio
            ended = true;
          } else stalled = true;
          L.fill(0, i);
          R.fill(0, i);
          break;
        }
        oL = c[local * 2];
        oR = c[local * 2 + 1];
      }
      const g = active.length ? this.gain(t, active) : 1;
      if (this.xf > 0) {
        this.mix(this.mode, oL, oR, vL, vR, g, L, i, R);
        this.mix(this.prevMode, oL, oR, vL, vR, g, tmpL, 0, tmpR);
        const w = this.xf / XFADE;
        L[i] = L[i] * (1 - w) + tmpL[0] * w;
        R[i] = R[i] * (1 - w) + tmpR[0] * w;
        this.xf--;
      } else {
        this.mix(this.mode, oL, oR, vL, vR, g, L, i, R);
      }
      L[i] *= this.volume;
      R[i] *= this.volume;
      this.pos += step;
      if (this.loop) {
        const [a, b] = this.loop;
        if (step > 0 && this.pos >= b) this.pos = a;
        else if (step < 0 && this.pos <= a) this.pos = b;
      } else if (this.stopAt !== null && ((step > 0 && this.pos >= this.stopAt) || (step < 0 && this.pos <= this.stopAt))) {
        this.playing = false;
        this.stopAt = null;
        L.fill(0, i + 1);
        R.fill(0, i + 1);
        this.report(true);
        break;
      }
    }
    if (ended) {
      this.playing = false;
      this.port.postMessage({ type: "ended", pos: this.pos });
      this.report(true);
    }
    const wasBuffering = this.buffering;
    this.buffering = stalled;
    this.sinceReport += n;
    if (this.sinceReport >= 1024 || wasBuffering !== stalled) {
      this.sinceReport = 0;
      this.report(wasBuffering !== stalled);
    }
    return true;
  }
}

registerProcessor("cliper-player", CliperPlayer);
