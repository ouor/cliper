// 방송 전체 지도: 노래 곡선, 파형, 곡 막대, 후보 구간, 받은 영상 범위. Canvas, mipmapped peaks.
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ListMusic, Maximize2, SlidersHorizontal, ZoomIn, ZoomOut } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import { begin, colors, drawCurve, drawGrid, drawRuler, drawWave, hatch, panView, roundRect, useCanvasSize, useThemeVersion, zoomView } from "@/canvas/draw";
import { loadPeaks, loadProbs, mediaUrl, type Peaks, type Probs } from "@/lib/peaks";
import { clipCovered, reviewStatus, songNumbers, songTitle, videoState } from "@/lib/song";
import { useWs } from "@/state/ws";
import { useApp, updateSettings } from "@/state/app";
import { player, usePlayer } from "@/audio/player";
import { setEdge } from "@/lib/edits";
import { fmtTime } from "@/lib/format";
import { DetectPopover } from "./DetectPopover";
import { runningJobs, useJobs } from "@/state/jobs";

const GUTTER = 44;
const LANE_EXTRA = 16;
const LANE_PROB = 38;
const LANE_WAVE = 34;
const LANE_BARS = 26;
const BAND = 5;
const RULER = 18;

type Drag =
  | { kind: "edge"; songId: string; edge: "start" | "end"; t: number; orig: number; x0: number }
  | { kind: "create"; a: number; b: number; x0: number }
  | { kind: "pan"; x0: number; view: [number, number]; moved: boolean; t: number }
  | { kind: "bar"; songId: string; x0: number; shift: boolean };

