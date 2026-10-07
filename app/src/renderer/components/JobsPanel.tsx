import { useEffect, useState } from "react";
import { Ban, CheckCircle2, Clock, Loader2, Moon, RotateCw, Trash2, X, XCircle } from "lucide-react";
import { useApp, go, openSettings } from "@/state/app";
import { cancelJob, cancelWaiting, clearFinished, dismissJob, KIND_LABEL, noteKo, retryJob, STAGE_LABEL, useJobs, type Job } from "@/state/jobs";
import { useProjects } from "@/state/projects";
import { Progress } from "./common";
import { fmtDurationKo, fmtEta } from "@/lib/format";
import { call } from "@/lib/rpc";
import { projectTitle } from "@/lib/song";

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
  if (j.items.length <= 1 || (j.kind !== "clips.fetch" && j.kind !== "export.run")) return "";
  const done = Math.min(j.items.length, j.itemsDone.length);
  return `${done}/${j.items.length}${j.kind === "clips.fetch" ? "개 구간" : "곡"}`;
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
      {j.status === "running" && noteKo(j.message) && <div className="job-sub faint">{noteKo(j.message)}</div>}
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
  const groups = new Map<string, Job[]>();
  for (const j of jobs) groups.set(j.projectId, [...(groups.get(j.projectId) ?? []), j]);
  for (const w of s.waiting) if (!groups.has(w.projectId)) groups.set(w.projectId, []);
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
      {running && (
        <div className="banner info jobs-sleep">
          <Moon size={15} /> 절전 방지 켜짐 · 작업이 끝나면 꺼져요
        </div>
      )}
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
