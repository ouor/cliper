// 프로젝트 작업공간: ① 곡 검수 ② 멘트 다듬기 ③ 내보내기.
import { useEffect, useMemo, useState, lazy, Suspense } from "react";
import { AlertTriangle, ArrowLeft, ArrowRight, Check, Lock, Moon, Redo2, RefreshCw, Settings2, Sparkles, Undo2, Zap } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import { ask, go, openSettings, setMode, toast, useApp, type WorkspaceMode } from "@/state/app";
import { loadProject, projectUi, putProject, redo, setProjectUi, undo, updateProjectSettings, useProjects } from "@/state/projects";
import { call } from "@/lib/rpc";
import { trackLabel } from "./Home";
import { runningJobs, startJob, useJobs, cancelJob, retryJob, type Job } from "@/state/jobs";
import { resetWs, requestFocus, useWs } from "@/state/ws";
import { useShallow } from "zustand/react/shallow";
import { player, usePlayer, type ListenMode } from "@/audio/player";
import { bindings, comboFromEvent, type KeyAction } from "@/lib/keys";
import { isTyping, Progress } from "@/components/common";
import { fmtEta, fmtShortDate, fmtTime } from "@/lib/format";
import { platformLabel, projectTitle, reviewStatus, songNumbers, unconfirmedMutes } from "@/lib/song";
import { fetchHere, songVideo, spanFor, vodGone } from "@/lib/video";
import { useVideo, videoOf } from "@/state/video";
import { FitVideoButton, ProjectMenu, ReplayBanner, usePrioritizeSelected, useVodCheck } from "./workspace/VideoControls";
import { fmtDurationKo } from "@/lib/format";
import { addSong, confirmSong, deleteMute, deleteSong, endSongHere, mergeSongs, nextUnreviewed, setEdge, splitMute, splitSong, toggleExclude, updateMute, addMute } from "@/lib/edits";
import { fetchSongs, processAll, startAnalysis } from "@/lib/pipeline";
import { friendly } from "@/lib/errors";
import { SongList, sortSongs } from "./workspace/SongList";
import { SongDetail, listenEnd, listenStart } from "./workspace/SongDetail";
import { OverviewMap } from "./workspace/OverviewMap";
import { Transport } from "./workspace/Transport";
const TalkEditor = lazy(() => import("./workspace/TalkEditor").then((m) => ({ default: m.TalkEditor })));
import { muteLoopRange, selectMute } from "./workspace/DetailTimeline";
const ExportView = lazy(() => import("./workspace/ExportView").then((m) => ({ default: m.ExportView })));
import { SetlistDialog } from "./workspace/SetlistDialog";

function TrackPicker({ p }: { p: Project }) {
  const tracks = p.source?.audio_tracks ?? [];
  if (p.source?.platform !== "local" || tracks.length < 2) return null;
  const change = async (track: number) => {
    const r = await ask({
      title: "오디오 트랙을 바꿀까요?",
      body: ["바꾼 트랙에서 소리를 다시 꺼내고 곡을 다시 찾아요(몇 초~1분).", "직접 고치거나 확인한 곡은 그대로 남아요. 받아 둔 영상은 그 트랙으로 다시 받아야 해요."],
      icon: "question",
      buttons: [
        { id: "cancel", label: "취소", kind: "ghost" },
        { id: "ok", label: "바꾸기", kind: "primary" },
      ],
    });
    if (r !== "ok") return;
    await updateProjectSettings(p.id, { audio_track: track });
    await startAnalysis(p.id, true).catch((e) => toast(friendly(e).title, { kind: "error" }));
  };
  return (
    <label className="track-pick" data-tip="멀티트랙 녹화본에서 분석·분리·내보내기에 쓸 오디오 트랙" data-tip-pos="left">
      <span className="faint">트랙</span>
      <select className="select sm" value={p.settings.audio_track} onChange={(e) => void change(Number(e.target.value))}>
        {tracks.map((t) => (
          <option key={t.index} value={t.index}>
            {trackLabel(t)}
          </option>
        ))}
      </select>
    </label>
  );
}

