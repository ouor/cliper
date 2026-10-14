import { useEffect, useState } from "react";
import { Ban, CheckCircle2, Clock, Download, Loader2, Moon, Pause, Play, RotateCw, Trash2, X, XCircle } from "lucide-react";
import { setPaused, useVideo } from "@/state/video";
import { useApp, go, openSettings } from "@/state/app";
import { cancelJob, cancelWaiting, clearFinished, dismissJob, KIND_LABEL, noteKo, retryJob, STAGE_LABEL, useJobs, type Job } from "@/state/jobs";
import { useProjects } from "@/state/projects";
import { Progress } from "./common";
import { fmtDurationKo, fmtEta } from "@/lib/format";
import { call } from "@/lib/rpc";
import { projectTitle, songNumbers } from "@/lib/song";

function useNow(ms = 1000) {
  const [, set] = useState(0);
  useEffect(() => {
    const t = setInterval(() => set((x) => x + 1), ms);
    return () => clearInterval(t);
  }, [ms]);
}

export function jobProgressText(j: Job): string {
  if (j.status === "done") return `완료 · ${fmtDurationKo(((j.finishedAt ?? Date.now()) - j.startedAt) / 1000)}`;
  if (j.status === "cancelled") return "취소함 · 끝난 곡은 남아 있어요";
  if (j.status === "failed") return j.error?.title ?? "실패";
  const pct = j.fraction !== null ? `${Math.floor(j.fraction * 100)}%` : "";
  const now = Date.now();
  if (now - j.lastChangeAt > 10000) return `${pct ? pct + " · " : ""}아직 처리 중이에요 (${j.stage === "video.fetch" || j.stage === "audio.fetch" ? "받는 속도가 느려요" : "시간이 조금 걸리고 있어요"})`;
  const eta = now - j.startedAt < 5000 || j.eta === null ? "계산 중…" : fmtEta(j.eta);
  return [pct, eta].filter(Boolean).join(" · ");
}

function itemsText(j: Job): string {
  if (j.items.length <= 1 || j.kind !== "export.run") return "";
  const done = Math.min(j.items.length, j.itemsDone.length);
  return `${done}/${j.items.length}곡`;
}

function JobRow({ j }: { j: Job }) {
  const icon =
    j.status === "running" ? <Loader2 size={15} className="spin" /> : j.status === "done" ? <CheckCircle2 size={15} color="var(--success)" /> : j.status === "failed" ? <XCircle size={15} color="var(--danger)" /> : <Ban size={15} color="var(--text-3)" />;
  return (
    <div className={`job-row ${j.status}`}>
      <div className="job-head">
        {icon}
        <span className="job-title">{j.request?.label ?? KIND_LABEL[j.kind] ?? j.kind}</span>
        {j.status === "running" && <span className="faint job-stage">{STAGE_LABEL[j.stage] ?? ""}</span>}
        <span className="spacer" />
        <span className="faint">{itemsText(j)}</span>
        {j.status === "running" && (
          <button className="btn xs ghost" onClick={() => void cancelJob(j.id)} data-tip="끝난 곡은 남아요" data-tip-pos="left">
            취소
          </button>
        )}
        {j.status !== "running" && (
          <button className="btn icon xs ghost" onClick={() => dismissJob(j.id)} aria-label="목록에서 지우기">
            <X size={13} />
          </button>
        )}
      </div>
      {j.status === "running" && <Progress value={j.fraction} />}
      <div className={`job-sub ${j.status === "failed" ? "err" : ""}`}>{jobProgressText(j)}</div>
      {j.status === "running" && noteKo(j.note) && <div className="job-sub faint">{noteKo(j.note)}</div>}
      {j.status === "failed" && j.error && (
        <div className="job-fail">
          {j.error.hint && <div className="muted">{j.error.hint}</div>}
          <div className="row">
            {j.request && (
              <button className="btn xs subtle" onClick={() => void retryJob(j)}>
                <RotateCw size={12} /> 다시 시도
              </button>
            )}
            {j.error.action === "cookies" && (
              <button className="btn xs subtle" onClick={() => openSettings("advanced")}>
                로그인 쿠키 설정
              </button>
            )}
            {j.error.action === "settings" && (
              <button className="btn xs subtle" onClick={() => openSettings("general")}>
                작업 폴더 바꾸기
              </button>
            )}
          </div>
          {j.error.detail && (
            <details>
              <summary>자세히</summary>
              <pre className="detail">{j.error.detail}</pre>
            </details>
          )}
        </div>
      )}
    </div>
  );
}

