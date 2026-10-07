// 곡 상세 타임라인 (멘트 다듬기): 원본 · 목소리 · 멘트 구간 · 노래 띠.
import { useEffect, useRef, useState } from "react";
import type { Clip } from "@ipc/generated/Clip";
import type { Mute } from "@ipc/generated/Mute";
import type { Project } from "@ipc/generated/Project";
import type { Song } from "@ipc/generated/Song";
import { begin, colors, drawGrid, drawRuler, drawWave, hatch, panView, useCanvasSize, useThemeVersion, zoomView } from "@/canvas/draw";
import type { Peaks, Probs } from "@/lib/peaks";
import { clipCovered, SING_THRESHOLD } from "@/lib/song";
import { fmtTime } from "@/lib/format";
import { useWs } from "@/state/ws";
import { player, usePlayer, LISTEN_LABEL } from "@/audio/player";
import { addMute, setEdge, updateMute } from "@/lib/edits";
import { useApp } from "@/state/app";

const LANE_ORIG = 70;
const LANE_VOC = 70;
const LANE_MUTE = 40;
const LANE_SING = 12;
const RULER = 20;
const HEAD = 6;

type Drag =
  | { kind: "create"; a: number; b: number; x0: number }
  | { kind: "mute"; id: string; part: "start" | "end" | "body" | "fadeIn" | "fadeOut"; t0: number; orig: Mute; cur: Mute; moved: boolean }
  | { kind: "edge"; edge: "start" | "end"; t: number; orig: number }
  | { kind: "pan"; x0: number; view: [number, number]; moved: boolean; t: number };

