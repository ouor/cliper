// The picture at the playhead (기획서 14.5): plays the downloaded video as one broadcast
// (`video.view` files: currentTime = vod − origin), and covers the box where there is no
// video, saying why and what to do. Never shows a frame from another time (이슈 #1).
import { useEffect, useRef, useState, type ReactNode } from "react";
import { AlertTriangle, Ban, Download, Loader2, Pause, RefreshCw, VideoOff, Zap } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import type { VideoView } from "@ipc/generated/VideoView";
import { player } from "@/audio/player";
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
  const ref = useRef<HTMLVideoElement>(null);
  const [view, setView] = useState<VideoView | null>(null);
  const [shown, setShown] = useState<Shown>({ kind: "opening" });
  const state = useRef({ vs, gone, view, asking: null as number | null, failedAt: null as number | null });
  state.current.vs = vs;
  state.current.gone = gone;
  state.current.view = view;

  // a new project: forget the old view
  useEffect(() => {
    setView(null);
    state.current.failedAt = null;
  }, [p.id]);

  // follow the playhead: what to show, and which view file to ask for
  useEffect(() => {
    let last = "";
    const show = (s: Shown) => {
      const k = s.kind === "cover" ? `c|${s.state}|${s.code ?? ""}|${s.range?.[0] ?? ""}` : s.kind;
      if (k === last) return;
      last = k;
      setShown(s);
    };
    const ask = (t: number) => {
      const st = state.current;
      if (st.asking !== null) return;
      st.asking = t;
      call("video.view", { id: p.id, t })
        .then((v) => {
          st.failedAt = null;
          setView(v);
        })
        .catch(() => {
          // VIDEO_MISSING (coverage just changed) or a remux problem: try again on the next change
          st.failedAt = t;
        })
        .finally(() => {
          st.asking = null;
        });
    };
    const tick = (pos: number) => {
      const st = state.current;
      const at = covAt(st.vs, pos, st.gone);
      if (at.state !== "have") {
        show({ kind: "cover", state: at.state, code: at.range?.code, t: pos, range: at.range ? [at.range.start, at.range.end] : null });
        return;
      }
      const v = st.view;
      if (v && pos >= v.start - 0.05 && pos <= v.end + 0.05) {
        show({ kind: "video" });
        return;
      }
      show({ kind: "opening" });
      if (st.failedAt === null || Math.abs(st.failedAt - pos) > 2) ask(pos);
    };
    tick(player.now());
    return player.onFrame(tick);
  }, [p.id]);

  // coverage changed (e.g. the stretch grew, a failure cleared): allow asking again
  useEffect(() => {
    state.current.failedAt = null;
  }, [vs]);

  // bind the <video> element to the view
  useEffect(() => {
    if (!view) {
      player.attachVideo(null);
      return;
    }
    player.attachVideo(ref.current, view.origin, view.start, view.end);
    return () => player.attachVideo(null);
  }, [view]);

  const t = shown.kind === "cover" ? shown.t : 0;
  const reqs = shown.kind === "cover" ? requestsAt(vs, t) : [];
  const total = reqs.reduce((n, r) => n + r.total_sec, 0);
  const have = reqs.reduce((n, r) => n + r.have_sec, 0);
  const pct = total > 0 ? Math.floor((have / total) * 100) : null;

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
          {paused ? <Pause size={20} /> : <Loader2 size={20} className="spin" />}
          <span className="vo-title">{paused ? "영상 받기가 일시정지됐어요" : st === "queued" ? "이 위치의 영상을 곧 받아요" : `이 위치의 영상을 받는 중이에요${pct !== null ? ` · ${pct}%` : ""}`}</span>
          <span className="vo-sub">{paused ? "다시 시작하면 이어서 받아요" : "받는 대로 여기서 바로 보여요"}</span>
          {paused ? (
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
      {view && <video key={view.file} ref={ref} src={mediaUrl(p.id, view.file)} muted playsInline preload="auto" />}
      {cover}
      {children}
    </div>
  );
}