export function JobsPanel() {
  useNow(1000);
  const s = useJobs();
  const projects = useProjects((x) => x.projects);
  const summaries = useProjects((x) => x.summaries);
  const jobs = s.order.map((id) => s.jobs[id]).filter(Boolean);
  const running = jobs.some((j) => j.status === "running");
  const keepAwake = useApp((x) => x.system?.keep_awake ?? true);
  const groups = new Map<string, Job[]>();
  for (const j of jobs) groups.set(j.projectId, [...(groups.get(j.projectId) ?? []), j]);
  for (const w of s.waiting) if (!groups.has(w.projectId)) groups.set(w.projectId, []);
  const videos = useVideo((x) => x.byProject);
  for (const [pid, v] of Object.entries(videos)) if (!groups.has(pid) && (v.requests.some((r) => r.remaining_sec > 0.05) || v.failed_sec > 0.05)) groups.set(pid, []);
  const titleOf = (id: string) => projectTitle(projects[id]) || summaries.find((x) => x.id === id)?.title || id;
  const queue = s.queue;

  return (
    <aside className="jobs-panel" role="dialog" aria-label="작업 현황">
      <header>
        <span className="grow">작업 현황</span>
        <kbd>Ctrl+J</kbd>
        <button className="btn ghost icon sm" onClick={() => useApp.setState({ jobsOpen: false })} aria-label="닫기">
          <X size={16} />
        </button>
      </header>
      {running && keepAwake && (
        <div className="banner info jobs-sleep">
          <Moon size={15} /> 절전 방지 켜짐 · 작업이 끝나면 꺼져요
        </div>
      )}
      <VideoPauseBar />
      <div className="jobs-body">
        {groups.size === 0 && queue.length === 0 && <div className="jobs-empty">진행 중인 작업이 없어요.</div>}
        {[...groups.entries()].map(([pid, list]) => (
          <section key={pid} className="job-group">
            <button className="job-group-title" onClick={() => go({ name: "workspace", projectId: pid, mode: "review" })}>
              {titleOf(pid)}
            </button>
            {s.waiting
              .filter((w) => w.projectId === pid)
              .map((w) => (
                <div key={w.key + w.addedAt} className="job-row waiting">
                  <div className="job-head">
                    <Clock size={15} color="var(--text-3)" />
                    <span className="job-title">{w.label ?? KIND_LABEL[w.method]}</span>
                    <span className="spacer" />
                    <button className="btn xs ghost" onClick={() => cancelWaiting(w)}>
                      취소
                    </button>
                  </div>
                  <div className="job-sub faint">대기 · 앞 작업이 끝나면 시작해요</div>
                </div>
              ))}
            <VideoRow pid={pid} />
            {list.map((j) => (
              <JobRow key={j.id} j={j} />
            ))}
          </section>
        ))}
        {queue.length > 0 && (
          <section className="job-group">
            <div className="job-group-title static">여러 방송 대기열</div>
            {queue.map((q) => (
              <div key={q.id} className={`job-row ${q.status}`}>
                <div className="job-head">
                  {q.status === "running" ? <Loader2 size={15} className="spin" /> : q.status === "done" ? <CheckCircle2 size={15} color="var(--success)" /> : q.status === "failed" ? <XCircle size={15} color="var(--danger)" /> : <Clock size={15} color="var(--text-3)" />}
                  <span className="job-title ellipsis">{(q.project_id && titleOf(q.project_id)) || q.input}</span>
                  <span className="spacer" />
                  {(q.status === "waiting" || q.status === "running") && (
                    <button className="btn xs ghost" onClick={() => void call("queue.cancel", { entry_id: q.id })}>
                      취소
                    </button>
                  )}
                </div>
                <div className={`job-sub ${q.status === "failed" ? "err" : "faint"}`}>
                  {q.status === "waiting" && "대기 중"}
                  {q.status === "running" && (KIND_LABEL[q.step ?? ""] ?? "방송 정보 확인") + " 중"}
                  {q.status === "done" && `완료 · ${q.through === "clips" ? "영상 받기까지" : "곡 찾기까지"}`}
                  {q.status === "failed" && (q.error?.message ?? "실패")}
                  {q.status === "cancelled" && "취소함"}
                </div>
              </div>
            ))}
          </section>
        )}
      </div>
      <footer>
        <button className="btn sm ghost" onClick={() => {
          clearFinished();
          void call("queue.clear", {}).catch(() => {});
        }}>
          <Trash2 size={13} /> 끝난 작업 지우기
        </button>
      </footer>
    </aside>
  );
}

const REASON_LABEL: Record<string, string> = {
  song: "곡 영상",
  here: "여기 영상 (앞뒤 1분)",
  range: "고른 범위",
  topup: "넓힌 경계",
  fit: "영상 맞추기",
  margin: "늘린 앞뒤 여유",
  export: "내보내기에 필요한 영상",
  batch: "밤새 처리",
  user: "영상 받기",
};