function OriginalBadge() {
  const [open, setOpen] = useState(false);
  return (
    <div className="popover-anchor">
      <button className="badge-original" onClick={() => setOpen(!open)}>
        <Lock size={12} /> 원본 보존
      </button>
      {open && (
        <div className="popover right" onMouseLeave={() => setOpen(false)}>
          <b>원본은 바뀌지 않아요</b>
          <p className="muted">받은 소리와 영상은 그대로 보관돼요. 편집은 기록만 남기고, 내보낼 때 새 파일을 만들어요. 모든 편집은 Ctrl+Z로 되돌릴 수 있어요.</p>
        </div>
      )}
    </div>
  );
}

function lastFailed(jobs: ReturnType<typeof useJobs.getState>, pid: string, kinds: string[]): Job | undefined {
  for (const id of jobs.order) {
    const j = jobs.jobs[id];
    if (j.projectId === pid && kinds.includes(j.kind)) return j.status === "failed" ? j : undefined;
  }
  return undefined;
}

function NextBanner({ p }: { p: Project }) {
  const jobs = useJobs();
  const ws = useWs(useShallow((s) => ({ selected: s.selected })));
  const audioJob = runningJobs(jobs, p.id, "audio.fetch")[0];
  const detectJob = runningJobs(jobs, p.id, "detect.run")[0];
  const vs = useVideo((s) => s.byProject[p.id]);
  const failed = lastFailed(jobs, p.id, ["audio.fetch", "detect.run"]);
  const songs = p.songs.filter((s) => s.enabled);
  const doubts = songs.filter((s) => reviewStatus(s) === "doubt");
  const unreviewed = songs.filter((s) => !s.reviewed);
  const reviewed = songs.filter((s) => s.reviewed);
  // reviewed songs without any video yet (partly covered ones: [영상 맞추기] / top-ups)
  const needVideo = vodGone(p) ? [] : reviewed.filter((s) => songVideo(p, vs, s).state === "none");
  const fetchingSongs = songs.filter((s) => songVideo(p, vs, s).pending);
  const unconf = songs.reduce((n, s) => n + (spanFor(p, s.start, s.end) ? unconfirmedMutes(s) : 0), 0);

  if (failed && !p.detection && !audioJob && !detectJob) {
    return (
      <div className="next-banner error">
        <AlertTriangle size={15} />
        <span className="grow">
          {failed.error?.title} {failed.error?.hint}
        </span>
        <button className="btn sm subtle" onClick={() => void retryJob(failed)}>
          <RefreshCw size={13} /> 다시 시도
        </button>
      </div>
    );
  }
  if (audioJob) {
    return (
      <div className="next-banner progress">
        <div className="spinner" />
        <span className="nb-text">
          {p.source?.platform === "local" ? "녹화 파일에서 소리를 꺼내는 중이에요" : "소리를 받는 중이에요"} · {audioJob.fraction !== null ? `${Math.floor(audioJob.fraction * 100)}%` : "준비 중"} · {audioJob.eta !== null ? fmtEta(audioJob.eta) : "남은 시간 계산 중…"}
        </span>
        <div className="nb-progress">
          <Progress value={audioJob.fraction} />
        </div>
        <span className="faint">4시간 방송도 영상은 받지 않아요</span>
        <button className="btn sm ghost" onClick={() => void cancelJob(audioJob.id)} data-tip="취소하면 다음에 처음부터 다시 받아요" data-tip-pos="left">
          취소
        </button>
      </div>
    );
  }
  if (detectJob) {
    return (
      <div className="next-banner progress">
        <div className="spinner" />
        <span className="nb-text">노래를 찾는 중이에요{detectJob.fraction !== null ? ` · ${Math.floor(detectJob.fraction * 100)}%` : ""} · {detectJob.eta !== null ? fmtEta(detectJob.eta) : "약 15초"}</span>
        <div className="nb-progress">
          <Progress value={detectJob.fraction} />
        </div>
      </div>
    );
  }
  if (p.audio?.dropped && !audioJob) {
    const gone = p.vod?.status === "gone";
    return (
      <div className="next-banner warn">
        <AlertTriangle size={15} />
        <span className="grow">
          {gone ? "소리를 정리한 작업이에요. 다시보기가 없어 다시 받을 수 없어요. 곡 목록과 경계, 멘트 구간은 그대로예요." : "소리를 정리한 작업이에요. 듣고 고치려면 방송 소리를 다시 받아요 (4시간 방송 약 1분). 곡 목록과 경계, 멘트 구간은 그대로예요."}
        </span>
        {!gone && (
          <button className="btn sm primary" onClick={() => void startJob("audio.fetch", { id: p.id }, { label: "소리 다시 받기" }).catch((e) => toast(friendly(e).title, { kind: "error" }))}>
            소리 다시 받기
          </button>
        )}
      </div>
    );
  }
  if (!p.audio) {
    return (
      <div className="next-banner">
        <Sparkles size={15} />
        <span className="grow">아직 소리를 받지 않았어요. 소리만 받아서 노래를 찾아요 (4시간 방송 약 1분).</span>
        <button className="btn sm primary" onClick={() => void startAnalysis(p.id, true).catch((e) => toast(friendly(e).title, { kind: "error" }))}>
          소리 받고 곡 찾기
        </button>
      </div>
    );
  }
  if (!p.detection) {
    return (
      <div className="next-banner">
        <Sparkles size={15} />
        <span className="grow">소리를 받았어요. 이제 노래 구간을 찾아요 (약 15초).</span>
        <button className="btn sm primary" onClick={() => void startAnalysis(p.id, false).catch((e) => toast(friendly(e).title, { kind: "error" }))}>
          곡 찾기
        </button>
      </div>
    );
  }
  if (songs.length === 0) {
    return (
      <div className="next-banner warn">
        <AlertTriangle size={15} />
        <span className="grow">노래를 찾지 못했어요. 노래가 없는 방송이거나 반주 없이 부른 곡일 수 있어요. 지도의 곡 줄을 드래그하면 직접 추가할 수 있어요.</span>
        <button className="btn sm subtle" onClick={() => useWs.setState({ detectOpen: true })}>
          민감도 높여 다시 찾기
        </button>
      </div>
    );
  }
  if (unreviewed.length > 0) {
    return (
      <div className="next-banner">
        <Sparkles size={15} />
        <span className="grow">
          곡 {songs.length}개를 찾았어요. {doubts.length ? `애매한 곡 ${doubts.length}개부터 확인해 보세요.` : `확인할 곡이 ${unreviewed.length}개 남았어요.`}{" "}
          <span className="faint">
            <kbd>S</kbd> 시작 듣기 · <kbd>E</kbd> 끝 듣기 · <kbd>↵</kbd> 확인
          </span>
        </span>
        {doubts.length > 0 && ws.selected !== doubts[0].id && (
          <button className="btn sm subtle" onClick={() => useWs.setState({ selected: doubts[0].id, sort: "attention" })}>
            애매한 곡부터
          </button>
        )}
        {reviewed.length > 0 && needVideo.length > 0 && (
          <>
            <button className="btn sm subtle" onClick={() => void fetchSongs(p.id, needVideo.map((s) => s.id), `확인한 곡 영상 받기 ${needVideo.length}곡`)}>
              확인한 {needVideo.length}곡 먼저 받기
            </button>
            <button className="btn sm ghost" onClick={() => void askOvernight(p)} data-tip="확인한 곡만 처리해요" data-tip-pos="left">
              <Moon size={13} /> 밤새 처리
            </button>
          </>
        )}
      </div>
    );
  }
  if (needVideo.length === 0 && fetchingSongs.length > 0) {
    return (
      <div className="next-banner progress">
        <div className="spinner" />
        <span className="nb-text">
          {vs?.paused ? "영상 받기가 일시정지됐어요" : `영상 받는 중 · ${fetchingSongs.length}곡`}
          {vs && vs.queued_sec + vs.fetching_sec > 0 ? ` · 남은 분량 ${fmtDurationKo(vs.queued_sec + vs.fetching_sec)}` : ""}
          {!vs?.paused && vs?.eta_sec ? ` · ${fmtEta(vs.eta_sec)}` : ""}
        </span>
        <span className="faint">받는 동안에도 경계를 고치고 다른 곡을 볼 수 있어요</span>
        <button className="btn sm ghost" onClick={() => useApp.setState({ jobsOpen: true })}>
          작업 현황
        </button>
      </div>
    );
  }
  if (needVideo.length > 0) {
    return (
      <div className="next-banner ok">
        <Check size={15} />
        <span className="grow">곡 {reviewed.length}개 확인 완료.</span>
        <button className="btn sm primary" onClick={() => void fetchSongs(p.id, needVideo.map((s) => s.id), `확인한 곡 영상 받기 ${needVideo.length}곡`)}>
          <Zap size={13} /> 확인한 곡 영상 받기
        </button>
        <button className="btn sm subtle" onClick={() => void askOvernight(p)}>
          <Moon size={13} /> 밤새 전부 처리하기
        </button>
      </div>
    );
  }
  return (
    <div className="next-banner ok">
      <Check size={15} />
      <span className="grow">{unconf ? `영상을 모두 받았어요. 멘트 제안 ${unconf}개가 확인을 기다려요.` : "영상을 모두 받았어요. 멘트를 확인했으면 내보낼 수 있어요."}</span>
      <button className="btn sm primary" onClick={() => setMode(unconf ? "talk" : "export")}>
        {unconf ? "멘트 다듬기로" : "내보내기로"} <ArrowRight size={13} />
      </button>
    </div>
  );
}

