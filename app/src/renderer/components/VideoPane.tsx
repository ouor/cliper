// The picture at the playhead (기획서 14.5): plays the downloaded video as one broadcast
// (`video.view` files: currentTime = vod − origin), and covers the box where there is no
// video, saying why and what to do. Never shows a frame from another time (이슈 #1).
import { useEffect, useRef, useState, type ReactNode } from "react";
import { AlertTriangle, Ban, Download, Loader2, Pause, RefreshCw, VideoOff, WifiOff, Zap } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import type { VideoView } from "@ipc/generated/VideoView";
import { player, usePlayer, VIDEO_START_LEAD } from "@/audio/player";
import { call } from "@/lib/rpc";
import { mediaUrl } from "@/lib/peaks";
import { covAt, prioritizeAt, requestsAt, vodGone, fetchHere, type Cov } from "@/lib/video";
import { setPaused, useVideo } from "@/state/video";

type Shown = { kind: "video" } | { kind: "opening" } | { kind: "cover"; state: Cov; code?: string; t: number; range: [number, number] | null };

const FAIL_REASON: Record<string, string> = {
  NETWORK: "연결이 끊겼어요",
  CDN_TRUNCATED: "플랫폼 서버의 영상이 깨져 있어요",
  AUTH_REQUIRED: "로그인이 필요해요",
  VOD_GONE: "다시보기를 열 수 없어요",
  FORMAT: "영상 형식을 읽지 못했어요",
  IO: "저장하지 못했어요",
};