/** One project's video downloads (14.7): remaining amount, ETA, requests, failures. */
function VideoRow({ pid }: { pid: string }) {
  const vs = useVideo((x) => x.byProject[pid]);
  const p = useProjects((x) => x.projects[pid]);
  const [open, setOpen] = useState(false);
  if (!vs) return null;
  const reqs = vs.requests.filter((r) => r.remaining_sec > 0.05 || r.failed_sec > 0.05);
  const remaining = reqs.reduce((n, r) => n + r.remaining_sec, 0);
  const total = reqs.reduce((n, r) => n + r.total_sec, 0);
  const have = reqs.reduce((n, r) => n + r.have_sec, 0);
  if (!reqs.length && vs.failed_sec < 0.05) return null;
  const nums = p ? songNumbers(p) : new Map<string, number>();
  const songsText = (ids: string[]) => ids.map((id) => nums.get(id)).filter((n): n is number => !!n).map((n) => String(n).padStart(2, "0")).join(", ");
  return (
    <div className={`job-row ${remaining > 0 ? "running" : "failed"}`}>
      <div className="job-head">
        {remaining > 0 ? vs.paused ? <Pause size={15} color="var(--text-3)" /> : <Loader2 size={15} className="spin" /> : <XCircle size={15} color="var(--danger)" />}
        <span className="job-title">영상 받기</span>
        <span className="faint job-stage">{reqs.length > 1 ? `요청 ${reqs.length}개` : reqs[0] ? REASON_LABEL[reqs[0].reason] ?? "" : ""}</span>
        <span className="spacer" />
        {reqs.length > 0 && (
          <button className="btn xs ghost" onClick={() => setOpen(!open)}>
            {open ? "접기" : "자세히"}
          </button>
        )}
        {remaining > 0 && (
          <button
            className="btn xs ghost"
            onClick={() => {
              // export fetches stay: cancelling them would fail a running [받고 내보내기] (U-22)
              for (const r of reqs) if (r.reason !== "export" && r.remaining_sec > 0.05) void call("video.cancel", { id: pid, request_id: r.id }).catch(() => {});
            }}
            data-tip={"아직 받지 않은 부분만 취소해요. 받은 영상은 남아요\n내보내기에 필요한 받기는 그대로 둬요"}
            data-tip-pos="left"
          >
            취소
          </button>
        )}
      </div>
      {remaining > 0 && <Progress value={total > 0 ? have / total : null} />}
      <div className="job-sub">
        {remaining > 0
          ? vs.offline
            ? `인터넷 연결을 기다리는 중 · ${Math.max(1, Math.round(vs.offline.retry_in_sec))}초 뒤 다시 · 남은 분량 ${fmtDurationKo(remaining)}`
            : `남은 분량 ${fmtDurationKo(remaining)}${vs.paused ? (reqs.some((r) => r.force) ? " · 내보내기용은 일시정지 중에도 받는 중" : " · 일시정지됨") : vs.eta_sec ? ` · ${fmtEta(vs.eta_sec)}` : " · 계산 중…"}`
          : ""}
      </div>
      {vs.failed_sec > 0.05 && (
        <div className="job-fail">
          <div className="row">
            <span className="err grow">받지 못한 영상 {fmtDurationKo(vs.failed_sec)}{vs.ranges.find((r) => r.state === "failed")?.code === "NETWORK" ? " · 연결이 끊겼어요" : ""}</span>
            {p?.vod?.status !== "gone" && (
              <button className="btn xs subtle" onClick={() => void call("video.retry", { id: pid }).catch(() => {})}>
                <RotateCw size={12} /> 다시 시도
              </button>
            )}
          </div>
        </div>
      )}
      {open && (
        <div className="video-reqs">
          {reqs.map((r) => (
            <div key={r.id} className="video-req">
              <span className="grow ellipsis">
                {REASON_LABEL[r.reason] ?? r.reason}
                {r.force && <span className="tag outline"> 일시정지 중에도 받는 중</span>}
                {r.song_ids.length ? ` · 곡 ${songsText(r.song_ids)}` : ""}
                <span className="faint"> · {r.remaining_sec > 0.05 ? `남은 ${fmtDurationKo(r.remaining_sec)}` : "받지 못한 부분 있음"}</span>
              </span>
              {r.priority === "now" ? (
                <span className="tag outline">먼저</span>
              ) : (
                r.remaining_sec > 0.05 && (
                  <button className="btn xs ghost" onClick={() => void call("video.prioritize", { id: pid, request_id: r.id, priority: "now" }).catch(() => {})}>
                    먼저 받기
                  </button>
                )
              )}
              <button className="btn icon xs ghost" onClick={() => void call("video.cancel", { id: pid, request_id: r.id }).catch(() => {})} aria-label="이 요청 취소">
                <X size={12} />
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/** [영상 받기 모두 일시정지] / [다시 시작] — app-wide (14.7). */
function VideoPauseBar() {
  const paused = useVideo((x) => x.paused);
  const active = useVideo((x) => Object.values(x.byProject).some((v) => v.requests.some((r) => r.remaining_sec > 0.05)));
  if (!paused && !active) return null;
  return (
    <div className={`banner ${paused ? "warn" : "info"} jobs-sleep`}>
      {paused ? <Pause size={15} /> : <Download size={15} />}
      <span className="grow">{paused ? "영상 받기 일시정지됨 · 편집과 재생은 그대로 돼요" : "영상을 받는 중이에요 · 앱 전체 동시 4개"}</span>
      <button className="btn xs subtle" onClick={() => void setPaused(!paused)}>
        {paused ? (
          <>
            <Play size={12} /> 다시 시작
          </>
        ) : (
          <>
            <Pause size={12} /> 모두 일시정지
          </>
        )}
      </button>
    </div>
  );
}
