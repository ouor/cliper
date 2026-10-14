// 방송 전체 지도: 노래 곡선, 파형, 곡 막대, 후보 구간, 받은 영상 범위. Canvas, mipmapped peaks.
import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Download, ListMusic, Maximize2, SlidersHorizontal, ZoomIn, ZoomOut } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import { begin, colors, drawCurve, drawGrid, drawRuler, drawWave, hatch, panView, roundRect, useCanvasSize, useThemeVersion, zoomView } from "@/canvas/draw";
import { invalidateMedia, loadPeaks, loadProbs, mediaUrl, type Peaks, type Probs } from "@/lib/peaks";
import { reviewStatus, songNumbers, songTitle } from "@/lib/song";
import { coverageSummary, len, partsIn, piecesIn, requestsAt, songVideo, vodGone, estimate, fetchVideo } from "@/lib/video";
import { useVideo } from "@/state/video";
import { fmtBytes, fmtDurationKo } from "@/lib/format";
import { useWs } from "@/state/ws";
import { ask, useApp, updateSettings } from "@/state/app";
import { player, usePlayer } from "@/audio/player";
import { setEdge } from "@/lib/edits";
import { fmtTime } from "@/lib/format";
import { DetectPopover } from "./DetectPopover";
import { fetchSongsVideo } from "./VideoControls";
import { runningJobs, useJobs } from "@/state/jobs";
import { useShallow } from "zustand/react/shallow";

const GUTTER = 44;
const LANE_EXTRA = 16;
const LANE_PROB = 38;
const LANE_WAVE = 34;
const LANE_BARS = 26;
/** the video lane (14.3): have / fetching / queued / none / failed / unavailable */
const BAND = 10;
const RULER = 18;

type Drag =
  | { kind: "edge"; songId: string; edge: "start" | "end"; t: number; orig: number; x0: number }
  | { kind: "create"; a: number; b: number; x0: number }
  | { kind: "pan"; x0: number; view: [number, number]; moved: boolean; t: number }
  | { kind: "bar"; songId: string; x0: number; shift: boolean }
  | { kind: "vrange"; a: number; b: number; x0: number };

/**
 * Overview peaks + PANNs curves. While `audio.fetch` runs the engine rewrites overview.peaks every
 * few seconds and reports `peaks_until`; pass it so the waveform fills in live.
 */