export function VideoPane({ p, children }: { p: Project; children?: ReactNode }) {
  const vs = useVideo((s) => s.byProject[p.id]);
  const paused = useVideo((s) => s.paused);
  const gone = vodGone(p);
  const els = useRef(new Map<string, HTMLVideoElement>());
  // one stable ref callback per file: an inline callback would be called with null on every
  // render, and null is where the element's decoder is released (U-9)
  const refFns = useRef(new Map<string, (el: HTMLVideoElement | null) => void>());
  const refFor = (file: string) => {
    let fn = refFns.current.get(file);
    if (!fn) {
      fn = (el) => {
        if (el) {
          els.current.set(file, el);
          return;
        }
        const old = els.current.get(file);
        els.current.delete(file);
        refFns.current.delete(file);
        if (old) {
          old.pause();
          old.removeAttribute("src");
          old.load();
        }
      };
      refFns.current.set(file, fn);
    }
    return fn;
  };
  const [view, setView] = useState<VideoView | null>(null);
  /** the view of the stretch right after the current one, loaded in a hidden element so the
   *  switch at a file boundary (adopted clip ↔ new segments, C2-1) is seamless */
  const [next, setNext] = useState<VideoView | null>(null);
  const [shown, setShown] = useState<Shown>({ kind: "opening" });
  const state = useRef({
    vs,
    gone,
    view,
    next,
    /** the position the in-flight video.view was asked for (null = none) */
    asking: null as number | null,
    askingNext: null as number | null,
    /** a video.view that failed: where, the coverage range there, and the backoff (U-8) */
    failed: null as { t: number; key: string; at: number; n: number } | null,
  });
  state.current.vs = vs;
  state.current.gone = gone;
  state.current.view = view;
  state.current.next = next;

  // a new project: forget the old views
  useEffect(() => {
    setView(null);
    setNext(null);
    state.current.failed = null;
  }, [p.id]);

  // follow the playhead: what to show, and which view files to ask for
  useEffect(() => {
    let last = "";
    const show = (s: Shown) => {
      const k = s.kind === "cover" ? `c|${s.state}|${s.code ?? ""}|${s.range?.[0] ?? ""}` : s.kind;
      if (k === last) return;
      last = k;
      setShown(s);
    };
    const inside = (v: VideoView | null, pos: number) => !!v && pos >= v.start - 0.05 && pos <= v.end + 0.05;
    const rangeKey = (t: number) => {
      const r = covAt(state.current.vs, t).range;
      return r ? `${r.start.toFixed(2)}-${r.end.toFixed(2)}-${r.state}` : "-";
    };
    const ask = (t: number) => {
      const st = state.current;
      if (st.asking !== null) return;
      st.asking = t;
      call("video.view", { id: p.id, t })
        .then((v) => {
          st.failed = null;
          // the playhead moved elsewhere while this was built: don't show it, ask again (U-8)
          const pos = player.now();
          if (inside(v, pos)) setView(v);
          else if (v.start <= pos + 6 && v.end >= pos - 6) setNext(v);
        })
        .catch(() => {
          // VIDEO_MISSING (coverage just changed) or a remux problem: back off; ask again sooner
          // only when the coverage around that point changes (U-8)
          const f = st.failed;
          st.failed = { t, key: rangeKey(t), at: performance.now(), n: f && Math.abs(f.t - t) < 2 ? f.n + 1 : 1 };
        })
        .finally(() => {
          st.asking = null;
        });
    };
    const mayAsk = (pos: number) => {
      const f = state.current.failed;
      if (!f || Math.abs(f.t - pos) > 2) return true;
      if (rangeKey(f.t) !== f.key) return true;
      return performance.now() - f.at > Math.min(30000, 1000 * 2 ** f.n);
    };
    const askNext = (t: number) => {
      const st = state.current;
      if (st.askingNext !== null) return;
      st.askingNext = t;
      call("video.view", { id: p.id, t })
        .then((v) => {
          if (state.current.view?.file !== v.file) setNext(v);
        })
        .catch(() => {})
        .finally(() => {
          st.askingNext = null;
        });
    };
    const tick = (pos: number) => {
      const st = state.current;
      const ps = usePlayer.getState();
      const n = st.next;
      const nextEl = n ? els.current.get(n.file) : undefined;
      const at = covAt(st.vs, pos, st.gone);
      if (at.state !== "have") {
        if (nextEl && !nextEl.paused) nextEl.pause();
        show({ kind: "cover", state: at.state, code: at.range?.code, t: pos, range: at.range ? [at.range.start, at.range.end] : null });
        return;
      }
      const v = st.view;
      if (inside(v, pos)) {
        show({ kind: "video" });
        // just before a forward boundary: start the preloaded next file so it is already running
        // when we switch (QA-9); otherwise keep it parked (U-9)
        const startSoon = !!n && ps.playing && ps.rate > 0 && ps.rate < 1.5 && n.start > pos && n.start - pos <= VIDEO_START_LEAD * ps.rate;
        if (nextEl) {
          if (startSoon && nextEl.paused && nextEl.readyState >= 2) {
            nextEl.playbackRate = ps.rate;
            void nextEl.play().catch(() => {});
          } else if (!startSoon && !nextEl.paused) nextEl.pause();
        }
        // near the end of this file (forward) or its start (reverse) while video continues
        // past it: load that neighbour now (U-6)
        if (v) {
          const fwd = ps.rate >= 0;
          const edge = fwd ? v.end + 0.2 : v.start - 0.2;
          const near = fwd ? v.end - pos < 6 : pos - v.start < 6;
          const have = !!n && n.start <= edge && n.end >= edge;
          if (near && !have && edge > 0 && covAt(st.vs, edge, st.gone).state === "have") askNext(edge);
        }
        return;
      }
      // crossed into the preloaded neighbour (either direction): switch without the cover
      if (inside(n, pos)) {
        st.view = n;
        st.next = null;
        setView(n);
        setNext(null);
        show({ kind: "video" });
        return;
      }
      show({ kind: "opening" });
      if (mayAsk(pos)) ask(pos);
    };
    tick(player.now());
    return player.onFrame(tick);
  }, [p.id]);

  // coverage changed: a failure at a point whose coverage changed may be asked again (mayAsk)

  // bind the active <video> element to the view
  useEffect(() => {
    if (!view) {
      player.attachVideo(null);
      return;
    }
    player.attachVideo(els.current.get(view.file) ?? null, view.origin, view.start, view.end);
    return () => player.attachVideo(null);
  }, [view]);

  const t = shown.kind === "cover" ? shown.t : 0;
  const reqs = shown.kind === "cover" ? requestsAt(vs, t) : [];
  const total = reqs.reduce((n, r) => n + r.total_sec, 0);
  const have = reqs.reduce((n, r) => n + r.have_sec, 0);
  const pct = total > 0 ? Math.floor((have / total) * 100) : null;
  // requests that run even while downloads are paused (an export waits for them, protocol 4)
  const forced = reqs.some((r) => r.force);

  let cover: ReactNode = null;
  if (shown.kind === "opening") {
    cover = (
      <div className="video-outside quiet">
        <Loader2 size={18} className="spin" />
        <span>영상을 여는 중…</span>
      </div>
    );
  } else if (shown.kind === "cover") {
    const st = shown.state;
    if (st === "none") {
      cover = (
        <div className="video-outside" role="status">
          <VideoOff size={20} />
          <span className="vo-title">이 위치의 영상은 받지 않았어요</span>
          <span className="vo-sub">소리는 그대로 들려요</span>
          <button className="btn sm subtle" onClick={() => void fetchHere(p.id, player.now(), p.audio?.duration ?? p.source?.duration ?? 0)} data-tip="재생 위치 앞뒤 1분(합쳐서 2분)을 받아요. 곡은 만들지 않아요." data-tip-pos="top">
            <Download size={13} /> 여기 영상 받기 · 앞뒤 1분 <kbd>V</kbd>
          </button>
        </div>
      );
    } else if (st === "fetching" || st === "queued") {
      cover = (
        <div className="video-outside" role="status">
          {paused && !forced ? <Pause size={20} /> : vs?.offline ? <WifiOff size={20} /> : <Loader2 size={20} className="spin" />}
          <span className="vo-title">
            {vs?.offline
              ? "인터넷 연결을 기다리는 중이에요"
              : paused && !forced
                ? "영상 받기가 일시정지됐어요"
                : st === "queued"
                  ? "이 위치의 영상을 곧 받아요"
                  : `이 위치의 영상을 받는 중이에요${pct !== null ? ` · ${pct}%` : ""}`}
          </span>
          <span className="vo-sub">
            {vs?.offline ? `${Math.max(1, Math.round(vs.offline.retry_in_sec))}초 뒤 다시 해 봐요. 연결되면 알아서 이어 받아요` : paused && !forced ? "다시 시작하면 이어서 받아요" : "받는 대로 여기서 바로 보여요"}
          </span>
          {paused && !forced && !vs?.offline ? (
            <button className="btn sm subtle" onClick={() => void setPaused(false)}>
              다시 시작
            </button>
          ) : (
            reqs.some((r) => r.priority !== "now") && (
              <button className="btn sm subtle" onClick={() => void prioritizeAt(p.id, vs, t)} data-tip="다른 받기보다 먼저 받아요" data-tip-pos="top">
                <Zap size={13} /> 먼저 받기
              </button>
            )
          )}
        </div>
      );
    } else if (st === "failed") {
      cover = (
        <div className="video-outside" role="status">
          <AlertTriangle size={20} />
          <span className="vo-title">이 위치의 영상을 받지 못했어요</span>
          <span className="vo-sub">{FAIL_REASON[shown.code ?? ""] ?? "잠시 뒤 다시 시도해 주세요"}</span>
          {!gone && (
            <button className="btn sm subtle" onClick={() => void call("video.retry", { id: p.id, ranges: shown.range ? [shown.range] : null }).catch(() => {})}>
              <RefreshCw size={13} /> 다시 시도
            </button>
          )}
        </div>
      );
    } else if (st === "gone") {
      cover = (
        <div className="video-outside" role="status">
          <Ban size={20} />
          <span className="vo-title">이 위치의 영상은 받을 수 없어요</span>
          <span className="vo-sub">다시보기를 더 이상 열 수 없어요. 소리는 그대로 들려요</span>
        </div>
      );
    }
  }

  return (
    <div className="video-box">
      {[view, next].map(
        (v) =>
          v && (
            <video
              key={v.file}
              ref={refFor(v.file)}
              className={v === view ? undefined : "video-preload"}
              src={mediaUrl(p.id, v.file)}
              muted
              playsInline
              preload="auto"
              // the preloaded neighbour waits on its first frame
              onLoadedMetadata={(e) => {
                // the preloaded neighbour waits on the frame it will be entered at (U-6)
                if (v !== view) e.currentTarget.currentTime = view && v.end <= view.start + 0.1 ? Math.max(0, v.end - v.origin - 0.01) : Math.max(0, v.start - v.origin + 0.01);
              }}
            />
          ),
      )}
      {cover}
      {children}
    </div>
  );
}