async function askOvernight(p: Project) {
  const r = await ask({
    title: "밤새 전부 처리할까요?",
    body: ["확인한 곡 전부 영상 받기 → 목소리와 반주 나누기 → 멘트 제안까지 차례로 해요. 처리 중에는 PC가 절전 모드로 들어가지 않아요.", "멘트 제안은 사람이 확인해야 해서 내보내기는 기본으로 하지 않아요."],
    icon: "question",
    buttons: [
      { id: "cancel", label: "취소", kind: "ghost" },
      { id: "export", label: "멘트 지우기 없이 내보내기까지", kind: "subtle" },
      { id: "ok", label: "시작하기", kind: "primary" },
    ],
  });
  if (r === "cancel") return;
  await processAll(p.id, { exportToo: r === "export" });
}

/** shortcuts that may auto-repeat while the key is held */
const REPEATABLE = new Set(["seek.back", "seek.fwd", "seek.back2", "seek.fwd2", "seek.fineBack", "seek.fineFwd", "edge.back", "edge.fwd", "song.prev", "song.next", "talk.nextSong", "talk.prevSong", "mute.next", "mute.prev"]);

const STEPS: { mode: WorkspaceMode; n: string; label: string }[] = [
  { mode: "review", n: "1", label: "곡 검수" },
  { mode: "talk", n: "2", label: "멘트 다듬기" },
  { mode: "export", n: "3", label: "내보내기" },
];