export function useMapData(p: Project | undefined, peaksUntil?: number) {
  const [peaks, setPeaks] = useState<Peaks | null>(null);
  const [probs, setProbs] = useState<Probs | null>(null);
  const live = peaksUntil !== undefined && !p?.detection ? Math.floor(peaksUntil / 600) : -1;
  const key = p ? `${p.id}|${p.detection ? "d" : ""}|${p.audio?.fetched_bytes ?? 0}|${p.audio?.elapsed_sec ?? 0}|${live}` : "";
  useEffect(() => {
    if (!p || (!p.audio && live < 0)) {
      setPeaks(null);
      setProbs(null);
      return;
    }
    let alive = true;
    // growing file: drop older parsed copies so memory stays flat
    if (live >= 0) invalidateMedia(mediaUrl(p.id, "overview.peaks"));
    void loadPeaks(mediaUrl(p.id, "overview.peaks", key)).then((x) => alive && setPeaks(x)).catch(() => {});
    if (p.detection) void loadProbs(mediaUrl(p.id, "probs.bin", key)).then((x) => alive && setProbs(x)).catch(() => {});
    else setProbs(null);
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);
  return { peaks, probs };
}

export const OverviewMap = memo(function OverviewMap({ p }: { p: Project }) {
  const total = p.audio?.duration || p.source?.duration || 1;
  const ws = useWs(
    useShallow((s) => ({ mapView: s.mapView, selected: s.selected, multi: s.multi, candidate: s.candidate, pendingRange: s.pendingRange, showExcluded: s.showExcluded, mapHeight: s.mapHeight, detectOpen: s.detectOpen })),
  );
  const lanes = useApp((s) => s.settings.showLanes);
  const showCandidates = useApp((s) => s.settings.showCandidates);
  // only the two jobs the map shows (the store updates ~4 Hz while jobs run)
  const [audioJob, detectJob] = useJobs(useShallow((s) => [runningJobs(s, p.id, "audio.fetch")[0], runningJobs(s, p.id, "detect.run")[0]] as const));
  const theme = useThemeVersion();
  const vs = useVideo((s) => s.byProject[p.id]);
  const videoRange = useWs((s) => s.videoRange);
  const gone = vodGone(p);
  const { ref, w, h, dpr } = useCanvasSize();
  const playheadRef = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [hover, setHover] = useState<{ x: number; y: number; text: string } | null>(null);
  const [lanesOpen, setLanesOpen] = useState(false);
  // memoised: a fresh array each render made every hover/progress tick redraw the whole canvas
  const view: [number, number] = useMemo(() => ws.mapView ?? [0, total], [ws.mapView, total]);
  const nums = useMemo(() => songNumbers(p), [p]);
  const extraLanes = (["music", "speech", "sing"] as const).filter((k) => lanes[k]);

  const yProb = extraLanes.length * LANE_EXTRA;
  const yWave = yProb + LANE_PROB;
  const yBars = yWave + LANE_WAVE;
  const yBand = yBars + LANE_BARS;
  const yRuler = h - RULER;

  const xOf = useCallback((t: number) => ((t - view[0]) / (view[1] - view[0])) * w, [view, w]);
  const tOf = useCallback((x: number) => view[0] + (x / w) * (view[1] - view[0]), [view, w]);

  const { peaks, probs } = useMapData(p, audioJob?.peaksUntil);

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
      // while the audio arrives only [0, peaks_until) is real; the rest of the file is zeros
      const until = audioJob?.peaksUntil;
      drawWave(ctx, peaks, 0, t0, t1, w, yWave + LANE_WAVE / 2, LANE_WAVE / 2 - 2, col.wave, 0.85, undefined, until);
      if (until !== undefined && until < t1) {
        ctx.fillStyle = col["accent"];
        ctx.fillRect(Math.max(0, xOf(until)) - 1, yWave + 3, 2, LANE_WAVE - 6);
      }
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

    // video lane: solid = have, stripes = fetching, outline = queued, red dashes = failed,
    // grey hatch = can't be fetched (replay gone), empty = none — never colour alone (7.7)
    ctx.fillStyle = col["bg-1"];
    ctx.fillRect(0, yBand, w, BAND);
    for (const pc of piecesIn(vs, t0, t1, gone)) {
      const xa = Math.max(0, xOf(pc.a));
      const xb = Math.min(w, xOf(pc.b));
      const bw = Math.max(pc.state === "none" ? 0 : 2, xb - xa);
      const y0 = yBand + 2;
      const bh = BAND - 4;
      if (pc.state === "have") {
        ctx.fillStyle = col.clipband;
        ctx.fillRect(xa, y0, bw, bh);
      } else if (pc.state === "fetching") {
        ctx.fillStyle = hatch(ctx, col.clipband, col["accent-soft"], 4);
        ctx.fillRect(xa, y0, bw, bh);
      } else if (pc.state === "queued") {
        ctx.strokeStyle = col.clipband;
        ctx.strokeRect(xa + 0.5, y0 + 0.5, Math.max(1, bw - 1), bh - 1);
      } else if (pc.state === "failed") {
        ctx.strokeStyle = col.danger;
        ctx.setLineDash([3, 2]);
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.moveTo(xa, y0 + bh / 2);
        ctx.lineTo(xa + bw, y0 + bh / 2);
        ctx.stroke();
        ctx.setLineDash([]);
        ctx.lineWidth = 1;
      } else if (pc.state === "gone") {
        ctx.fillStyle = hatch(ctx, col.excluded, "transparent", 4);
        ctx.fillRect(xa, y0, bw, bh);
      }
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
      // song wider than its video: the bar's missing ends turn orange (14.3)
      if (s.enabled && st !== "excluded") {
        const sv = songVideo(p, vs, { ...s, start: a, end: b });
        if (sv.touched && sv.state !== "ready") {
          ctx.fillStyle = col.doubt;
          for (const [ma, mb] of sv.missing) ctx.fillRect(Math.max(x0, xOf(ma)), y + bh - 4, Math.max(2, Math.min(x0 + bw, xOf(mb)) - Math.max(x0, xOf(ma))), 4);
        }
      }
      // exported: ⬆ on the bar (14.10)
      if (s.export && bw > 44) {
        ctx.fillStyle = st === "reviewed" ? "rgba(0,0,0,0.78)" : col["text-1"];
        ctx.font = `700 10px ${col.font}`;
        ctx.fillText("⬆", x0 + bw - 12, y + bh / 2 + 0.5);
        ctx.font = `600 10.5px ${col.font}`;
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
    // Shift+drag video range
    const vr = drag?.kind === "vrange" ? [Math.min(drag.a, drag.b), Math.max(drag.a, drag.b)] : videoRange;
    if (vr) {
      const x0 = xOf(vr[0]);
      const x1 = xOf(vr[1]);
      ctx.fillStyle = col["accent-soft"];
      ctx.fillRect(x0, yBand - 2, x1 - x0, BAND + 4);
      ctx.strokeStyle = col.accent;
      ctx.setLineDash([4, 3]);
      ctx.strokeRect(x0 + 0.5, yBand - 1.5, x1 - x0 - 1, BAND + 3);
      ctx.setLineDash([]);
    }
    drawRuler(ctx, t0, t1, w, yRuler, RULER, col, (t, step) => fmtTime(t, step < 1 ? 1 : 0));
  }, [vs, gone, videoRange, p, peaks, probs, view, w, h, dpr, ws.selected, ws.multi, ws.candidate, ws.pendingRange, ws.showExcluded, drag, theme, extraLanes.length, showCandidates, audioJob?.fraction, audioJob?.peaksUntil, nums, xOf, yBars, yBand, yProb, yRuler, yWave, total, ref, lanes]);

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

  const bandTip = (t: number): string => {
    const pc = piecesIn(vs, t, t + 0.001, gone)[0];
    if (!pc) return "";
    const r = (vs?.ranges ?? []).find((x) => t >= x.start && t < x.end);
    const a = r?.start ?? pc.a;
    const b = r?.end ?? pc.b;
    switch (pc.state) {
      case "have":
        return vs?.local ? "녹화 파일 영상" : `영상 받음 ${fmtTime(a, 0)}~${fmtTime(b, 0)} (${fmtDurationKo(b - a)})`;
      case "fetching": {
        const reqs = requestsAt(vs, t);
        const tot = reqs.reduce((n, q) => n + q.total_sec, 0);
        const hv = reqs.reduce((n, q) => n + q.have_sec, 0);
        return `받는 중${tot ? ` ${Math.floor((hv / tot) * 100)}%` : ""}${vs?.eta_sec ? ` · 약 ${fmtDurationKo(vs.eta_sec)}` : ""}`;
      }
      case "queued":
        if (vs?.offline) return `인터넷 연결을 기다리는 중 · ${Math.max(1, Math.round(vs.offline.retry_in_sec))}초 뒤 다시`;
        return vs?.paused ? "대기 중 · 영상 받기가 일시정지됐어요" : "대기 중 · 앞 작업이 끝나면 받아요";
      case "failed":
        return `받지 못했어요${r?.code === "NETWORK" ? " · 연결이 끊겼어요" : ""} · 작업 현황에서 다시 시도할 수 있어요`;
      case "gone":
        return "다시보기를 더 이상 열 수 없어요";
      default:
        return "영상을 받지 않았어요 · 소리는 들을 수 있어요\nShift+드래그로 범위를 골라 받을 수 있어요";
    }
  };

  const hitCandidate = (x: number) => (p.detection?.candidates ?? []).findIndex((c) => x >= xOf(c.start) - 2 && x <= xOf(c.end) + 2);

  const onPointerDown = (e: React.PointerEvent) => {
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    const x = e.clientX - rect.left;
    const y = e.clientY - rect.top;
    const t = tOf(x);
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    // Shift+drag outside the song bars: pick a range to fetch video for (14.5)
    if (e.shiftKey && !(y >= yBars && y < yBand && hitSong(x))) {
      setDrag({ kind: "vrange", a: t, b: t, x0: x });
      return;
    }
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
      if (y >= yBand && y < yBand + BAND) {
        el.style.cursor = "default";
        setHover({ x, y, text: bandTip(t) });
        return;
      }
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
    } else if (drag.kind === "vrange") {
      const b = Math.max(0, Math.min(total, t));
      setDrag({ ...drag, b });
      setHover({ x, y, text: `영상 받을 범위 ${fmtTime(Math.min(drag.a, b), 0)} ~ ${fmtTime(Math.max(drag.a, b), 0)} (${fmtDurationKo(Math.abs(b - drag.a))})` });
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
    if (d.kind === "vrange") {
      const a = Math.min(d.a, d.b);
      const b = Math.max(d.a, d.b);
      useWs.setState({ videoRange: Math.abs(x - d.x0) > 4 && b - a > 1 ? [a, b] : null });
    } else if (d.kind === "edge") {
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
    { y: 0, h: LANE_BARS, text: "곡" },
    { y: 0, h: BAND, text: "영상" },
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
          <span className="lg band" data-tip={"영상 줄: 실선 받음 · 줄무늬 받는 중 · 테두리 받을 예정\n빨간 점선 받지 못함 · 회색 빗금 받을 수 없음"} data-tip-pos="top">
            받은 영상
          </span>
        </div>
        <span className="map-cov faint" data-tip="받은 영상 / 방송 길이 · 디스크 사용량" data-tip-pos="top">
          {vs && !vs.local && vs.have_sec > 0 ? `영상 ${fmtDurationKo(vs.have_sec)} / ${fmtTime(total, 0, true)} · ${fmtBytes(vs.bytes)}` : coverageSummary(vs)}
        </span>
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
      <MapNotices p={p} />
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
          <canvas ref={ref} role="img" aria-label={`방송 전체 지도: 곡 ${p.songs.filter((s) => s.enabled).length}개, 후보 구간 ${p.detection?.candidates.length ?? 0}개. 곡 목록에서 같은 곡을 고를 수 있어요.`} />
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
});

/**
 * One-line prompts above the map (not dialogs, 14.4–14.6): the Shift+drag range, a boundary
 * top-up over the automatic limit, and filling a larger margin for songs that have video.
 */
function MapNotices({ p }: { p: Project }) {
  const vs = useVideo((s) => s.byProject[p.id]);
  const { videoRange, topupAsk, marginAsk } = useWs(useShallow((s) => ({ videoRange: s.videoRange, topupAsk: s.topupAsk, marginAsk: s.marginAsk })));
  const nums = useMemo(() => songNumbers(p), [p]);
  const [est, setEst] = useState<{ key: string; sec: number; bytes: number } | null>(null);
  const key = videoRange ? `${videoRange[0].toFixed(1)}-${videoRange[1].toFixed(1)}` : "";
  useEffect(() => {
    if (!videoRange) return;
    let alive = true;
    void estimate(p.id, { ranges: [videoRange] }).then((r) => alive && r && setEst({ key, sec: r.missing_sec, bytes: r.estimated_bytes }));
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, p.id]);
  const gone = vodGone(p);
  if (!videoRange && !topupAsk.length && !marginAsk) return null;
  const fetchRange = async () => {
    if (!videoRange) return;
    const missing = est?.key === key ? est.sec : len(partsIn(vs, videoRange[0], videoRange[1]));
    if (missing > 30 * 60) {
      const r = await ask({
        title: `영상 ${fmtDurationKo(missing)}을 받을까요?`,
        body: [`약 ${est?.key === key ? fmtBytes(est.bytes) : "?"}를 써요. 받는 동안에도 편집은 그대로 할 수 있어요.`],
        icon: "info",
        buttons: [
          { id: "cancel", label: "취소", kind: "ghost" },
          { id: "ok", label: "받기", kind: "primary" },
        ],
      });
      if (r !== "ok") return;
    }
    useWs.setState({ videoRange: null });
    await fetchVideo(p.id, { ranges: [videoRange], priority: "user", reason: "range" });
  };
  return (
    <div className="map-notices">
      {videoRange && (
        <div className="map-notice">
          <Download size={13} />
          <span className="grow">
            고른 범위 <span className="mono">{fmtTime(videoRange[0], 0)} ~ {fmtTime(videoRange[1], 0)}</span>
            {est?.key === key ? (est.sec > 0 ? ` · 영상 ${fmtDurationKo(est.sec)} 더 필요 · 약 ${fmtBytes(est.bytes)}` : " · 영상이 이미 다 있어요") : " · 계산 중…"}
          </span>
          <button className="btn xs primary" disabled={gone || (est?.key === key && est.sec <= 0)} onClick={() => void fetchRange()} data-tip={gone ? "다시보기를 더 이상 열 수 없어요" : undefined}>
            이 범위 영상 받기
          </button>
          <button className="btn xs ghost" onClick={() => useWs.setState({ videoRange: null })}>
            취소 <kbd>Esc</kbd>
          </button>
        </div>
      )}
      {topupAsk.map((ask) => (
        <div key={ask.songId} className="map-notice">
          <Download size={13} />
          <span className="grow">
            곡 {String(nums.get(ask.songId) ?? "").padStart(2, "0")}: 영상이 {fmtDurationKo(ask.sec)}(약 {fmtBytes(ask.bytes)}) 더 필요해요. 자동으로 받는 양(5분)을 넘어서 물어봐요.
          </span>
          <button
            className="btn xs primary"
            onClick={() => {
              useWs.setState((st) => ({ topupAsk: st.topupAsk.filter((x) => x.songId !== ask.songId) }));
              const s = p.songs.find((x) => x.id === ask.songId);
              void fetchVideo(p.id, { ranges: ask.ranges, priority: "user", reason: "topup", process: !!s && (p.stems ?? []).some((sp) => sp.end > s.start && sp.start < s.end) });
            }}
          >
            받기
          </button>
          <button className="btn xs ghost" onClick={() => useWs.setState((st) => ({ topupAsk: st.topupAsk.filter((x) => x.songId !== ask.songId) }))}>
            나중에
          </button>
        </div>
      ))}
      {marginAsk && (
        <div className="map-notice">
          <Download size={13} />
          <span className="grow">앞뒤 여유를 바꿨어요. 이미 영상을 받은 곡에는 그대로예요.</span>
          <button
            className="btn xs subtle"
            onClick={() => {
              useWs.setState({ marginAsk: null });
              void fetchSongsVideo(p, marginAsk.songIds, { priority: "user", reason: "margin" });
            }}
          >
            받은 곡에도 적용 · {marginAsk.songIds.length}곡 · 약 {fmtDurationKo(marginAsk.sec)} 분량
          </button>
          <button className="btn xs ghost" onClick={() => useWs.setState({ marginAsk: null })}>
            괜찮아요
          </button>
        </div>
      )}
    </div>
  );
}
