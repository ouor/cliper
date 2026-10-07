// S1 홈 + S2 방송 확인 카드.
import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, ArrowRight, Clipboard, Film, FolderOpen, History, Layers, ListPlus, Mic2, MoreHorizontal, Play, Search, Settings2, Trash2, Users, X } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import type { ProjectSummary } from "@ipc/generated/ProjectSummary";
import type { AudioTrack } from "@ipc/generated/AudioTrack";
import { ask, go, openSettings, toast, useApp } from "@/state/app";
import { forgetProject, putProject, refreshList, useProjects } from "@/state/projects";
import { KIND_LABEL, startJob, takeInterrupted, useJobs } from "@/state/jobs";
import { call } from "@/lib/rpc";
import { friendly, type FriendlyError } from "@/lib/errors";
import { fmtBytes, fmtDate, fmtDurationKo, fmtRelative, fmtTime } from "@/lib/format";
import { platformLabel, projectTitle } from "@/lib/song";
import { startAnalysis, applyDefaultSettings } from "@/lib/pipeline";

const LINK_RE = /(https?:\/\/[^\s]+|soop:\S+|\b\d{6,10}\b)/g;
const VIDEO_EXT = ["mp4", "mkv", "mov", "flv", "ts", "webm", "m4v", "avi", "mka", "m4a", "wav", "mp3", "aac", "flac"];

function looksLikeInput(t: string): boolean {
  const s = t.trim();
  return /chzzk\.naver\.com\/(video|live)\//.test(s) || /(sooplive\.co\.kr|afreecatv\.com)\//.test(s) || /^soop:\S+$/.test(s) || /^\d{5,10}$/.test(s) || /^[a-zA-Z]:[\\/]/.test(s) || s.startsWith("\\\\");
}

function extractInputs(text: string): string[] {
  const lines = text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  if (lines.length === 1 && /^[a-zA-Z]:[\\/]/.test(lines[0])) return [lines[0]];
  const found = new Set<string>();
  for (const m of text.matchAll(LINK_RE)) found.add(m[0].replace(/[),.]+$/, ""));
  return [...found].filter(looksLikeInput);
}

interface Resolved {
  project: Project;
  existed: boolean;
}

function Thumb({ url, platform, size = "md" }: { url?: string | null; platform?: string | null; size?: "md" | "lg" }) {
  const [err, setErr] = useState(false);
  return (
    <div className={`thumb ${size}`}>
      {url && !err ? <img src={url} alt="" onError={() => setErr(true)} draggable={false} /> : platform === "local" ? <Film size={size === "lg" ? 28 : 18} /> : <Mic2 size={size === "lg" ? 28 : 18} />}
    </div>
  );
}

export function trackLabel(t: AudioTrack): string {
  return [`${t.index + 1}번`, t.title, t.language, `${t.codec} ${t.channels}ch`].filter(Boolean).join(" · ");
}