export function Workspace({ projectId }: { projectId: string }) {
  const route = useApp((s) => s.route);
  const mode = route.name === "workspace" ? route.mode : "review";
  const p = useProjects((s) => s.projects[projectId]);
  const history = useProjects((s) => s.history[projectId]);
  const engineReady = useApp((s) => s.engine.state === "ready");
  const keymap = useApp((s) => s.settings.keymap);
  const ws = useWs(useShallow((s) => ({ selected: s.selected, selectedMute: s.selectedMute, lastEdge: s.lastEdge, setlistOpen: s.setlistOpen })));
  const [err, setErr] = useState<string | null>(null);

  // new project: reset the workspace UI; leaving: stop playback
  useEffect(() => {
    resetWs(projectId);
    return () => {
      player.pause();
      void player.clearSource();
    };
  }, [projectId]);

  // (re)load whenever the engine is ready (also after an engine restart) — keep the selection
  useEffect(() => {
    if (!engineReady) return;
    loadProject(projectId)
      .then((proj) => {
        // local recordings made before the engine listed audio tracks: reopen to fill them in
        if (proj.source?.platform === "local" && !(proj.source.audio_tracks ?? []).length) {
          void call("project.create", { input: proj.source.url }).then(putProject).catch(() => {});
        }
        const cur = useWs.getState().selected;
        if (cur && proj.songs.some((s) => s.id === cur)) return;
        const last = projectUi(projectId).lastSongId;
        const first = last && proj.songs.some((s) => s.id === last) ? last : sortSongs(proj, "attention", "")[0]?.id ?? null;
        useWs.setState({ selected: first });
      })
      .catch((e) => setErr(friendly(e).title));
  }, [projectId, engineReady]);

  // select the first song to check once songs appear (e.g. right after 곡 찾기)
  const songCount = p?.songs.length ?? 0;
  useEffect(() => {
    const st = useWs.getState();
    const proj = useProjects.getState().projects[projectId];
    if (!proj || !songCount) return;
    if (!st.selected || !proj.songs.some((s) => s.id === st.selected)) useWs.setState({ selected: sortSongs(proj, "attention", "")[0]?.id ?? null });
  }, [songCount, projectId]);

  // video: boundary top-ups, the selected song first, replay check in the background
  usePrioritizeSelected(projectId, ws.selected);
  useVodCheck(projectId, engineReady);

  // remember the selected song
  useEffect(() => {
    if (ws.selected) setProjectUi(projectId, { lastSongId: ws.selected });
  }, [ws.selected, projectId]);

  // audio source by mode
  const hasAudio = !!p?.audio && !p.audio.dropped;
  const total = p?.audio?.duration || p?.source?.duration || 0;
  useEffect(() => {
    if (!hasAudio) return;
    if (mode === "review") void player.useStream(projectId, total);
    if (mode !== "talk") player.setMode(0 as ListenMode);
    else player.setMode(1 as ListenMode);
    if (mode !== "talk") player.setMutes([], true);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, hasAudio, projectId]);

  // review loop range = 시작 듣기 범위 of the selected song
  useEffect(() => {
    if (mode !== "review" || !p) return;
    const s = p.songs.find((x) => x.id === ws.selected);
    if (!s) return player.setLoop(null);
    const pre = useApp.getState().settings.listenPreroll;
    if (ws.lastEdge === "end") player.setLoop([Math.max(0, s.end - pre), s.end + 3]);
    else player.setLoop([Math.max(0, s.start - pre), s.start + pre]);
  }, [mode, ws.selected, ws.lastEdge, p]);

  // talk loop range = the selected mute ± loopPad (follows edits); none selected → no range.
  // Only the range moves here — loop on/off is the user's toggle (#3).
  const talkSel = mode === "talk" ? p?.songs.find((x) => x.id === ws.selected)?.mutes.find((m) => m.id === ws.selectedMute) : undefined;
  const loopPad = useApp((a) => a.settings.loopPad);
  useEffect(() => {
    if (mode !== "talk") return;
    player.setLoop(talkSel ? muteLoopRange(talkSel) : null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mode, talkSel?.id, talkSel?.start, talkSel?.end, loopPad]);

  const keys = useMemo(() => bindings(keymap), [keymap]);

  // keyboard
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const app = useApp.getState();
      if (app.dialog || app.overlay) return;
      const st = useWs.getState();
      if (st.setlistOpen) return;
      if (isTyping(e)) return;
      const combo = comboFromEvent(e);
      if (!combo) return;
      if (combo === "Escape") {
        if (st.pendingRange || st.candidate !== null || st.multi.length || st.markStart !== null || st.videoRange || st.topupAsk.length) {
          useWs.setState({ pendingRange: null, candidate: null, multi: [], markStart: null, videoRange: null, topupAsk: [] });
          e.preventDefault();
        }
        return;
      }
      if (mode === "export" && combo === "Enter") {
        e.preventDefault();
        window.dispatchEvent(new Event("cliper:export-start"));
        return;
      }
      const acts = keys.get(combo);
      if (!acts) return;
      const scopes = mode === "review" ? ["review", "play", "global"] : mode === "talk" ? ["talk", "play", "global"] : ["play", "global"];
      const a = scopes.map((sc) => acts.find((x) => x.scope === sc)).find(Boolean) as KeyAction | undefined;
      if (!a) return;
      // jobs / settings / help are owned by the app-level handler
      if (a.id === "jobs" || a.id === "settings" || a.id === "help") return;
      // holding a key must not confirm / add / toggle repeatedly
      if (e.repeat && !REPEATABLE.has(a.id)) {
        e.preventDefault();
        return;
      }
      e.preventDefault();
      run(a.id);
    };
    const run = (id: string) => {
      const proj = useProjects.getState().projects[projectId];
      if (!proj) return;
      const st = useWs.getState();
      const s = proj.songs.find((x) => x.id === st.selected);
      const ordered = sortSongs(proj, mode === "talk" ? "time" : st.sort, st.search).filter((x) => x.enabled || st.showExcluded);
      const idx = ordered.findIndex((x) => x.id === st.selected);
      const t = player.now();
      const mutes = s ? [...s.mutes].sort((a, b) => a.start - b.start) : [];
      const m = s?.mutes.find((x) => x.id === st.selectedMute);
      switch (id) {
        case "mode.review":
          return setMode("review");
        case "mode.talk":
          return setMode("talk");
        case "mode.export":
          return setMode("export");
        case "jobs":
          return useApp.setState((x) => ({ jobsOpen: !x.jobsOpen }));
        case "undo": {
          const l = undo(projectId);
          if (l) toast(`되돌렸어요: ${l}`, { kind: "info", ttl: 2000 });
          return;
        }
        case "redo": {
          const l = redo(projectId);
          if (l) toast(`다시 했어요: ${l}`, { kind: "info", ttl: 2000 });
          return;
        }
        case "settings":
          return openSettings();
        case "help":
          return useApp.setState({ overlay: "help" });
        case "search":
          return requestFocus("search");
        case "play":
          return player.toggle();
        case "shuttle.back":
          return player.shuttle(-1);
        case "shuttle.stop":
          return player.pause();
        case "shuttle.fwd":
          return player.shuttle(1);
        case "seek.back":
          return player.nudge(-0.5);
        case "seek.fwd":
          return player.nudge(0.5);
        case "seek.back2":
          return player.nudge(-2);
        case "seek.fwd2":
          return player.nudge(2);
        case "seek.fineBack":
          return player.nudge(-0.1);
        case "seek.fineFwd":
          return player.nudge(0.1);
        case "loop":
          return player.toggleLoop();
        case "video.here":
          if (vodGone(proj)) return toast("다시보기를 더 이상 열 수 없어서 영상을 받을 수 없어요.", { kind: "warn" });
          return void fetchHere(projectId, t, total);
        // review
        case "song.prev":
          if (idx > 0) useWs.setState({ selected: ordered[idx - 1].id, multi: [], candidate: null, pendingRange: null });
          return;
        case "song.next":
          if (idx < ordered.length - 1) useWs.setState({ selected: ordered[idx + 1]?.id ?? ordered[0]?.id, multi: [], candidate: null, pendingRange: null });
          return;
        case "song.nextUnreviewed": {
          const n = nextUnreviewed(projectId, st.selected ?? undefined);
          if (n) useWs.setState({ selected: n, multi: [] });
          else toast("확인할 곡이 더 없어요.", { kind: "ok" });
          return;
        }
        case "listen.start":
          return s && listenStart(s);
        case "listen.end":
          return s && listenEnd(s);
        case "edge.in":
          return s && void setEdge(projectId, s.id, "start", t);
        case "edge.out":
          return s && void setEdge(projectId, s.id, "end", t);
        case "edge.back":
        case "edge.fwd": {
          if (!s) return;
          const d = id === "edge.back" ? -0.1 : 0.1;
          const edge = st.lastEdge;
          void setEdge(projectId, s.id, edge, s[edge] + d, { label: "경계 미세 조정" });
          return;
        }
        case "song.title":
          return requestFocus("title");
        case "song.note":
          return requestFocus("note");
        case "song.confirm":
          if (st.pendingRange) return void addSong(projectId, st.pendingRange[0], st.pendingRange[1]);
          return s && void confirmSong(projectId, s.id);
        case "song.exclude":
          return s && void toggleExclude(projectId, s.id);
        case "song.add": {
          if (st.pendingRange) return void addSong(projectId, st.pendingRange[0], st.pendingRange[1]);
          if (st.candidate !== null) {
            const cd = proj.detection?.candidates[st.candidate];
            if (cd) return void addSong(projectId, cd.start, cd.end);
          }
          return void addSong(projectId, t, Math.min(total, t + 180));
        }
        case "song.split":
          return s && void splitSong(projectId, s.id, t);
        case "song.merge": {
          if (st.multi.length > 1) return void mergeSongs(projectId, st.multi);
          const sorted = [...proj.songs].filter((x) => x.enabled).sort((a, b) => a.start - b.start);
          const i = sorted.findIndex((x) => x.id === s?.id);
          if (s && sorted[i + 1]) return void mergeSongs(projectId, [s.id, sorted[i + 1].id]);
          return;
        }
        case "song.delete":
          return s && void deleteSong(projectId, s.id);
        case "song.finish":
          if (!s) return;
          if (songVideo(proj, videoOf(projectId), s).state === "ready" && spanFor(proj, s.start, s.end)) {
            setMode("talk");
            return;
          }
          void fetchSongs(projectId, [s.id]);
          return;
        // talk
        case "listen.orig":
        case "listen.result":
        case "listen.removed":
        case "listen.acc":
          return player.setMode((["listen.orig", "listen.result", "listen.removed", "listen.acc"].indexOf(id) as ListenMode) ?? 1);
        case "mute.next":
        case "mute.prev": {
          if (!mutes.length) return;
          const i = mutes.findIndex((x) => x.id === st.selectedMute);
          const n = id === "mute.next" ? (i + 1) % mutes.length : (i - 1 + mutes.length) % mutes.length;
          selectMute(mutes[i < 0 ? 0 : n], true);
          return;
        }
        case "mute.confirm":
          if (s && m) {
            void updateMute(projectId, s.id, m.id, "적용 확정", (x) => void (x.confirmed = true));
            const next = mutes.find((x) => !x.confirmed && x.id !== m.id && x.start > m.start) ?? mutes.find((x) => !x.confirmed && x.id !== m.id);
            if (next) selectMute(next, usePlayer.getState().playing);
          }
          return;
        case "mute.audition":
          if (!m || m.confirmed) {
            toast("확인 전 제안을 고르면 적용해서 들어 볼 수 있어요.", { kind: "info", ttl: 2500 });
            return;
          }
          useWs.setState({ audition: !st.audition });
          return;
        case "mute.delete":
          return s && m && void deleteMute(projectId, s.id, m.id);
        case "mute.mark":
          if (!s) return;
          if (st.markStart === null) {
            useWs.setState({ markStart: t });
            toast("구간 시작을 찍었어요. 끝에서 B를 한 번 더 누르세요.", { kind: "info", ttl: 2500 });
          } else {
            const a = st.markStart;
            useWs.setState({ markStart: null });
            void addMute(projectId, s.id, a, t);
          }
          return;
        case "mute.startHere":
          return s && m && void updateMute(projectId, s.id, m.id, "구간 시작", (x) => void (x.start = Math.min(t, x.end - 0.05)));
        case "mute.endHere":
          return s && m && void updateMute(projectId, s.id, m.id, "구간 끝", (x) => void (x.end = Math.max(t, x.start + 0.05)));
        case "mute.split":
          return s && m && void splitMute(projectId, s.id, m.id, t);
        case "song.endHere":
          return s && m && void endSongHere(projectId, s.id, m.id);
        case "talk.nextSong":
        case "talk.prevSong": {
          const list = [...proj.songs].filter((x) => x.enabled).sort((a, b) => a.start - b.start);
          const i = list.findIndex((x) => x.id === st.selected);
          const n = list[id === "talk.nextSong" ? i + 1 : i - 1];
          if (n) useWs.setState({ selected: n.id, selectedMute: null });
          return;
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [keys, mode, projectId, total]);

  if (!p) {
    return (
      <div className="ws-loading">
        {err ? (
          <div className="banner error">
            <AlertTriangle size={15} /> {err}
            <button className="btn sm subtle" onClick={() => go({ name: "home" })}>
              홈으로
            </button>
          </div>
        ) : (
          <>
            <div className="spinner" /> 작업을 여는 중…
          </>
        )}
      </div>
    );
  }

  const songs = p.songs.filter((s) => s.enabled);
  const reviewed = songs.filter((s) => s.reviewed).length;
  const excluded = p.songs.length - songs.length;
  const nums = songNumbers(p);
  const sel = p.songs.find((s) => s.id === ws.selected);
  const talkDone = songs.length > 0 && songs.every((s) => !spanFor(p, s.start, s.end) || unconfirmedMutes(s) === 0) && songs.some((s) => spanFor(p, s.start, s.end));
  const exportDone = songs.length > 0 && songs.every((s) => s.export);
  const stepDone = { review: songs.length > 0 && reviewed === songs.length, talk: talkDone, export: exportDone };

  return (
    <div className={`workspace mode-${mode}`}>
      <header className="ws-top">
        <button className="btn ghost sm" onClick={() => go({ name: "home" })}>
          <ArrowLeft size={15} /> 홈
        </button>
        <div className="ws-title">
          <span className="ellipsis">{projectTitle(p)}</span>
          <span className="faint ws-meta">
            {[p.source?.channel, platformLabel(p.source?.platform), fmtShortDate(p.source?.catalog.published_at), total ? fmtTime(total, 0, true) : ""].filter(Boolean).join(" · ")}
          </span>
        </div>
        <span className="spacer" />
        <TrackPicker p={p} />
        <OriginalBadge />
        <ProjectMenu p={p} />
        <button className="btn ghost icon sm" onClick={() => openSettings()} aria-label="설정" data-tip="설정 (Ctrl+,)" data-tip-pos="left">
          <Settings2 size={16} />
        </button>
      </header>
      <nav className="stepbar">
        {STEPS.map((st, i) => (
          <div key={st.mode} className="step-wrap">
            {i > 0 && <span className="step-line" />}
            <button className={`step${mode === st.mode ? " on" : ""}${stepDone[st.mode] ? " done" : ""}`} onClick={() => setMode(st.mode)}>
              <span className="step-n">{stepDone[st.mode] ? <Check size={11} /> : st.n}</span>
              {st.label}
              <kbd>{st.n}</kbd>
            </button>
          </div>
        ))}
        <span className="spacer" />
        <FitVideoButton p={p} />
        <span className="step-stat">
          확인 <b>{reviewed}</b>/{songs.length}
          {excluded > 0 && <> · 제외 {excluded}</>}
        </span>
        <button className="btn ghost icon sm" onClick={() => undo(projectId)} disabled={!history?.undo.length} data-tip={history?.undo.length ? `되돌리기: ${history.undo[history.undo.length - 1].label} (Ctrl+Z)` : "되돌릴 편집이 없어요"} data-tip-pos="left">
          <Undo2 size={15} />
        </button>
        <button className="btn ghost icon sm" onClick={() => redo(projectId)} disabled={!history?.redo.length} data-tip="다시 하기 (Ctrl+Shift+Z)" data-tip-pos="left">
          <Redo2 size={15} />
        </button>
      </nav>
      <ReplayBanner p={p} />

      {mode === "review" && (
        <>
          <NextBanner p={p} />
          <div className="ws-body ws-review">
            <SongList p={p} />
            <main className="ws-main">
              <SongDetail p={p} />
            </main>
          </div>
          <OverviewMap p={p} />
          <Transport total={total} label={sel ? `선택: ${String(nums.get(sel.id) ?? "").padStart(2, "0")} ${sel.title || "제목 없음"}` : undefined} />
        </>
      )}
      {mode === "talk" && (
        <>
          <div className="ws-body ws-talk">
            <SongList p={p} talk />
            <main className="ws-main">
              <Suspense fallback={null}>
                <TalkEditor p={p} />
              </Suspense>
            </main>
          </div>
          <Transport total={total} />
        </>
      )}
      {mode === "export" && (
        <div className="ws-body export-body">
          <Suspense fallback={null}>
            <ExportView p={p} />
          </Suspense>
        </div>
      )}
      {ws.setlistOpen && <SetlistDialog p={p} />}
    </div>
  );
}