export function useMapData(p: Project | undefined) {
  const [peaks, setPeaks] = useState<Peaks | null>(null);
  const [probs, setProbs] = useState<Probs | null>(null);
  const key = p ? `${p.id}|${p.detection ? "d" : ""}|${p.audio?.fetched_bytes ?? 0}|${p.audio?.elapsed_sec ?? 0}` : "";
  useEffect(() => {
    if (!p?.detection) {
      setPeaks(null);
      setProbs(null);
      return;
    }
    let alive = true;
    void loadPeaks(mediaUrl(p.id, "overview.peaks", key)).then((x) => alive && setPeaks(x)).catch(() => {});
    void loadProbs(mediaUrl(p.id, "probs.bin", key)).then((x) => alive && setProbs(x)).catch(() => {});
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  return { peaks, probs };
}

export function OverviewMap({ p }: { p: Project }) {
  const total = p.audio?.duration || p.source?.duration || 1;
  const { peaks, probs } = useMapData(p);
  const ws = useWs();
  const lanes = useApp((s) => s.settings.showLanes);
  const showCandidates = useApp((s) => s.settings.showCandidates);
  const jobs = useJobs();
  const theme = useThemeVersion();
  const { ref, w, h, dpr } = useCanvasSize();
  const playheadRef = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [hover, setHover] = useState<{ x: number; y: number; text: string } | null>(null);
  const [lanesOpen, setLanesOpen] = useState(false);
  const view: [number, number] = ws.mapView ?? [0, total];
  const nums = useMemo(() => songNumbers(p), [p]);
  const extraLanes = (["music", "speech", "sing"] as const).filter((k) => lanes[k]);

  const yProb = extraLanes.length * LANE_EXTRA;
  const yWave = yProb + LANE_PROB;
  const yBars = yWave + LANE_WAVE;
  const yBand = yBars + LANE_BARS;
  const yRuler = h - RULER;

  const xOf = useCallback((t: number) => ((t - view[0]) / (view[1] - view[0])) * w, [view, w]);
  const tOf = useCallback((x: number) => view[0] + (x / w) * (view[1] - view[0]), [view, w]);

  const audioJob = runningJobs(jobs, p.id, "audio.fetch")[0];
  const detectJob = runningJobs(jobs, p.id, "detect.run")[0];

  // keep the selected song visible
  useEffect(() => {
    const s = p.songs.find((x) => x.id === ws.selected);
    if (!s) return;
    const [a, b] = view;
    if (s.end < a || s.start > b) {
      const span = b - a;
      const c = (s.start + s.end) / 2;
      const na = Math.max(0, Math.min(total - span, c - span / 2));
      useWs.setState({ mapView: [na, na + span] });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ws.selected]);

  // draw
  useEffect(() => {
    const c = ref.current;
    if (!c || w < 2 || h < 2) return;
    const ctx = begin(c, dpr);
    const col = colors();
    const [t0, t1] = view;
    ctx.fillStyle = col["bg-0"];
    ctx.fillRect(0, 0, w, h);
    drawGrid(ctx, t0, t1, w, 0, yRuler, col);

    // selection column
    const sel = p.songs.find((s) => s.id === ws.selected);
    if (sel) {
      ctx.fillStyle = col["accent-soft"];
      const x0 = xOf(sel.start);
      ctx.fillRect(x0, 0, Math.max(2, xOf(sel.end) - x0), yBars);
    }

    // extra lanes
    if (probs) {
      extraLanes.forEach((k, i) => {
        const arr = k === "music" ? probs.music : k === "speech" ? probs.speech : probs.sing;
        const color = col[`lane-${k}`];
        drawCurve(ctx, arr, probs.fps, t0, t1, w, i * LANE_EXTRA + 2, LANE_EXTRA - 3, k === "sing" ? (v) => v / 0.15 : (v) => v, color, color + "33");
      });
      // 노래 곡선 (smoothed music − speech, the detector's margin)
      const hi = p.detection?.params.pann_hi ?? 0.2;
      const f = (v: number) => (v + 0.25) / 1.1;
      drawCurve(ctx, probs.margin, probs.fps, t0, t1, w, yProb + 3, LANE_PROB - 5, f, col.prob, col["prob-fill"], "mean");
      const yThr = yProb + 3 + (LANE_PROB - 5) * (1 - f(hi));
      ctx.setLineDash([3, 4]);
      ctx.strokeStyle = col["text-4"];
      ctx.beginPath();
      ctx.moveTo(0, Math.round(yThr) + 0.5);
      ctx.lineTo(w, Math.round(yThr) + 0.5);
      ctx.stroke();
      ctx.setLineDash([]);
    }
    // waveform
    if (peaks) {
      drawWave(ctx, peaks, 0, t0, t1, w, yWave + LANE_WAVE / 2, LANE_WAVE / 2 - 2, col.wave, 0.85);
    } else if (audioJob && audioJob.fraction !== null) {
      // fills from the left while the audio arrives
      ctx.fillStyle = col["accent-soft"];
      ctx.fillRect(0, yWave + 4, xOf(total * audioJob.fraction), LANE_WAVE - 8);
    }
    // lane separators
    ctx.strokeStyle = col["line-1"];
    ctx.beginPath();
    for (const y of [yWave, yBars, yRuler]) {
      ctx.moveTo(0, y + 0.5);
      ctx.lineTo(w, y + 0.5);
    }
    ctx.stroke();

    // candidates
    const cands = p.detection?.candidates ?? [];
    if (showCandidates) {
      cands.forEach((cd, i) => {
        const x0 = xOf(cd.start);
        const x1 = xOf(cd.end);
        if (x1 < 0 || x0 > w) return;
        ctx.setLineDash([3, 3]);
        ctx.strokeStyle = ws.candidate === i ? col["text-1"] : col.candidate;
        ctx.lineWidth = ws.candidate === i ? 1.5 : 1;
        roundRect(ctx, x0 + 0.5, yBars + 4.5, Math.max(3, x1 - x0 - 1), LANE_BARS - 9, 3);
        ctx.stroke();
        ctx.setLineDash([]);
        ctx.lineWidth = 1;
        if (x1 - x0 > 30) {
          ctx.fillStyle = col["text-3"];
          ctx.font = `10px ${col.font}`;
          ctx.textBaseline = "middle";
          ctx.fillText("후보", x0 + 5, yBars + LANE_BARS / 2);
        }
      });
    }

    // clip coverage bands (받은 영상 범위)
    for (const cl of p.clips) {
      const cov = clipCovered(cl);
      if (!cov) continue;
      ctx.fillStyle = col.clipband;
      ctx.fillRect(xOf(cov[0]), yBand + 1, Math.max(2, xOf(cov[1]) - xOf(cov[0])), BAND - 2);
    }

    // songs
    const songs = [...p.songs].sort((a, b) => a.start - b.start);
    const dragging = drag?.kind === "edge" ? drag : null;
    ctx.font = `600 10.5px ${col.font}`;
    ctx.textBaseline = "middle";
    for (const s of songs) {
      const st = reviewStatus(s);
      if (st === "excluded" && !ws.showExcluded) {
        // still hint where excluded songs are
        const x0 = xOf(s.start);
        const x1 = xOf(s.end);
        if (x1 < 0 || x0 > w) continue;
        ctx.fillStyle = hatch(ctx, col.excluded, "transparent", 5);
        ctx.fillRect(x0, yBars + 8, Math.max(2, x1 - x0), LANE_BARS - 16);
        continue;
      }
      let a = s.start;
      let b = s.end;
      if (dragging && dragging.songId === s.id) {
        if (dragging.edge === "start") a = Math.min(dragging.t, b - 1);
        else b = Math.max(dragging.t, a + 1);
      }
      const x0 = xOf(a);
      const x1 = xOf(b);
      if (x1 < -2 || x0 > w + 2) continue;
      const bw = Math.max(3, x1 - x0);
      const y = yBars + 3;
      const bh = LANE_BARS - 6;
      const overlap = songs.some((o) => o.id !== s.id && o.enabled && s.enabled && o.start < b && a < o.end);
      const color = st === "doubt" ? col.doubt : st === "excluded" ? col.excluded : col.accent;
      if (st === "excluded") {
        ctx.fillStyle = hatch(ctx, col.excluded, col["bg-2"], 5);
        roundRect(ctx, x0, y, bw, bh, 3);
        ctx.fill();
      } else if (st === "reviewed") {
        ctx.fillStyle = color;
        roundRect(ctx, x0, y, bw, bh, 3);
        ctx.fill();
      } else {
        ctx.fillStyle = st === "doubt" ? col["doubt-soft"] : col["accent-soft"];
        roundRect(ctx, x0 + 0.5, y + 0.5, bw - 1, bh - 1, 3);
        ctx.fill();
        ctx.strokeStyle = color;
        ctx.lineWidth = 1.5;
        ctx.stroke();
        ctx.lineWidth = 1;
      }
      if (overlap) {
        ctx.strokeStyle = col.danger;
        ctx.lineWidth = 2;
        roundRect(ctx, x0 - 1, y - 1, bw + 2, bh + 2, 4);
        ctx.stroke();
        ctx.lineWidth = 1;
      }
      const selected = s.id === ws.selected || ws.multi.includes(s.id);
      if (selected) {
        ctx.strokeStyle = col["text-1"];
        ctx.lineWidth = 2;
        roundRect(ctx, x0 - 1.5, y - 1.5, bw + 3, bh + 3, 4);
        ctx.stroke();
        ctx.lineWidth = 1;
        // edge handles
        ctx.fillStyle = col["text-1"];
        ctx.fillRect(x0 - 2, y + 3, 3, bh - 6);
        ctx.fillRect(x0 + bw - 1, y + 3, 3, bh - 6);
      }
      if (videoState(p, s) === "refetch") {
        ctx.fillStyle = col.doubt;
        ctx.fillRect(x0, yBand + 1, bw, BAND - 2);
      }
      // label
      const n = nums.get(s.id);
      const label = bw > 90 ? `${n ? String(n).padStart(2, "0") + " " : ""}${songTitle(s)}` : bw > 18 && n ? String(n).padStart(2, "0") : "";
      if (label) {
        ctx.save();
        ctx.beginPath();
        ctx.rect(x0 + 4, y, bw - 8, bh);
        ctx.clip();
        ctx.fillStyle = st === "reviewed" ? col["text-inv"] ?? "#000" : col["text-1"];
        if (st === "reviewed") ctx.fillStyle = "rgba(0,0,0,0.78)";
        ctx.fillText(label, x0 + 5, y + bh / 2 + 0.5);
        ctx.restore();
      }
    }
    // pending (drag-to-add) range
    const pr = drag?.kind === "create" ? [Math.min(drag.a, drag.b), Math.max(drag.a, drag.b)] : ws.pendingRange;
    if (pr) {
      const x0 = xOf(pr[0]);
      const x1 = xOf(pr[1]);
      ctx.fillStyle = col["accent-soft"];
      ctx.fillRect(x0, 0, x1 - x0, yBand);
      ctx.setLineDash([4, 3]);
      ctx.strokeStyle = col.accent;
      ctx.lineWidth = 1.5;
      roundRect(ctx, x0, yBars + 3, x1 - x0, LANE_BARS - 6, 3);
      ctx.stroke();
      ctx.setLineDash([]);
      ctx.lineWidth = 1;
    }
    drawRuler(ctx, t0, t1, w, yRuler, RULER, col, (t, step) => fmtTime(t, step < 1 ? 1 : 0));
  }, [p, peaks, probs, view, w, h, dpr, ws.selected, ws.multi, ws.candidate, ws.pendingRange, ws.showExcluded, drag, theme, extraLanes.length, showCandidates, audioJob?.fraction, nums, xOf, yBars, yBand, yProb, yRuler, yWave, total, ref, lanes]);

  // playhead (no React re-render per frame)
  useEffect(() => {
    return player.onFrame((pos) => {
      const el = playheadRef.current;
      if (!el) return;
      const x = ((pos - view[0]) / (view[1] - view[0])) * w;
      el.style.transform = `translateX(${x}px)`;
      el.style.display = x < 0 || x > w ? "none" : "block";
      // follow while playing
      if (player.now() > view[1] && usePlayer.getState().playing) {
        const span = view[1] - view[0];
        const na = Math.min(total - span, pos - span * 0.1);
        useWs.setState({ mapView: [na, na + span] });
      }
    });
  }, [view, w, total]);

  const hitSong = (x: number) => {
    const songs = p.songs.filter((s) => s.enabled || ws.showExcluded);
    // edges of the selected song first, then any song
    const order = [...songs].sort((a, b) => (a.id === ws.selected ? -1 : b.id === ws.selected ? 1 : 0));
    for (const s of order) {
      const x0 = xOf(s.start);
      const x1 = xOf(s.end);
      if (Math.abs(x - x0) <= 5 && x1 - x0 > 8) return { s, edge: "start" as const };
      if (Math.abs(x - x1) <= 5 && x1 - x0 > 8) return { s, edge: "end" as const };
    }
    for (const s of songs) if (x >= xOf(s.start) - 1 && x <= xOf(s.end) + 1) return { s, edge: null };
    return null;
  };

  const hitCandidate = (x: number) => (p.detection?.candidates ?? []).findIndex((c) => x >= xOf(c.start) - 2 && x <= xOf(c.end) + 2);

  const onPointerDown = (e: React.PointerEvent) => {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;
    const t = tOf(x);
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    if (y >= yBars && y < yRuler) {
      const hit = hitSong(x);
      if (hit?.edge) {
        useWs.setState({ selected: hit.s.id, lastEdge: hit.edge });
        setDrag({ kind: "edge", songId: hit.s.id, edge: hit.edge, t: hit.s[hit.edge], orig: hit.s[hit.edge], x0: x });
        return;
      }
      if (hit) {
        setDrag({ kind: "bar", songId: hit.s.id, x0: x, shift: e.shiftKey });
        return;
      }
      const ci = showCandidates ? hitCandidate(x) : -1;
      if (ci >= 0) {
        const cd = p.detection!.candidates[ci];
        useWs.setState({ candidate: ci, pendingRange: null });
        player.seek(cd.start);
        return;
      }
      setDrag({ kind: "create", a: t, b: t, x0: x });
      return;
    }
    setDrag({ kind: "pan", x0: x, view, moved: false, t });
  };

  const onPointerMove = (e: React.PointerEvent) => {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;
    const t = tOf(x);
    if (!drag) {
      // hover feedback
      const el = e.currentTarget as HTMLElement;
      if (y >= yBars && y < yRuler) {
        const hit = hitSong(x);
        el.style.cursor = hit?.edge ? "ew-resize" : hit ? "pointer" : "crosshair";
        if (hit) {
          const others = p.songs.filter((o) => o.id !== hit.s.id && o.enabled && hit.s.enabled && o.start < hit.s.end && hit.s.start < o.end);
          const n = nums.get(hit.s.id);
          const text = `${n ? String(n).padStart(2, "0") + " " : ""}${songTitle(hit.s)} · ${fmtTime(hit.s.start, 0)} ~ ${fmtTime(hit.s.end, 0)}${others.length ? `\n곡 ${others.map((o) => String(nums.get(o.id) ?? "").padStart(2, "0")).join(", ")}과 겹쳐요` : ""}`;
          setHover({ x, y, text });
        } else {
          const ci = showCandidates ? hitCandidate(x) : -1;
          if (ci >= 0) {
            const cd = p.detection!.candidates[ci];
            const why = { too_short: "짧아서", no_singing: "노래가 약해서", talk: "말소리가 많아서" }[cd.reason];
            setHover({ x, y, text: `놓친 곡일 수 있어요 (${why} 뺐어요)\n클릭해서 들어 보고 A로 추가` });
            el.style.cursor = "pointer";
          } else setHover({ x, y, text: "드래그해서 곡 추가" });
        }
      } else {
        el.style.cursor = "default";
        setHover(null);
      }
      return;
    }
    if (drag.kind === "edge") {
      const snapped = e.altKey ? t : Math.round(t * 10) / 10;
      setDrag({ ...drag, t: Math.max(0, Math.min(total, snapped)) });
      setHover({ x, y, text: `${fmtTime(snapped, 1)}  (${snapped - drag.orig >= 0 ? "+" : ""}${(snapped - drag.orig).toFixed(1)}초)` });
    } else if (drag.kind === "create") {
      setDrag({ ...drag, b: Math.max(0, Math.min(total, t)) });
      setHover({ x, y, text: `${fmtTime(Math.min(drag.a, t), 0)} ~ ${fmtTime(Math.max(drag.a, t), 0)}` });
    } else if (drag.kind === "pan") {
      const dx = x - drag.x0;
      if (Math.abs(dx) > 3 || drag.moved) {
        const dt = (-dx / w) * (drag.view[1] - drag.view[0]);
        useWs.setState({ mapView: panView(drag.view, dt, total) });
        if (!drag.moved) setDrag({ ...drag, moved: true });
      }
    }
  };

  const onPointerUp = (e: React.PointerEvent) => {
    const d = drag;
    setDrag(null);
    setHover(null);
    if (!d) return;
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const x = e.clientX - rect.left;
    if (d.kind === "edge") {
      if (Math.abs(d.t - d.orig) > 0.05) void setEdge(p.id, d.songId, d.edge, d.t);
    } else if (d.kind === "create") {
      const a = Math.min(d.a, d.b);
      const b = Math.max(d.a, d.b);
      if (Math.abs(x - d.x0) > 4 && b - a > 1) useWs.setState({ pendingRange: [a, b], candidate: null });
      else {
        useWs.setState({ pendingRange: null, candidate: null });
        player.seek(tOf(x));
      }
    } else if (d.kind === "pan") {
      if (!d.moved) {
        player.seek(d.t);
        useWs.setState({ pendingRange: null, candidate: null });
      }
    } else if (d.kind === "bar") {
      const st = useWs.getState();
      if (d.shift) {
        const base = st.multi.length ? st.multi : st.selected ? [st.selected] : [];
        const multi = base.includes(d.songId) ? base.filter((x) => x !== d.songId) : [...base, d.songId];
        useWs.setState({ multi, selected: d.songId });
      } else useWs.setState({ selected: d.songId, multi: [], candidate: null, pendingRange: null });
    }
  };

  const onWheel = (e: React.WheelEvent) => {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const t = tOf(e.clientX - rect.left);
    if (e.ctrlKey) {
      useWs.setState({ mapView: zoomView(view, t, e.deltaY > 0 ? 1.25 : 0.8, total, 60) });
    } else {
      const d = Math.abs(e.deltaX) > Math.abs(e.deltaY) ? e.deltaX : e.deltaY;
      useWs.setState({ mapView: panView(view, (d / w) * (view[1] - view[0]), total) });
    }
  };

  const zoom = (f: number) => {
    const at = player.now() >= view[0] && player.now() <= view[1] ? player.now() : (view[0] + view[1]) / 2;
    useWs.setState({ mapView: zoomView(view, at, f, total, 60) });
  };

  const startMapResize = (e: React.PointerEvent) => {
    const y0 = e.clientY;
    const h0 = ws.mapHeight;
    const move = (ev: PointerEvent) => useWs.setState({ mapHeight: Math.max(130, Math.min(420, h0 - (ev.clientY - y0))) });
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
  };

  const laneLabels = [
    ...extraLanes.map((k) => ({ y: 0, h: LANE_EXTRA, text: { music: "음악", speech: "말소리", sing: "가창" }[k] })),
    { y: 0, h: LANE_PROB, text: "노래" },
    { y: 0, h: LANE_WAVE, text: "파형" },
    { y: 0, h: LANE_BARS + BAND, text: "곡" },
  ];
  const zoomPct = Math.round((total / (view[1] - view[0])) * 100);

  return (
    <section className="map" style={{ height: ws.mapHeight }}>
      <div className="map-resize" onPointerDown={startMapResize} />
      <div className="map-head">
        <span className="map-title">방송 전체 지도</span>
        <div className="legend">
          <span className="lg reviewed">확인</span>
          <span className="lg unreviewed">미확인</span>
          <span className="lg doubt">애매함</span>
          <span className="lg cand">후보</span>
          <span className="lg band">받은 영상</span>
        </div>
        <span className="spacer" />
        <div className="popover-anchor">
          <button className="btn xs ghost" onClick={() => setLanesOpen(!lanesOpen)}>
            표시 ▾
          </button>
          {lanesOpen && (
            <div className="popover" onMouseLeave={() => setLanesOpen(false)}>
              {(["music", "speech", "sing"] as const).map((k) => (
                <label key={k} className="check">
                  <input type="checkbox" checked={lanes[k]} onChange={(e) => void updateSettings({ showLanes: { ...lanes, [k]: e.target.checked } })} />
                  {{ music: "음악 곡선", speech: "말소리 곡선", sing: "가창 곡선" }[k]}
                </label>
              ))}
              <label className="check">
                <input type="checkbox" checked={showCandidates} onChange={(e) => void updateSettings({ showCandidates: e.target.checked })} />
                후보 구간
              </label>
              <label className="check">
                <input type="checkbox" checked={ws.showExcluded} onChange={(e) => useWs.setState({ showExcluded: e.target.checked })} />
                제외한 곡 막대
              </label>
            </div>
          )}
        </div>
        <button className="btn xs ghost" onClick={() => useWs.setState({ setlistOpen: true })} disabled={!p.songs.length}>
          <ListMusic size={13} /> 세트리스트 붙여넣기
        </button>
        <div className="popover-anchor">
          <button className="btn xs ghost" onClick={() => useWs.setState({ detectOpen: !ws.detectOpen })} disabled={!p.detection}>
            <SlidersHorizontal size={13} /> 찾기 설정 · 다시 찾기
          </button>
          {ws.detectOpen && <DetectPopover p={p} />}
        </div>
        <span className="map-zoom faint">{zoomPct}%</span>
        <button className="btn icon xs ghost" onClick={() => zoom(1.5)} aria-label="축소" data-tip="축소 (Ctrl+휠)" data-tip-pos="left">
          <ZoomOut size={14} />
        </button>
        <button className="btn icon xs ghost" onClick={() => zoom(1 / 1.5)} aria-label="확대" data-tip="확대 (Ctrl+휠)" data-tip-pos="left">
          <ZoomIn size={14} />
        </button>
        <button className="btn icon xs ghost" onClick={() => useWs.setState({ mapView: null })} aria-label="전체 보기" data-tip="전체 보기" data-tip-pos="left">
          <Maximize2 size={13} />
        </button>
      </div>
      <div className="map-body">
        <div className="map-gutter" style={{ width: GUTTER }}>
          {laneLabels.map((l, i) => (
            <div key={i} style={{ height: l.h }}>
              {l.text}
            </div>
          ))}
        </div>
        <div className="map-canvas" onPointerDown={onPointerDown} onPointerMove={onPointerMove} onPointerUp={onPointerUp} onPointerLeave={() => !drag && setHover(null)} onWheel={onWheel} onDoubleClick={(e) => {
          const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
          const hit = hitSong(e.clientX - rect.left);
          if (hit) void player.play(Math.max(0, hit.s.start - useApp.getState().settings.listenPreroll));
        }}>
          <canvas ref={ref} />
          <div className="playhead" ref={playheadRef} />
          {hover && (
            <div className="map-tip" style={{ left: Math.min(hover.x + 12, w - 200), top: Math.max(0, hover.y - 34) }}>
              {hover.text}
            </div>
          )}
          {!p.detection && (
            <div className="map-state">
              {audioJob ? (
                <span>
                  소리를 받는 중이에요 · {audioJob.fraction !== null ? `${Math.floor(audioJob.fraction * 100)}%` : "준비 중"}
                </span>
              ) : detectJob ? (
                <span className="scan">
                  <i />
                  노래를 찾는 중이에요{detectJob.fraction !== null ? ` · ${Math.floor(detectJob.fraction * 100)}%` : ""}
                </span>
              ) : (
                <span className="faint">곡 찾기를 하면 방송 전체 지도가 나타나요.</span>
              )}
            </div>
          )}
          {detectJob && p.detection && <div className="scanline" />}
        </div>
      </div>
    </section>
  );
}
