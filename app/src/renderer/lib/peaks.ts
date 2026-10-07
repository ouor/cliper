// CLPEAKS1 / CLPROBS1 readers (docs/ipc.md) + min/max mipmaps for fast drawing.

export interface Peaks {
  rate: number; // peaks per second
  count: number;
  /** level 0 = file data; level k = 2^k peaks merged. interleaved min,max as int16 */
  levels: Int16Array[];
}

export interface Probs {
  fps: number;
  duration: number;
  n: number;
  music: Float32Array;
  speech: Float32Array;
  sing: Float32Array;
  /** smoothed music − speech (what detection thresholds) */
  margin: Float32Array;
}

function magic(view: DataView, s: string): boolean {
  for (let i = 0; i < 8; i++) if (view.getUint8(i) !== s.charCodeAt(i)) return false;
  return true;
}

export function parsePeaks(buf: ArrayBuffer): Peaks {
  const v = new DataView(buf);
  if (buf.byteLength < 20 || !magic(v, "CLPEAKS1")) throw new Error("not a CLPEAKS1 file");
  const rate = v.getFloat64(8, true);
  const count = v.getUint32(16, true);
  const n = Math.min(count, Math.floor((buf.byteLength - 20) / 4));
  // data starts at byte 20 (not 2-aligned for Int16Array views → copy)
  const base = new Int16Array(n * 2);
  for (let i = 0; i < n * 2; i++) base[i] = v.getInt16(20 + i * 2, true);
  const levels = [base];
  let cur = base;
  while (cur.length / 2 > 64) {
    const m = Math.ceil(cur.length / 4);
    const next = new Int16Array(m * 2);
    for (let i = 0; i < m; i++) {
      const a = i * 4;
      let lo = cur[a];
      let hi = cur[a + 1];
      if (a + 2 < cur.length) {
        if (cur[a + 2] < lo) lo = cur[a + 2];
        if (cur[a + 3] > hi) hi = cur[a + 3];
      }
      next[i * 2] = lo;
      next[i * 2 + 1] = hi;
    }
    levels.push(next);
    cur = next;
  }
  return { rate, count: n, levels };
}

/** Smoothing in frames (centered moving average). */
function smooth(src: Float32Array, win: number): Float32Array {
  const out = new Float32Array(src.length);
  const h = Math.max(0, Math.floor(win / 2));
  let acc = 0;
  let cnt = 0;
  let lo = 0;
  let hi = -1;
  for (let i = 0; i < src.length; i++) {
    const a = Math.max(0, i - h);
    const b = Math.min(src.length - 1, i + h);
    while (hi < b) acc += src[++hi], cnt++;
    while (lo < a) acc -= src[lo++], cnt--;
    out[i] = acc / cnt;
  }
  return out;
}

export function parseProbs(buf: ArrayBuffer, smoothSec = 3): Probs {
  const v = new DataView(buf);
  if (buf.byteLength < 28 || !magic(v, "CLPROBS1")) throw new Error("not a CLPROBS1 file");
  const fps = v.getFloat64(8, true);
  const duration = v.getFloat64(16, true);
  const n = v.getUint32(24, true);
  const read = (k: number) => {
    const out = new Float32Array(n);
    const off = 28 + k * n * 4;
    for (let i = 0; i < n; i++) out[i] = v.getFloat32(off + i * 4, true);
    return out;
  };
  const music = read(0);
  const speech = read(1);
  const sing = read(2);
  const raw = new Float32Array(n);
  for (let i = 0; i < n; i++) raw[i] = music[i] - speech[i];
  const margin = smooth(raw, Math.round(smoothSec * fps));
  return { fps, duration, n, music, speech, sing, margin };
}

/** Pick the mipmap level so that one pixel covers ≥ 1 peak. */
export function peaksLevel(p: Peaks, peaksPerPixel: number): { data: Int16Array; rate: number } {
  let k = 0;
  while (k + 1 < p.levels.length && 2 ** (k + 1) <= peaksPerPixel) k++;
  return { data: p.levels[k], rate: p.rate / 2 ** k };
}

// ── cached loaders (by URL) ──
/**
 * Parsed files keyed by path (without the cache-bust): a new bust replaces the old copy, and
 * at most CACHE_MAX files stay in memory (least recently used dropped).
 */
const CACHE_MAX = 12;
const cache = new Map<string, { url: string; p: Promise<unknown> }>();

function cached<T>(url: string, parse: (b: ArrayBuffer) => T): Promise<T> {
  const key = url.split("?")[0];
  const hit = cache.get(key);
  if (hit && hit.url === url) {
    cache.delete(key);
    cache.set(key, hit); // most recent last
    return hit.p as Promise<T>;
  }
  const p = fetch(url).then(async (r) => {
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    return parse(await r.arrayBuffer());
  });
  p.catch(() => {
    if (cache.get(key)?.p === p) cache.delete(key);
  });
  cache.delete(key);
  cache.set(key, { url, p });
  while (cache.size > CACHE_MAX) cache.delete(cache.keys().next().value!);
  return p;
}

export function loadPeaks(url: string): Promise<Peaks> {
  return cached(url, parsePeaks);
}

export function loadProbs(url: string): Promise<Probs> {
  return cached(url, (b) => parseProbs(b));
}

export function invalidateMedia(prefix: string) {
  for (const k of [...cache.keys()]) if (k.startsWith(prefix)) cache.delete(k);
}

export function mediaUrl(projectId: string, rel: string, bust?: string): string {
  return `cliper-media://project/${encodeURIComponent(projectId)}/${rel}${bust ? `?v=${encodeURIComponent(bust)}` : ""}`;
}
