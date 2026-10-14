// 경계 확대: ±8초 around the song start and end. Click = listen from there, drag the line = move
// the boundary (0.1 s steps, Alt = free). Uses the clip's 100/s peaks when the video is fetched,
// otherwise the whole-VOD 20/s peaks.
import { useEffect, useRef, useState } from "react";
import type { Project } from "@ipc/generated/Project";
import type { Song } from "@ipc/generated/Song";
import { begin, colors, drawCurve, drawWave, useCanvasSize, useThemeVersion } from "@/canvas/draw";
import { loadPeaks, mediaUrl, type Peaks, type Probs } from "@/lib/peaks";
import { spanFor } from "@/lib/video";
import { fmtTime } from "@/lib/format";
import { player } from "@/audio/player";
import { setEdge } from "@/lib/edits";
import { useWs } from "@/state/ws";

const SPAN = 8;

function Scope({ p, s, edge, peaks, origin, probs }: { p: Project; s: Song; edge: "start" | "end"; peaks: Peaks | null; origin: number; probs: Probs | null }) {
  const { ref, w, h, dpr } = useCanvasSize();
  const theme = useThemeVersion();
  const head = useRef<HTMLDivElement>(null);
  const [drag, setDrag] = useState<number | null>(null);
  const lastEdge = useWs((x) => x.lastEdge);
  const at = edge === "start" ? s.start : s.end;
  const t0 = at - SPAN;
  const t1 = at + SPAN;
  const xOf = (t: number) => ((t - t0) / (t1 - t0)) * w;
  const tOf = (x: number) => t0 + (x / w) * (t1 - t0);

  useEffect(() => {
    const c = ref.current;
    if (!c || w < 2) return;
    const ctx = begin(c, dpr);
    const col = colors();
    ctx.fillStyle = col["bg-0"];
    ctx.fillRect(0, 0, w, h);
    const inside = edge === "start" ? [at, t1] : [t0, at];
    ctx.fillStyle = col["accent-soft"];
    ctx.fillRect(xOf(inside[0]), 0, xOf(inside[1]) - xOf(inside[0]), h);
    if (probs) drawCurve(ctx, probs.margin, probs.fps, t0, t1, w, 4, h * 0.35, (v) => (v + 0.25) / 1.1, col.prob, col["prob-fill"]);
    if (peaks) drawWave(ctx, peaks, origin, t0, t1, w, h * 0.66, h * 0.3, col["wave-strong"], 1.2);
    // 1 s ticks
    ctx.fillStyle = col["text-3"];
    ctx.font = `10px ${col.font}`;
    ctx.textBaseline = "bottom";
    for (let k = -SPAN; k <= SPAN; k += 2) {
      const x = xOf(at + k);
      ctx.fillRect(x, h - 4, 1, 4);
      if (k !== 0) ctx.fillText(`${k > 0 ? "+" : ""}${k}s`, x + 2, h - 1);
    }
    const x = xOf(drag ?? at);
    ctx.fillStyle = col.accent;
    ctx.fillRect(x - 1, 0, 2, h);
    ctx.beginPath();
    ctx.moveTo(x - 6, 0);
    ctx.lineTo(x + 6, 0);
    ctx.lineTo(x, 7);
    ctx.closePath();
    ctx.fill();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [w, h, dpr, peaks, probs, at, drag, theme, edge]);

  useEffect(
    () =>
      player.onFrame((pos) => {
        const el = head.current;
        if (!el) return;
        const x = ((pos - t0) / (t1 - t0)) * w;
        el.style.transform = `translateX(${x}px)`;
        el.style.display = x < 0 || x > w ? "none" : "block";
      }),
    [t0, t1, w],
  );

  const local = (e: React.PointerEvent) => e.clientX - (e.currentTarget as HTMLElement).getBoundingClientRect().left;
  return (
    <div className={`scope${lastEdge === edge ? " active" : ""}`}>
      <div className="scope-head">
        <span>{edge === "start" ? "곡 시작 부근" : "곡 끝 부근"}</span>
        <span className="mono faint">{fmtTime(drag ?? at, 1, true)}</span>
      </div>
      <div
        className="scope-canvas"
        onPointerDown={(e) => {
          const x = local(e);
          (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
          if (Math.abs(x - xOf(at)) <= 7) setDrag(at);
          else {
            useWs.setState({ lastEdge: edge });
            void player.play(tOf(x), { stopAt: tOf(x) + 4 });
          }
        }}
        onPointerMove={(e) => {
          const x = local(e);
          (e.currentTarget as HTMLElement).style.cursor = drag !== null || Math.abs(x - xOf(at)) <= 7 ? "ew-resize" : "pointer";
          if (drag !== null) {
            const t = tOf(x);
            setDrag(e.altKey ? t : Math.round(t * 10) / 10);
          }
        }}
        onPointerUp={() => {
          if (drag !== null && Math.abs(drag - at) > 0.01) void setEdge(p.id, s.id, edge, drag);
          setDrag(null);
        }}
        title="클릭: 거기서 4초 듣기 · 초록 선 드래그: 경계 옮기기 (Alt: 0.1초 스냅 끄기)"
      >
        <canvas ref={ref} />
        <div className="playhead" ref={head} style={{ bottom: 0 }} />
      </div>
    </div>
  );
}

export function EdgeScopes({ p, s, probs }: { p: Project; s: Song; probs: Probs | null }) {
  // the stem span's 100/s peaks when it covers both edges (± 8 s), else the whole-VOD 20/s peaks
  const sp = spanFor(p, s.start, s.end);
  const span = sp && sp.start <= s.start - 8 && sp.end >= s.end + 8 ? sp : null;
  const [peaks, setPeaks] = useState<{ data: Peaks; origin: number } | null>(null);
  const key = span ? span.id : p.detection ? "overview" : "";
  useEffect(() => {
    let alive = true;
    if (span) {
      void loadPeaks(mediaUrl(p.id, `${span.dir}/orig.peaks`, span.id)).then((d) => alive && setPeaks({ data: d, origin: span.start })).catch(() => {});
    } else if (p.detection) {
      void loadPeaks(mediaUrl(p.id, "overview.peaks", `${p.audio?.fetched_bytes ?? 0}|${p.audio?.elapsed_sec ?? 0}`)).then((d) => alive && setPeaks({ data: d, origin: 0 })).catch(() => {});
    }
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, p.id]);
  return (
    <div className="scopes">
      <Scope p={p} s={s} edge="start" peaks={peaks?.data ?? null} origin={peaks?.origin ?? 0} probs={probs} />
      <Scope p={p} s={s} edge="end" peaks={peaks?.data ?? null} origin={peaks?.origin ?? 0} probs={probs} />
    </div>
  );
}