function BroadcastCard({ r, onCancel }: { r: Resolved; onCancel: () => void }) {
  const p = r.project;
  const src = p.source!;
  const settings = useApp((s) => s.settings);
  const heights = [...new Set(src.variants.map((v) => v.height))].sort((a, b) => b - a);
  const tracks = src.audio_tracks ?? [];
  const [track, setTrack] = useState(p.settings.audio_track ?? 0);
  const [height, setHeight] = useState<number | null>(settings.videoHeight && heights.includes(settings.videoHeight) ? settings.videoHeight : null);
  const [busy, setBusy] = useState(false);
  const songs = p.songs.filter((s) => s.enabled);
  const exported = songs.filter((s) => s.export).length;
  const dur = src.duration || 0;
  const local = src.platform === "local";
  const audioMin = local ? Math.max(5, dur / 600) : Math.max(10, dur / 240);
  const hasWork = !!p.audio;

  const start = async () => {
    setBusy(true);
    try {
      if (!r.existed || !p.audio) await applyDefaultSettings(p.id, { video_height: height, audio_track: track });
      go({ name: "workspace", projectId: p.id, mode: "review" });
      if (!p.audio || !p.detection) await startAnalysis(p.id, !p.audio);
    } catch (e) {
      const f = friendly(e);
      toast(`${f.title} ${f.hint ?? ""}`, { kind: "error" });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="bcard" onKeyDown={(e) => e.key === "Enter" && !busy && void start()}>
      <Thumb url={src.catalog.thumbnail} platform={src.platform} size="lg" />
      <div className="bcard-main">
        <div className="bcard-title">{src.title}</div>
        <div className="bcard-meta">
          {src.channel && <span>{src.channel}</span>}
          <span>{platformLabel(src.platform)}</span>
          {src.catalog.published_at && <span>{fmtDate(src.catalog.published_at)}</span>}
          {dur > 0 && <span>길이 {fmtTime(dur, 0, true)}</span>}
          {src.catalog.adult && <span className="tag danger">연령 제한</span>}
        </div>
        {r.existed && hasWork ? (
          <div className="banner accent">
            <History size={15} />
            <span className="grow">
              이 방송은 이미 작업 중이에요 (곡 {songs.length}개{exported ? ` · ${exported}곡 내보냄` : ""}).
            </span>
          </div>
        ) : (
          <div className="bcard-grid">
            {local && tracks.length > 1 && (
              <>
                <span className="label">트랙</span>
                <select className="select sm" value={track} onChange={(e) => setTrack(Number(e.target.value))}>
                  {tracks.map((t) => (
                    <option key={t.index} value={t.index}>
                      {trackLabel(t)}
                    </option>
                  ))}
                </select>
              </>
            )}
            {!local && heights.length > 0 && (
              <>
                <span className="label">화질</span>
                <select className="select sm" value={height ?? ""} onChange={(e) => setHeight(e.target.value ? Number(e.target.value) : null)}>
                  <option value="">{heights[0]}p (최고)</option>
                  {heights.slice(1).map((h) => (
                    <option key={h} value={h}>
                      {h}p
                    </option>
                  ))}
                </select>
              </>
            )}
            <span className="label">예상</span>
            <span>
              소리 {local ? "꺼내기" : "받기"} 약 {fmtDurationKo(audioMin)} → 곡 찾기 약 {fmtDurationKo(Math.max(5, dur / 900))}
            </span>
            <span className="label">공간</span>
            <span className="muted">약 {fmtBytes(Math.max(5e6, dur * 8400))} (영상은 고른 곡만 나중에 받아요)</span>
          </div>
        )}
        {/* engine notes are English log text; show only localized ones */}
        {src.notes.filter((n) => /[가-힣]/.test(n)).length > 0 && <div className="hint">{src.notes.filter((n) => /[가-힣]/.test(n)).join(" · ")}</div>}
      </div>
      <div className="bcard-actions">
        <button className="btn ghost" onClick={onCancel} disabled={busy}>
          취소
        </button>
        <button className="btn primary lg" onClick={() => void start()} disabled={busy} autoFocus>
          {r.existed && hasWork ? "이어서 하기" : "시작하기"} <kbd>↵</kbd>
        </button>
      </div>
    </div>
  );
}

function ContinueCard({ s }: { s: ProjectSummary }) {
  const night = (() => {
    const h = new Date(s.updated_at).getHours();
    return h < 7 || h >= 23;
  })();
  const parts = [
    s.unreviewed ? `확인 전 곡 ${s.unreviewed}개` : null,
    s.clips_ready ? `영상 ${s.clips_ready}곡 받음` : null,
    s.unconfirmed_mutes ? `멘트 확인 대기 ${s.unconfirmed_mutes}개` : null,
    s.exported ? `${s.exported}곡 내보냄` : null,
    s.last_error ? `실패: ${KIND_LABEL[s.last_error.kind] ?? s.last_error.kind}` : null,
  ].filter(Boolean);
  const mode = s.unreviewed ? "review" : s.unconfirmed_mutes ? "talk" : "export";
  return (
    <button className="continue-card" onClick={() => go({ name: "workspace", projectId: s.id, mode })}>
      <Thumb url={s.thumbnail} platform={s.platform} />
      <div className="grow">
        <div className="cc-kicker">
          {night ? "🌙 밤사이 완료" : "이어서 할 작업"} · {fmtRelative(s.updated_at)}
        </div>
        <div className="cc-title ellipsis">{s.title || s.id}</div>
        <div className="cc-meta">{parts.join(" · ") || "곡 찾기 전"}</div>
      </div>
      <span className="btn primary sm">
        이어서 하기 <ArrowRight size={14} />
      </span>
    </button>
  );
}

function ProjectRow({ s }: { s: ProjectSummary }) {
  const [menu, setMenu] = useState(false);
  const status = !s.has_detection ? (s.has_audio ? "곡 찾기 전" : "소리 받기 전") : s.songs === 0 ? "곡 없음" : s.exported >= s.songs ? "완료" : s.exported > 0 ? `${s.exported}곡 내보냄` : s.unreviewed ? `확인 전 ${s.unreviewed}` : s.unconfirmed_mutes ? `멘트 확인 ${s.unconfirmed_mutes}` : "검수 중";
  const err = s.last_error ? friendly({ code: s.last_error.code, message: s.last_error.message }) : null;
  const remove = async () => {
    setMenu(false);
    const r = await ask({
      title: "이 작업을 삭제할까요?",
      body: [projectTitle({ source: { title: s.title } } as Project) || s.id, "받은 소리와 영상, 편집 기록이 이 PC에서 지워져요. 내보낸 파일은 그대로 남아요. 되돌릴 수 없어요."],
      icon: "warn",
      buttons: [
        { id: "cancel", label: "취소", kind: "ghost" },
        { id: "delete", label: "삭제", kind: "danger" },
      ],
    });
    if (r !== "delete") return;
    try {
      await call("project.delete", { id: s.id });
      forgetProject(s.id);
      toast("작업을 삭제했어요.", { kind: "ok" });
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    }
  };
  return (
    <div className="proj-row" role="button" tabIndex={0} onClick={() => go({ name: "workspace", projectId: s.id, mode: "review" })} onKeyDown={(e) => e.key === "Enter" && go({ name: "workspace", projectId: s.id, mode: "review" })}>
      <Thumb url={s.thumbnail} platform={s.platform} />
      <div className="grow">
        <div className="pr-title ellipsis">{s.title || s.id}</div>
        <div className="pr-meta">
          {s.channel && <span>{s.channel}</span>}
          <span>{platformLabel(s.platform)}</span>
          {s.duration ? <span>{fmtTime(s.duration, 0, true)}</span> : null}
          <span>{s.songs}곡</span>
        </div>
      </div>
      {err && (
        <span className="tag danger" data-tip={`${KIND_LABEL[s.last_error!.kind] ?? s.last_error!.kind} 실패 · ${err.title}`} data-tip-pos="left">
          <AlertTriangle size={10} /> 실패
        </span>
      )}
      <span className={`tag ${status === "완료" ? "ok" : status.endsWith("전") ? "" : "accent"}`}>{status}</span>
      <span className="faint pr-date">{fmtRelative(s.updated_at)}</span>
      <div className="pr-menu" onClick={(e) => e.stopPropagation()}>
        <button className="btn ghost icon sm" onClick={() => setMenu(!menu)} aria-label="더 보기">
          <MoreHorizontal size={16} />
        </button>
        {menu && (
          <div className="menu" onMouseLeave={() => setMenu(false)}>
            <button onClick={() => void remove()}>
              <Trash2 size={14} /> 작업 삭제
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

export function Home() {
  const summaries = useProjects((s) => s.summaries);
  const listLoaded = useProjects((s) => s.listLoaded);
  const engineReady = useApp((s) => s.engine.state === "ready");
  const queue = useJobs((s) => s.queue);
  const settings = useApp((s) => s.settings);
  const [text, setText] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<FriendlyError | null>(null);
  const [resolved, setResolved] = useState<Resolved | null>(null);
  const [multi, setMulti] = useState<string[] | null>(null);
  const [through, setThrough] = useState<"detect" | "clips">("detect");
  const [search, setSearch] = useState("");
  const [byChannel, setByChannel] = useState(false);
  const [drag, setDrag] = useState(false);
  const [interrupted, setInterrupted] = useState<Awaited<ReturnType<typeof takeInterrupted>>>([]);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    inputRef.current?.focus();
    void takeInterrupted().then(setInterrupted);
  }, []);

  const resolve = async (input: string) => {
    setError(null);
    setResolved(null);
    setMulti(null);
    if (!looksLikeInput(input)) {
      setError(friendly({ code: "UNRECOGNIZED_INPUT", message: input }));
      return;
    }
    setLoading(true);
    const before = new Set(useProjects.getState().summaries.map((s) => s.id));
    try {
      const project = await call("project.create", { input: input.trim() });
      putProject(project);
      setResolved({ project, existed: before.has(project.id) });
      void refreshList();
    } catch (e) {
      setError(friendly(e));
    } finally {
      setLoading(false);
    }
  };

  const handleText = (t: string) => {
    const inputs = extractInputs(t);
    if (inputs.length > 1) {
      setMulti(inputs);
      setResolved(null);
      setError(null);
    } else if (inputs.length === 1) void resolve(inputs[0]);
    else if (t.trim()) setError(friendly({ code: "UNRECOGNIZED_INPUT", message: t }));
  };

  const cancelCard = async () => {
    const r = resolved;
    setResolved(null);
    setText("");
    inputRef.current?.focus();
    if (r && !r.existed && !r.project.audio) {
      try {
        await call("project.delete", { id: r.project.id });
        forgetProject(r.project.id);
      } catch {
        /* ignore */
      }
    }
  };

  const openFile = async () => {
    const p = await window.cliper.pickFile({ title: "녹화 파일 열기", filters: [{ name: "녹화 파일", extensions: VIDEO_EXT }] });
    if (p) {
      setText(p);
      void resolve(p);
    }
  };

  const addQueue = async () => {
    if (!multi) return;
    try {
      await call("queue.add", { inputs: multi, through });
      toast(`방송 ${multi.length}개를 대기열에 넣었어요. 순서대로 ${through === "clips" ? "영상 받기" : "곡 찾기"}까지 처리해요.`, { kind: "ok", action: { label: "작업 현황", run: () => useApp.setState({ jobsOpen: true }) } });
      setMulti(null);
      setText("");
    } catch (e) {
      setError(friendly(e));
    }
  };

  const resume = async () => {
    const list = interrupted;
    setInterrupted([]);
    for (const r of list) {
      try {
        await startJob(r.method, r.params as never, { label: r.label });
      } catch (e) {
        toast(friendly(e).title, { kind: "error" });
      }
    }
    useApp.setState({ jobsOpen: true });
  };

  const filtered = useMemo(() => {
    const q = search.trim().toLowerCase();
    // a just-resolved broadcast stays out of the list until it is started
    const hidden = resolved && !resolved.existed ? resolved.project.id : null;
    const list = summaries.filter((s) => s.id !== hidden);
    return q ? list.filter((s) => `${s.title ?? ""} ${s.channel ?? ""}`.toLowerCase().includes(q)) : list;
  }, [summaries, search, resolved]);
  const grouped = useMemo(() => {
    if (!byChannel) return null;
    const m = new Map<string, ProjectSummary[]>();
    for (const s of filtered) m.set(s.channel || platformLabel(s.platform) || "기타", [...(m.get(s.channel || platformLabel(s.platform) || "기타") ?? []), s]);
    return [...m.entries()];
  }, [filtered, byChannel]);

  const continueProjects = summaries
    .filter((x) => x.has_detection && x.songs > 0 && (x.unreviewed > 0 || x.unconfirmed_mutes > 0 || x.exported < x.songs || x.last_error))
    .slice(0, 2);

  return (
    <div
      className={`home${drag ? " dragging" : ""}`}
      onDragOver={(e) => {
        if (e.dataTransfer.types.includes("Files")) {
          e.preventDefault();
          setDrag(true);
        }
      }}
      onDragLeave={(e) => {
        if (e.currentTarget === e.target) setDrag(false);
      }}
      onDrop={(e) => {
        e.preventDefault();
        setDrag(false);
        const f = e.dataTransfer.files[0];
        if (!f) return;
        const path = window.cliper.pathForFile(f);
        if (path) {
          setText(path);
          void resolve(path);
        }
      }}
    >
      <header className="home-top">
        <div className="brand">
          <div className="brand-mark">
            <Mic2 size={15} />
          </div>
          Cliper
        </div>
        <span className="spacer" />
        <button className="btn ghost sm" onClick={() => useApp.setState({ overlay: "help" })}>
          단축키 <kbd>?</kbd>
        </button>
        <button className="btn ghost sm" onClick={() => openSettings()}>
          <Settings2 size={15} /> 설정
        </button>
      </header>

      <div className="home-scroll">
        <div className="home-inner">
          <section className="hero">
            <h1>다시보기 링크를 붙여넣으세요</h1>
            <div className={`link-box${error ? " error" : ""}${loading ? " loading" : ""}`}>
              <input
                ref={inputRef}
                className="link-input"
                placeholder="https://chzzk.naver.com/video/1234567"
                value={text}
                spellCheck={false}
                onChange={(e) => {
                  setText(e.target.value);
                  setError(null);
                }}
                onPaste={(e) => {
                  const t = e.clipboardData.getData("text");
                  if (t) {
                    e.preventDefault();
                    setText(t.includes("\n") ? t.split(/\r?\n/).filter(Boolean).join("  ") : t.trim());
                    handleText(t);
                  }
                }}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleText(text);
                  if (e.key === "Escape") setText("");
                }}
                disabled={!engineReady}
              />
              {loading ? (
                <div className="spinner" />
              ) : text ? (
                <button className="btn ghost icon sm" onClick={() => setText("")} aria-label="지우기">
                  <X size={15} />
                </button>
              ) : (
                <button
                  className="btn ghost sm"
                  onClick={async () => {
                    const t = await navigator.clipboard.readText().catch(() => "");
                    if (t) {
                      setText(t.trim());
                      handleText(t);
                    }
                  }}
                >
                  <Clipboard size={14} /> <kbd>Ctrl+V</kbd>
                </button>
              )}
            </div>
            <div className="hero-sub">
              <span>치지직 · SOOP 다시보기 링크 또는 치지직 영상 번호. 여러 개를 붙여넣으면 대기열에 넣어요.</span>
              <span className="spacer" />
              <button className="btn subtle sm" onClick={() => void openFile()} disabled={!engineReady}>
                <FolderOpen size={14} /> 녹화 파일 열기
              </button>
            </div>

            {loading && (
              <div className="bcard skeleton-card">
                <div className="thumb lg skeleton" />
                <div className="grow col">
                  <div className="skeleton" style={{ height: 16, width: "60%" }} />
                  <div className="skeleton" style={{ height: 12, width: "40%" }} />
                  <div className="faint">방송 정보를 불러오는 중…</div>
                </div>
              </div>
            )}
            {error && (
              <div className="banner error">
                <AlertTriangle size={15} />
                <div className="grow">
                  <div>{error.title}</div>
                  {error.hint && <div className="muted">{error.hint}</div>}
                  {error.detail && error.code !== "UNRECOGNIZED_INPUT" && (
                    <details>
                      <summary>자세히</summary>
                      <pre className="detail">{error.detail}</pre>
                    </details>
                  )}
                </div>
                <div className="actions">
                  {error.action === "retry" && (
                    <button className="btn sm subtle" onClick={() => handleText(text)}>
                      다시 시도
                    </button>
                  )}
                  {error.action === "cookies" && (
                    <button className="btn sm subtle" onClick={() => useApp.setState({ overlay: "cookies" })}>
                      로그인 쿠키 설정 (고급)
                    </button>
                  )}
                </div>
              </div>
            )}
            {resolved && <BroadcastCard r={resolved} onCancel={() => void cancelCard()} />}
            {multi && (
              <div className="bcard multi">
                <div className="grow col">
                  <div className="bcard-title">
                    <ListPlus size={16} /> 방송 {multi.length}개를 대기열에 넣을까요?
                  </div>
                  <ol className="multi-list">
                    {multi.map((m) => (
                      <li key={m} className="ellipsis mono">
                        {m}
                      </li>
                    ))}
                  </ol>
                  <div className="row">
                    <span className="label">어디까지</span>
                    <div className="seg">
                      <button className={through === "detect" ? "on" : ""} onClick={() => setThrough("detect")}>
                        곡 찾기까지
                      </button>
                      <button className={through === "clips" ? "on" : ""} onClick={() => setThrough("clips")}>
                        영상 받기와 멘트 제안까지
                      </button>
                    </div>
                  </div>
                  <div className="hint">순서대로 하나씩 처리해요. 밤새 돌려 두고 아침에 확인하세요. 처리 중에는 PC가 절전 모드로 들어가지 않아요.</div>
                </div>
                <div className="bcard-actions">
                  <button className="btn ghost" onClick={() => setMulti(null)}>
                    취소
                  </button>
                  <button className="btn primary lg" onClick={() => void addQueue()}>
                    대기열에 넣기
                  </button>
                </div>
              </div>
            )}
          </section>

          {interrupted.length > 0 && (
            <div className="banner warn">
              <History size={15} />
              <span className="grow">지난번에 멈춘 작업 {interrupted.length}개가 있어요. 다 받은 영상과 내보낸 곡은 건너뛰고 이어서 해요(소리 받기는 처음부터 다시 받아요).</span>
              <div className="actions">
                <button className="btn sm ghost" onClick={() => setInterrupted([])}>
                  나중에
                </button>
                <button className="btn sm primary" onClick={() => void resume()}>
                  이어서 하기
                </button>
              </div>
            </div>
          )}

          {continueProjects.length > 0 && (
            <section className="home-section">
              <div className="section-title">이어서 할 작업</div>
              {continueProjects.map((x) => (
                <ContinueCard key={x.id} s={x} />
              ))}
            </section>
          )}

          {queue.length > 0 && (
            <section className="home-section">
              <div className="section-title">
                <Layers size={14} /> 여러 방송 대기열
              </div>
              <div className="queue-list">
                {queue.map((q) => (
                  <div key={q.id} className="queue-row">
                    <span className={`tag ${q.status === "done" ? "ok" : q.status === "failed" ? "danger" : q.status === "running" ? "accent" : ""}`}>
                      {{ waiting: "대기", running: "처리 중", done: "완료", failed: "실패", cancelled: "취소" }[q.status]}
                    </span>
                    <span className="grow ellipsis">{(q.project_id && summaries.find((s) => s.id === q.project_id)?.title) || q.input}</span>
                    {q.status === "failed" && <span className="faint ellipsis" style={{ maxWidth: 260 }}>{friendly(q.error).title}</span>}
                    {q.status === "done" && q.project_id && (
                      <button className="btn xs subtle" onClick={() => go({ name: "workspace", projectId: q.project_id!, mode: "review" })}>
                        검수하기
                      </button>
                    )}
                  </div>
                ))}
              </div>
            </section>
          )}

          <section className="home-section">
            <div className="section-title">최근 작업</div>
            {(filtered.length > 0 || search) && (
              <div className="row list-tools">
                <div className="search">
                  <Search size={14} />
                  <input className="input sm" placeholder="방송 제목, 채널 검색" value={search} onChange={(e) => setSearch(e.target.value)} />
                </div>
                <button className={`btn sm ${byChannel ? "active" : "ghost"}`} onClick={() => setByChannel(!byChannel)}>
                  <Users size={14} /> 채널별
                </button>
              </div>
            )}
            {!listLoaded && engineReady && <div className="skeleton" style={{ height: 56 }} />}
            {listLoaded && filtered.length === 0 && !search && (
              <div className="empty-home">
                <p>아직 작업한 방송이 없어요. 위에 링크를 붙여넣으면 시작돼요.</p>
                <div className="steps3">
                  <span>
                    <b>1</b> 링크 붙여넣기
                  </span>
                  <ArrowRight size={14} />
                  <span>
                    <b>2</b> 곡 확인
                  </span>
                  <ArrowRight size={14} />
                  <span>
                    <b>3</b> 내보내기
                  </span>
                </div>
              </div>
            )}
            {grouped
              ? grouped.map(([ch, list]) => (
                  <div key={ch} className="channel-group">
                    <div className="channel-name">
                      {ch} <span className="faint">{list.length}</span>
                    </div>
                    {list.map((s) => (
                      <ProjectRow key={s.id} s={s} />
                    ))}
                  </div>
                ))
              : filtered.map((s) => <ProjectRow key={s.id} s={s} />)}
          </section>
          <div className="home-foot faint">
            <Play size={12} /> 처음이라면 <kbd>?</kbd>로 단축키를 확인해 보세요 · 기본 화질 {settings.videoHeight ? `${settings.videoHeight}p` : "최고"} · 앞뒤 여유 {settings.marginSec}초
          </div>
        </div>
      </div>
      {drag && (
        <div className="drop-overlay">
          <Film size={32} />
          <div>녹화 파일을 놓으면 새 작업을 만들어요</div>
          <div className="faint">원본 파일은 바뀌지 않아요</div>
        </div>
      )}
    </div>
  );
}

