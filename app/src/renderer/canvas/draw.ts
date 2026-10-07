// Canvas helpers shared by the overview map and the detail timeline.
import { useEffect, useRef, useState } from "react";
import { peaksLevel, type Peaks } from "@/lib/peaks";
import { fmtTime } from "@/lib/format";

export interface Colors {
  [k: string]: string;
}

let colorCache: Colors | null = null;
const colorListeners = new Set<() => void>();
new MutationObserver(() => {
  colorCache = null;
  for (const l of colorListeners) l();
}).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

const VARS = [
  "bg-0", "bg-1", "bg-2", "bg-3", "bg-4", "line-1", "line-2", "line-3", "text-1", "text-2", "text-3", "text-4",
  "accent", "accent-soft", "accent-line", "doubt", "doubt-soft", "danger", "danger-soft", "success", "info",
  "mute", "mute-soft", "suggest", "suggest-soft", "wave", "wave-strong", "wave-vocal", "wave-vocal-soft",
  "prob", "prob-fill", "lane-music", "lane-speech", "lane-sing", "candidate", "excluded", "clipband", "playhead", "grid", "grid-strong",
];

export function colors(): Colors {
  if (colorCache) return colorCache;
  const cs = getComputedStyle(document.documentElement);
  const c: Colors = {};
  for (const v of VARS) c[v] = cs.getPropertyValue(`--${v}`).trim();
  c.font = cs.getPropertyValue("--font-ui").trim();
  colorCache = c;
  return c;
}

export function useThemeVersion(): number {
  const [v, setV] = useState(0);
  useEffect(() => {
    const l = () => setV((x) => x + 1);
    colorListeners.add(l);
    return () => void colorListeners.delete(l);
  }, []);
  return v;
}