export function DetailTimeline({ p, s, clip, orig, voc, probs, muteOn }: { p: Project; s: Song; clip: Clip; orig: Peaks | null; voc: Peaks | null; probs: Probs | null; muteOn: boolean }) {
  const ws = useWs();
  const mode = usePlayer((x) => x.mode);
  const loopOn = usePlayer((x) => x.loopOn);
  const loop = usePlayer((x) => x.loop);
  const theme = useThemeVersion();
  const { ref, w, h, dpr } = useCanvasSize();
  const playheadRef = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [tip, setTip] = useState<{ x: number; y: number; text: string } | null>(null);
  const cov = clipCovered(clip)!;
  const origin = clip.video!.vod_start + clip.offset;
  const lo = Math.max(cov[0], s.start - p.settings.margin_before);
  const hi = Math.min(cov[1], s.end + p.settings.margin_after);
  const view: [number, number] = ws.detailView ?? [lo, hi];
  const span = view[1] - view[0];
  const xOf = (t: number) => ((t - view[0]) / span) * w;
  const tOf = (x: number) => view[0] + (x / w) * span;
  const yOrig = HEAD;
  const yVoc = yOrig + LANE_ORIG;
  const yMute = yVoc + LANE_VOC;
  const ySing = yMute + LANE_MUTE;
  const yRuler = ySing + LANE_SING + 4;

  // reset the view when the song changes
  useEffect(() => {
    useWs.setState({ detailView: null });
  }, [s.id]);

  const mutesForDraw = (): Mute[] => {
    if (drag?.kind === "mute") return s.mutes.map((m) => (m.id === drag.id ? drag.cur : m));
    return s.mutes;
  };

  useEffect(() => {
    const c = ref.current;
    if (!c || w < 2) return;
    const ctx = begin(c, dpr);
    const col = colors();
    const [t0, t1] = view;
    ctx.fillStyle = col["bg-0"];
    ctx.fillRect(0, 0, w, h);
    drawGrid(ctx, t0, t1, w, 0, yRuler, col, s.start);

    // song range (outside is dimmed after drawing)
    let sa = s.start;
    let sb = s.end;
    if (drag?.kind === "edge") {
      if (drag.edge === "start") sa = Math.min(drag.t, sb - 1);
      else sb = Math.max(drag.t, sa + 1);
    }
    // waves
    if (orig) drawWave(ctx, orig, origin, t0, t1, w, yOrig + LANE_ORIG / 2, LANE_ORIG / 2 - 3, col.wave, 1.2);
    const mutes = mutesForDraw();
    if (voc) drawWave(ctx, voc, origin, t0, t1, w, yVoc + LANE_VOC / 2, LANE_VOC / 2 - 3, col["wave-vocal"], 1.4);
    // mute overlay on the voice lane
    for (const m of mutes) {
      const x0 = xOf(m.start);
      const x1 = xOf(m.end);
      if (x1 < 0 || x0 > w) continue;
      ctx.fillStyle = muteOn ? (m.confirmed ? col["mute-soft"] : col["suggest-soft"]) : col["grid-strong"];
      ctx.fillRect(x0, yVoc, x1 - x0, LANE_VOC);
    }
    // song fade curves on the original lane (export fades)
    const fi = s.fade_in ?? p.settings.song_fade_in;
    const fo = s.fade_out ?? p.settings.song_fade_out;
    ctx.strokeStyle = col["text-2"];
    ctx.setLineDash([2, 3]);
    ctx.beginPath();
    const yT = yOrig + 3;
    const yB = yOrig + LANE_ORIG - 3;
    for (let i = 0; i <= 24; i++) {
      const u = i / 24;
      const x = xOf(sa + u * fi);
      const g = 0.5 - 0.5 * Math.cos(Math.PI * u);
      if (i === 0) ctx.moveTo(x, yB - (yB - yT) * g);
      else ctx.lineTo(x, yB - (yB - yT) * g);
    }
    for (let i = 0; i <= 24; i++) {
      const u = i / 24;
      const x = xOf(sb - fo + u * fo);
      const g = 0.5 + 0.5 * Math.cos(Math.PI * u);
      ctx.lineTo(x, yB - (yB - yT) * g);
    }
    ctx.stroke();
    ctx.setLineDash([]);

    // mute lane
    ctx.fillStyle = col["bg-1"];
    ctx.fillRect(0, yMute, w, LANE_MUTE);
    for (const m of mutes) {
      const x0 = xOf(m.start);
      const x3 = xOf(m.end);
      if (x3 < -4 || x0 > w + 4) continue;
      let fin = m.fade_in;
      let fout = m.fade_out;
      const len = m.end - m.start;
      if (fin + fout > len) {
        const k = len / (fin + fout);
        fin *= k;
        fout *= k;
      }
      const x1 = xOf(m.start + fin);
      const x2 = xOf(m.end - fout);
      const top = yMute + 6;
      const bot = yMute + LANE_MUTE - 5;
      const floorY = m.gain_db === null ? top : top + (bot - top) * (1 - Math.min(1, -m.gain_db / 30)) * 0.7;
      ctx.beginPath();
      ctx.moveTo(x0, bot);
      ctx.lineTo(x1, floorY);
      ctx.lineTo(x2, floorY);
      ctx.lineTo(x3, bot);
      ctx.closePath();
      const selected = m.id === ws.selectedMute;
      if (!muteOn) ctx.fillStyle = hatch(ctx, col.excluded, col["bg-2"], 6);
      else if (m.confirmed) ctx.fillStyle = col.mute;
      else ctx.fillStyle = hatch(ctx, col.suggest, col["suggest-soft"], 6);
      ctx.fill();
      ctx.strokeStyle = selected ? col["text-1"] : m.confirmed ? col.mute : col.suggest;
      ctx.lineWidth = selected ? 2 : 1;
      ctx.stroke();
      ctx.lineWidth = 1;
      if (selected) {
        // fade handles
        ctx.fillStyle = col["text-1"];
        for (const x of [x1, x2]) {
          ctx.beginPath();
          ctx.arc(x, floorY, 4, 0, Math.PI * 2);
          ctx.fill();
        }
      }
      if (m.origin === "manual" && x3 - x0 > 16) {
        ctx.fillStyle = m.confirmed ? "rgba(255,255,255,0.85)" : col["text-1"];
        ctx.font = `10px ${col.font}`;
        ctx.textBaseline = "middle";
        ctx.fillText("✋", Math.max(x0, 0) + 3, yMute + LANE_MUTE / 2 + 1);
      }
    }
    // pending create
    if (drag?.kind === "create") {
      const a = xOf(Math.min(drag.a, drag.b));
      const b = xOf(Math.max(drag.a, drag.b));
      ctx.fillStyle = col["mute-soft"];
      ctx.fillRect(a, yVoc, b - a, LANE_VOC + LANE_MUTE);
      ctx.strokeStyle = col.mute;
      ctx.setLineDash([4, 3]);
      ctx.strokeRect(a + 0.5, yMute + 6.5, b - a - 1, LANE_MUTE - 12);
      ctx.setLineDash([]);
    }
    // B mark
    if (ws.markStart !== null) {
      const x = xOf(ws.markStart);
      ctx.strokeStyle = col.mute;
      ctx.setLineDash([2, 2]);
      ctx.beginPath();
      ctx.moveTo(x + 0.5, yVoc);
      ctx.lineTo(x + 0.5, ySing);
      ctx.stroke();
      ctx.setLineDash([]);
    }
    // singing band
    if (probs) {
      const step = Math.max(1, Math.floor(w / ((span * probs.fps) || 1)));
      for (let x = 0; x < w; x += step) {
        const t = tOf(x);
        const i = Math.floor(t * probs.fps);
        const v = probs.sing[i] ?? 0;
        const a = Math.max(0, Math.min(1, v / (SING_THRESHOLD * 2)));
        if (a < 0.05) continue;
        ctx.fillStyle = col["lane-sing"];
        ctx.globalAlpha = 0.15 + a * 0.75;
        ctx.fillRect(x, ySing + 2, step, LANE_SING - 4);
      }
      ctx.globalAlpha = 1;
    }
    // loop range
    if (loop && loopOn) {
      ctx.fillStyle = col["info"];
      ctx.globalAlpha = 0.5;
      ctx.fillRect(xOf(loop[0]), 0, Math.max(1, xOf(loop[1]) - xOf(loop[0])), 3);
      ctx.globalAlpha = 1;
    }
    // dim outside the song
    ctx.fillStyle = "rgba(0,0,0,0.38)";
    const xa = xOf(sa);
    const xb = xOf(sb);
    if (xa > 0) ctx.fillRect(0, 0, xa, yRuler);
    if (xb < w) ctx.fillRect(xb, 0, w - xb, yRuler);
    // song edge handles ◤ ◥
    ctx.fillStyle = col.accent;
    for (const [x, dir] of [
      [xa, 1],
      [xb, -1],
    ] as const) {
      if (x < -10 || x > w + 10) continue;
      ctx.fillRect(x - (dir > 0 ? 0 : 2), 0, 2, yRuler);
      ctx.beginPath();
      ctx.moveTo(x, 0);
      ctx.lineTo(x + dir * 12, 0);
      ctx.lineTo(x, 12);
      ctx.closePath();
      ctx.fill();
    }
    ctx.font = `600 10px ${col.font}`;
    ctx.fillStyle = col.accent;
    ctx.textBaseline = "top";
    if (xa > -60 && xa < w) ctx.fillText("곡 시작", xa + 14, 1);
    if (xb > 0 && xb < w + 40) ctx.fillText("곡 끝", xb - 40, 1);
    // separators
    ctx.strokeStyle = col["line-1"];
    ctx.beginPath();
    for (const y of [yVoc, yMute, ySing]) {
      ctx.moveTo(0, y + 0.5);
      ctx.lineTo(w, y + 0.5);
    }
    ctx.stroke();
    drawRuler(ctx, t0, t1, w, yRuler, RULER, col, (t, step) => fmtTime(t - s.start, step < 0.1 ? 2 : step < 1 ? 1 : 0), s.start);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [p, s, orig, voc, probs, view, w, h, dpr, drag, ws.selectedMute, ws.markStart, theme, muteOn, loop, loopOn]);

  useEffect(
    () =>
      player.onFrame((pos) => {
        const el = playheadRef.current;
        if (!el) return;
        const x = ((pos - view[0]) / (view[1] - view[0])) * w;
        el.style.transform = `translateX(${x}px)`;
        el.style.display = x < 0 || x > w ? "none" : "block";
        if (usePlayer.getState().playing && pos > view[1] && !usePlayer.getState().loopOn) {
          const sp = view[1] - view[0];
          const na = Math.min(cov[1] - sp, pos - sp * 0.1);
          useWs.setState({ detailView: [na, na + sp] });
        }
      }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [view[0], view[1], w],
  );

  const snapPoints = (excludeId?: string) => {
    const pts = [s.start, s.end];
    for (const m of s.mutes) if (m.id !== excludeId) pts.push(m.start, m.end);
    return pts;
  };
  const snap = (t: number, alt: boolean, excludeId?: string) => {
    if (alt) return t;
    const tol = (8 / w) * span;
    let best = t;
    let bd = tol;
    for (const q of snapPoints(excludeId)) {
      const d = Math.abs(q - t);
      if (d < bd) (bd = d), (best = q);
    }
    return best;
  };

  const hitMute = (x: number, y: number): { m: Mute; part: "start" | "end" | "body" | "fadeIn" | "fadeOut" } | null => {
    const sel = s.mutes.find((m) => m.id === ws.selectedMute);
    const order = sel ? [sel, ...s.mutes.filter((m) => m !== sel)] : s.mutes;
    for (const m of order) {
      const x0 = xOf(m.start);
      const x3 = xOf(m.end);
      if (x < x0 - 6 || x > x3 + 6) continue;
      if (m === sel) {
        const len = m.end - m.start;
        const k = m.fade_in + m.fade_out > len ? len / (m.fade_in + m.fade_out) : 1;
        if (Math.abs(x - xOf(m.start + m.fade_in * k)) <= 6 && y < yMute + 20) return { m, part: "fadeIn" };
        if (Math.abs(x - xOf(m.end - m.fade_out * k)) <= 6 && y < yMute + 20) return { m, part: "fadeOut" };
      }
      if (Math.abs(x - x0) <= 5) return { m, part: "start" };
      if (Math.abs(x - x3) <= 5) return { m, part: "end" };
      if (x >= x0 && x <= x3) return { m, part: "body" };
    }
    return null;
  };

  const local = (e: React.PointerEvent | React.MouseEvent) => {
    const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
    return { x: e.clientX - r.left, y: e.clientY - r.top };
  };

  const onDown = (e: React.PointerEvent) => {
    const { x, y } = local(e);
    const t = tOf(x);
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    // song edges (top area of every lane)
    if (y < yRuler && Math.abs(x - xOf(s.start)) <= 6 && (y < 16 || y < yMute)) {
      setDrag({ kind: "edge", edge: "start", t: s.start, orig: s.start });
      return;
    }
    if (y < yRuler && Math.abs(x - xOf(s.end)) <= 6 && (y < 16 || y < yMute)) {
      setDrag({ kind: "edge", edge: "end", t: s.end, orig: s.end });
      return;
    }
    if (y >= yMute && y < ySing) {
      const hit = hitMute(x, y);
      if (hit) {
        useWs.setState({ selectedMute: hit.m.id });
        setDrag({ kind: "mute", id: hit.m.id, part: hit.part, t0: t, orig: hit.m, cur: hit.m, moved: false });
        return;
      }
    }
    if (y >= yVoc && y < ySing) {
      setDrag({ kind: "create", a: snap(t, e.altKey), b: snap(t, e.altKey), x0: x });
      return;
    }
    setDrag({ kind: "pan", x0: x, view, moved: false, t });
  };

  const onMove = (e: React.PointerEvent) => {
    const { x, y } = local(e);
    const t = tOf(x);
    const el = e.currentTarget as HTMLElement;
    if (!drag) {
      let cursor = "default";
      if (y < yRuler && (Math.abs(x - xOf(s.start)) <= 6 || Math.abs(x - xOf(s.end)) <= 6) && y < yMute) cursor = "ew-resize";
      else if (y >= yMute && y < ySing) {
        const hit = hitMute(x, y);
        cursor = !hit ? "crosshair" : hit.part === "body" ? "grab" : "ew-resize";
        if (hit) {
          const m = hit.m;
          setTip({ x, y, text: `${m.confirmed ? (m.origin === "manual" ? "직접 만듦" : "확인함") : "제안 · 확인 전"} · ${fmtTime(m.start - s.start, 2)} ~ ${fmtTime(m.end - s.start, 2)}${hit.part === "fadeIn" || hit.part === "fadeOut" ? "\n드래그해서 페이드 길이 조절" : ""}` });
        } else setTip(null);
      } else if (y >= yVoc && y < yMute) {
        cursor = "crosshair";
        setTip(null);
      } else setTip(null);
      el.style.cursor = cursor;
      return;
    }
    if (drag.kind === "create") {
      setDrag({ ...drag, b: snap(t, e.altKey) });
      setTip({ x, y, text: `${fmtTime(Math.min(drag.a, t) - s.start, 2)} ~ ${fmtTime(Math.max(drag.a, t) - s.start, 2)}` });
    } else if (drag.kind === "edge") {
      const v = e.altKey ? t : Math.round(t * 100) / 100;
      setDrag({ ...drag, t: v });
      setTip({ x, y, text: `${drag.edge === "start" ? "곡 시작" : "곡 끝"} ${fmtTime(v, 2, true)} (${v - drag.orig >= 0 ? "+" : ""}${(v - drag.orig).toFixed(2)}초)` });
    } else if (drag.kind === "mute") {
      const o = drag.orig;
      const dt = t - drag.t0;
      const cur = { ...o };
      if (drag.part === "body") {
        const len = o.end - o.start;
        let a = snap(o.start + dt, e.altKey, o.id);
        if (a === o.start + dt) a = snap(o.end + dt, e.altKey, o.id) - len;
        a = Math.max(cov[0], Math.min(cov[1] - len, a));
        cur.start = a;
        cur.end = a + len;
      } else if (drag.part === "start") cur.start = Math.max(cov[0], Math.min(o.end - 0.05, snap(o.start + dt, e.altKey, o.id)));
      else if (drag.part === "end") cur.end = Math.min(cov[1], Math.max(o.start + 0.05, snap(o.end + dt, e.altKey, o.id)));
      else if (drag.part === "fadeIn") cur.fade_in = Math.max(0.005, Math.min(2, o.end - o.start, o.fade_in + dt));
      else cur.fade_out = Math.max(0.005, Math.min(2, o.end - o.start, o.fade_out - dt));
      setDrag({ ...drag, cur, moved: drag.moved || Math.abs(dt) > span / w });
      const txt = drag.part === "fadeIn" ? `앞 페이드 ${Math.round(cur.fade_in * 1000)}ms` : drag.part === "fadeOut" ? `뒤 페이드 ${Math.round(cur.fade_out * 1000)}ms` : `${fmtTime(cur.start - s.start, 2)} ~ ${fmtTime(cur.end - s.start, 2)}`;
      setTip({ x, y, text: txt });
    } else if (drag.kind === "pan") {
      const dx = x - drag.x0;
      if (Math.abs(dx) > 3 || drag.moved) {
        useWs.setState({ detailView: panView(drag.view, (-dx / w) * (drag.view[1] - drag.view[0]), cov[1], cov[0]) });
        if (!drag.moved) setDrag({ ...drag, moved: true });
      }
    }
  };

  const onUp = (e: React.PointerEvent) => {
    const d = drag;
    setDrag(null);
    setTip(null);
    if (!d) return;
    const { x } = local(e);
    if (d.kind === "create") {
      const a = Math.min(d.a, d.b);
      const b = Math.max(d.a, d.b);
      if (Math.abs(x - d.x0) > 4 && b - a > 0.05) void addMute(p.id, s.id, Math.max(cov[0], a), Math.min(cov[1], b));
      else player.seek(tOf(x));
    } else if (d.kind === "edge") {
      if (Math.abs(d.t - d.orig) > 0.005) void setEdge(p.id, s.id, d.edge, d.t);
    } else if (d.kind === "mute") {
      if (d.moved) {
        const c = d.cur;
        void updateMute(p.id, s.id, d.id, d.part === "fadeIn" || d.part === "fadeOut" ? "페이드 길이" : d.part === "body" ? "멘트 구간 이동" : "멘트 구간 경계", (m) => {
          m.start = c.start;
          m.end = c.end;
          m.fade_in = c.fade_in;
          m.fade_out = c.fade_out;
        });
      } else {
        selectMuteAndLoop(d.orig);
      }
    } else if (d.kind === "pan" && !d.moved) {
      player.seek(d.t);
    }
  };

  const onWheel = (e: React.WheelEvent) => {
    const { x } = local(e);
    if (e.ctrlKey) useWs.setState({ detailView: zoomView(view, tOf(x), e.deltaY > 0 ? 1.25 : 0.8, cov[1], 0.4) });
    else {
      const dd = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
      useWs.setState({ detailView: panView(view, (dd / w) * span, cov[1], cov[0]) });
    }
  };

  // zoomView clamps to [0,total]; keep the lower bound at the clip start
  useEffect(() => {
    if (ws.detailView && ws.detailView[0] < cov[0]) {
      const sp = ws.detailView[1] - ws.detailView[0];
      useWs.setState({ detailView: [cov[0], Math.min(cov[1], cov[0] + sp)] });
    }
  }, [ws.detailView, cov]);

  const labels = [
    { h: LANE_ORIG, t: "원본" },
    { h: LANE_VOC, t: "목소리" },
    { h: LANE_MUTE, t: "멘트" },
    { h: LANE_SING + 4, t: "노래" },
  ];
  return (
    <div className="detail-tl">
      <div className="tl-gutter">
        <div style={{ height: HEAD }} />
        {labels.map((l) => (
          <div key={l.t} style={{ height: l.h }}>
            {l.t}
          </div>
        ))}
      </div>
      <div className="tl-canvas" onPointerDown={onDown} onPointerMove={onMove} onPointerUp={onUp} onPointerLeave={() => !drag && setTip(null)} onWheel={onWheel}>
        <canvas ref={ref} />
        <div className="playhead" ref={playheadRef} />
        <div className={`mode-badge m${mode}`}>듣는 중 · {LISTEN_LABEL[mode]}</div>
        {tip && (
          <div className="map-tip" style={{ left: Math.min(tip.x + 12, w - 220), top: Math.max(0, tip.y - 36) }}>
            {tip.text}
          </div>
        )}
      </div>
    </div>
  );
}

/** Select a mute: loop "구간 앞 3초 ~ 뒤 3초" and seek there. */
export function selectMuteAndLoop(m: Mute, play = false) {
  const pad = useApp.getState().settings.loopPad;
  useWs.setState({ selectedMute: m.id });
  player.setLoop([Math.max(0, m.start - pad), m.end + pad], true);
  if (play) void player.play(Math.max(0, m.start - pad));
  else if (!usePlayer.getState().playing) player.seek(Math.max(0, m.start - pad));
}
