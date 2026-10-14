import { CudaAddonStatus } from "./CudaAddonCard";
import { useEffect, useState } from "react";
import { Cpu, Download, HardDrive, Moon, Pause, ShieldCheck } from "lucide-react";
import { useVideo } from "@/state/video";
import { useApp } from "@/state/app";
import { runningJobs, useJobs } from "@/state/jobs";
import { usePlayer, LISTEN_LABEL } from "@/audio/player";
import { fmtBytes, fmtDurationKo, fmtEta } from "@/lib/format";

export function StatusBar() {
  const root = useApp((s) => s.settings.projectsRoot);
  const route = useApp((s) => s.route);
  const engine = useApp((s) => s.engine.state);
  const keepAwake = useApp((s) => s.system?.keep_awake ?? true);
  const jobs = useJobs();
  const mode = usePlayer((s) => s.mode);
  const source = usePlayer((s) => s.source);
  const [free, setFree] = useState<number | null>(null);
  const paused = useVideo((s) => s.paused);
  const videoLeft = useVideo((s) => Object.values(s.byProject).reduce((n, v) => n + v.requests.reduce((m, r) => m + r.remaining_sec, 0), 0));

  useEffect(() => {
    if (!root) return;
    let alive = true;
    const check = () => window.cliper.checkFolder(root).then((r) => alive && setFree(r.ok ? r.free : null));
    check();
    const t = setInterval(check, 60000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [root]);

  const running = runningJobs(jobs);
  const waiting = jobs.waiting.length;
  const queueActive = jobs.queue.filter((q) => q.status === "waiting" || q.status === "running").length;
  const etas = running.map((j) => j.eta).filter((x): x is number => x !== null);
  const failed = jobs.order.map((id) => jobs.jobs[id]).filter((j) => j?.status === "failed").length;
  const busy = running.length > 0;

  let summary = "작업 없음";
  if (busy) summary = `작업 ${running.length}개 진행 중${etas.length ? ` · ${fmtEta(Math.max(...etas)).replace(" 남음", "")}` : ""}`;
  if (waiting) summary += ` · 대기 ${waiting}`;
  if (queueActive) summary += ` · 대기열 ${queueActive}`;

  return (
    <footer className="statusbar">
      <span className="sb-item" data-tip="영상과 목소리를 외부로 보내지 않아요" data-tip-pos="top">
        <ShieldCheck size={13} /> 이 PC에서 처리
      </span>
      {root && (
        <span className="sb-item faint" data-tip={root} data-tip-pos="top">
          <HardDrive size={13} /> {root.length > 40 ? "…" + root.slice(-38) : root}
          {free !== null && <> ({fmtBytes(free)} 남음)</>}
        </span>
      )}
      {engine !== "ready" && (
        <span className="sb-item" style={{ color: engine === "failed" ? "var(--danger)" : "var(--doubt)" }}>
          <Cpu size={13} /> {engine === "starting" ? "엔진 준비 중…" : engine === "restarting" ? "엔진 다시 시작 중…" : engine === "failed" ? "엔진 멈춤" : ""}
        </span>
      )}
      <span className="spacer" />
      <CudaAddonStatus />
      {route.name === "workspace" && route.mode === "talk" && source === "stems" && (
        <span className={`sb-item listen-pill m${mode}`}>듣는 중: {LISTEN_LABEL[mode]}</span>
      )}
      {paused ? (
        <button className="sb-item sb-paused" onClick={() => useApp.setState({ jobsOpen: true })} data-tip="작업 현황에서 다시 시작할 수 있어요" data-tip-pos="top">
          <Pause size={13} /> 영상 받기 일시정지됨
        </button>
      ) : (
        videoLeft > 0.05 && (
          <span className="sb-item faint" data-tip="작업 현황(Ctrl+J)에서 자세히 볼 수 있어요" data-tip-pos="top">
            <Download size={13} /> 영상 받는 중 · 남은 {fmtDurationKo(videoLeft)}
          </span>
        )
      )}
      {busy && keepAwake && (
        <span className="sb-item faint" data-tip="작업이 끝나면 꺼져요" data-tip-pos="top">
          <Moon size={13} /> 절전 방지 켜짐
        </span>
      )}
      <button className={`sb-jobs${busy ? " busy" : ""}${failed ? " failed" : ""}`} onClick={() => useApp.setState((s) => ({ jobsOpen: !s.jobsOpen }))}>
        {busy && <span className="pulse" />}
        {summary}
        {failed > 0 && <span className="tag danger">실패 {failed}</span>}
        <kbd>Ctrl+J</kbd>
      </button>
    </footer>
  );
}