/** Canvas sized to its parent with devicePixelRatio. */
export function useCanvasSize() {
  const ref = useRef<HTMLCanvasElement>(null);
  const [size, setSize] = useState({ w: 0, h: 0, dpr: 1 });
  useEffect(() => {
    const el = ref.current?.parentElement;
    if (!el) return;
    const ro = new ResizeObserver(() => {
      const r = el.getBoundingClientRect();
      setSize({ w: Math.max(1, Math.floor(r.width)), h: Math.max(1, Math.floor(r.height)), dpr: window.devicePixelRatio || 1 });
    });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  useEffect(() => {
    const c = ref.current;
    if (!c) return;
    c.width = Math.round(size.w * size.dpr);
    c.height = Math.round(size.h * size.dpr);
    c.style.width = `${size.w}px`;
    c.style.height = `${size.h}px`;
  }, [size]);
  return { ref, ...size };
}

export function begin(c: HTMLCanvasElement, dpr: number): CanvasRenderingContext2D {
  const ctx = c.getContext("2d")!;
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, c.width, c.height);
  return ctx;
}

const STEPS = [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1, 2, 5, 10, 15, 30, 60, 120, 300, 600, 900, 1800, 3600, 7200];

export function tickStep(span: number, width: number, minPx = 90): number {
  for (const s of STEPS) if ((s / span) * width >= minPx) return s;
  return STEPS[STEPS.length - 1];
}

export function fmtTick(t: number, step: number): string {
  const digits = step < 0.1 ? 2 : step < 1 ? 1 : 0;
  return fmtTime(t, digits);
}

/** Time ruler along the bottom (or top) of a band. `label(t)` customises text. */
export function drawRuler(ctx: CanvasRenderingContext2D, t0: number, t1: number, w: number, y: number, h: number, c: Colors, label?: (t: number, step: number) => string) {
  const span = t1 - t0;
  const step = tickStep(span, w);
  const minor = step / 5;
  ctx.fillStyle = c["bg-1"];
  ctx.fillRect(0, y, w, h);
  ctx.strokeStyle = c["line-2"];
  ctx.beginPath();
  ctx.moveTo(0, y + 0.5);
  ctx.lineTo(w, y + 0.5);
  ctx.stroke();
  ctx.font = `10px ${c.font}`;
  ctx.textBaseline = "middle";
  ctx.fillStyle = c["text-3"];
  const first = Math.ceil(t0 / minor) * minor;
  ctx.beginPath();
  for (let t = first; t <= t1; t += minor) {
    const x = Math.round(((t - t0) / span) * w) + 0.5;
    const major = Math.abs(t / step - Math.round(t / step)) < 1e-6;
    ctx.moveTo(x, y);
    ctx.lineTo(x, y + (major ? 6 : 3));
    if (major) ctx.fillText((label ?? fmtTick)(Math.round(t / step) * step, step), x + 3, y + h / 2 + 2);
  }
  ctx.strokeStyle = c["line-3"];
  ctx.stroke();
}

export function drawGrid(ctx: CanvasRenderingContext2D, t0: number, t1: number, w: number, y: number, h: number, c: Colors) {
  const span = t1 - t0;
  const step = tickStep(span, w);
  ctx.strokeStyle = c.grid;
  ctx.beginPath();
  for (let t = Math.ceil(t0 / step) * step; t <= t1; t += step) {
    const x = Math.round(((t - t0) / span) * w) + 0.5;
    ctx.moveTo(x, y);
    ctx.lineTo(x, y + h);
  }
  ctx.stroke();
}

/**
 * Min/max waveform. `origin` = time of peak 0. Values are int16-scaled.
 * `gain` lifts quiet material so speech remains visible.
 */
export function drawWave(ctx: CanvasRenderingContext2D, p: Peaks, origin: number, t0: number, t1: number, w: number, yMid: number, half: number, color: string, gain = 1, from?: number, to?: number) {
  const span = t1 - t0;
  const peaksPerPx = (span * p.rate) / w;
  const { data, rate } = peaksLevel(p, peaksPerPx);
  const n = data.length / 2;
  ctx.fillStyle = color;
  const xa = from === undefined ? 0 : Math.max(0, Math.floor(((from - t0) / span) * w));
  const xb = to === undefined ? w : Math.min(w, Math.ceil(((to - t0) / span) * w));
  const k = (half / 32768) * gain;
  for (let x = xa; x < xb; x++) {
    const ta = t0 + (x / w) * span - origin;
    const tb = t0 + ((x + 1) / w) * span - origin;
    let i0 = Math.floor(ta * rate);
    let i1 = Math.ceil(tb * rate);
    if (i1 <= 0 || i0 >= n) continue;
    i0 = Math.max(0, i0);
    i1 = Math.min(n, Math.max(i1, i0 + 1));
    let lo = 32767;
    let hi = -32768;
    for (let i = i0; i < i1; i++) {
      const a = data[i * 2];
      const b = data[i * 2 + 1];
      if (a < lo) lo = a;
      if (b > hi) hi = b;
    }
    const top = Math.max(-half, -hi * k);
    const bot = Math.min(half, -lo * k);
    ctx.fillRect(x, yMid + top, 1, Math.max(1, bot - top));
  }
}

/** Area curve of `arr` (fps samples) mapped by `f` to 0..1. */
export function drawCurve(ctx: CanvasRenderingContext2D, arr: Float32Array, fps: number, t0: number, t1: number, w: number, y: number, h: number, f: (v: number) => number, stroke: string, fill?: string, agg: "max" | "mean" = "max") {
  const span = t1 - t0;
  const pts: number[] = [];
  const perPx = (span * fps) / w;
  for (let x = 0; x <= w; x++) {
    const t = t0 + (x / w) * span;
    let v: number;
    if (perPx > 1) {
      // max over the pixel's samples so short songs stay visible when zoomed out
      const i0 = Math.max(0, Math.floor(t * fps));
      const i1 = Math.min(arr.length, Math.ceil((t + span / w) * fps));
      v = -Infinity;
      if (agg === "mean") {
        let acc = 0;
        for (let i = i0; i < i1; i++) acc += arr[i];
        v = i1 > i0 ? acc / (i1 - i0) : -Infinity;
      } else for (let i = i0; i < i1; i++) if (arr[i] > v) v = arr[i];
      if (v === -Infinity) v = arr[Math.min(arr.length - 1, i0)] ?? 0;
    } else {
      const fi = t * fps;
      const i = Math.floor(fi);
      const a = arr[Math.max(0, Math.min(arr.length - 1, i))] ?? 0;
      const b = arr[Math.max(0, Math.min(arr.length - 1, i + 1))] ?? 0;
      v = a + (b - a) * (fi - i);
    }
    pts.push(x, y + h - Math.max(0, Math.min(1, f(v))) * h);
  }
  if (fill) {
    ctx.beginPath();
    ctx.moveTo(0, y + h);
    for (let i = 0; i < pts.length; i += 2) ctx.lineTo(pts[i], pts[i + 1]);
    ctx.lineTo(w, y + h);
    ctx.closePath();
    ctx.fillStyle = fill;
    ctx.fill();
  }
  ctx.beginPath();
  for (let i = 0; i < pts.length; i += 2) (i ? ctx.lineTo : ctx.moveTo).call(ctx, pts[i], pts[i + 1]);
  ctx.strokeStyle = stroke;
  ctx.lineWidth = 1.25;
  ctx.stroke();
  ctx.lineWidth = 1;
}

const patterns = new Map<string, CanvasPattern>();
export function hatch(ctx: CanvasRenderingContext2D, color: string, bg = "transparent", gap = 6): CanvasPattern {
  const key = `${color}|${bg}|${gap}`;
  let p = patterns.get(key);
  if (p) return p;
  const s = document.createElement("canvas");
  const dpr = window.devicePixelRatio || 1;
  s.width = s.height = Math.round(gap * dpr);
  const g = s.getContext("2d")!;
  g.scale(dpr, dpr);
  if (bg !== "transparent") {
    g.fillStyle = bg;
    g.fillRect(0, 0, gap, gap);
  }
  g.strokeStyle = color;
  g.lineWidth = 1.5;
  g.beginPath();
  g.moveTo(-1, gap + 1);
  g.lineTo(gap + 1, -1);
  g.moveTo(-1, 1);
  g.lineTo(1, -1);
  g.moveTo(gap - 1, gap + 1);
  g.lineTo(gap + 1, gap - 1);
  g.stroke();
  p = ctx.createPattern(s, "repeat")!;
  p.setTransform(new DOMMatrix().scale(1 / dpr));
  patterns.set(key, p);
  return p;
}

export function roundRect(ctx: CanvasRenderingContext2D, x: number, y: number, w: number, h: number, r: number) {
  const rr = Math.min(r, w / 2, h / 2);
  ctx.beginPath();
  ctx.roundRect(x, y, Math.max(0, w), h, rr);
}

/** Zoom around `at` (s) by `factor`, clamped to [0, total] and a minimum span. */
export function zoomView(view: [number, number], at: number, factor: number, total: number, minSpan: number): [number, number] {
  const [a, b] = view;
  const span = Math.min(total, Math.max(minSpan, (b - a) * factor));
  const r = (at - a) / (b - a);
  let na = at - r * span;
  na = Math.max(0, Math.min(total - span, na));
  return [na, na + span];
}

export function panView(view: [number, number], dt: number, total: number, lo = 0): [number, number] {
  const span = view[1] - view[0];
  const a = Math.max(lo, Math.min(total - span, view[0] + dt));
  return [a, a + span];
}
