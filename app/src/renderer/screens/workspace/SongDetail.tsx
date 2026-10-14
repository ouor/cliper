import { useEffect, useRef, useState } from "react";
import { AlertTriangle, ArrowDownToLine, Check, Combine, Download, Headphones, Plus, Scissors, SkipBack, SkipForward, Trash2, Undo2, X, Zap } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import type { Song } from "@ipc/generated/Song";
import { useWs } from "@/state/ws";
import { useApp } from "@/state/app";
import { editSong } from "@/state/projects";
import { songProgress, useJobs } from "@/state/jobs";
import { player } from "@/audio/player";
import { addSong, confirmSong, deleteSong, mergeSongs, setEdge, splitSong, toggleExclude } from "@/lib/edits";
import { fmtTime, parseTime } from "@/lib/format";
import { hasUnsafeFileChars, reviewStatus, songLabels, songNumbers } from "@/lib/song";
import { fetchVideo, songVideo, vodGone } from "@/lib/video";
import { useVideo } from "@/state/video";
import { VideoPane } from "@/components/VideoPane";
import { fetchSongs } from "@/lib/pipeline";
import { EdgeScopes } from "./EdgeScopes";
import { useMapData } from "./OverviewMap";

export function TimeField({ value, onCommit, label }: { value: number; onCommit: (v: number) => void; label: string }) {
  const [text, setText] = useState<string | null>(null);
  const [bad, setBad] = useState(false);
  return (
    <input
      className={`input sm mono time-field${bad ? " invalid" : ""}`}
      aria-label={label}
      value={text ?? fmtTime(value, 1, true)}
      onFocus={(e) => {
        setText(fmtTime(value, 1, true));
        requestAnimationFrame(() => e.target.select());
      }}
      onChange={(e) => {
        setText(e.target.value);
        setBad(parseTime(e.target.value) === null);
      }}
      onBlur={() => {
        const v = text !== null ? parseTime(text) : null;
        if (v !== null && Math.abs(v - value) > 0.01) onCommit(v);
        setText(null);
        setBad(false);
      }}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter") (e.target as HTMLInputElement).blur();
        if (e.key === "Escape") {
          setText(null);
          setBad(false);
          requestAnimationFrame(() => (e.target as HTMLInputElement).blur());
        }
      }}
      title="1:12:05, 72:05 모두 입력할 수 있어요"
    />
  );
}

let handledFocus = 0;

export function TextField({ value, placeholder, onCommit, field, className, maxLength }: { value: string; placeholder: string; onCommit: (v: string) => void; field?: "title" | "note"; className?: string; maxLength?: number }) {
  const [text, setText] = useState<string | null>(null);
  const ref = useRef<HTMLInputElement>(null);
  const focus = useWs((s) => s.focus);
  useEffect(() => {
    // one-shot: a remount (e.g. another song selected) must not steal focus again
    if (field && focus.field === field && focus.n !== handledFocus) {
      handledFocus = focus.n;
      ref.current?.focus();
      ref.current?.select();
    }
  }, [focus, field]);
  return (
    <input
      ref={ref}
      className={`input ${className ?? ""}`}
      value={text ?? value}
      placeholder={placeholder}
      maxLength={maxLength}
      spellCheck={false}
      onFocus={() => setText(value)}
      onChange={(e) => setText(e.target.value)}
      onBlur={() => {
        if (text !== null && text !== value) onCommit(text.trim());
        setText(null);
      }}
      onKeyDown={(e) => {
        e.stopPropagation();
        if (e.key === "Enter" && !e.nativeEvent.isComposing) ref.current?.blur();
        if (e.key === "Escape") {
          setText(null);
          requestAnimationFrame(() => ref.current?.blur());
        }
      }}
    />
  );
}

export function listenStart(s: Song) {
  const pre = useApp.getState().settings.listenPreroll;
  useWs.setState({ lastEdge: "start" });
  void player.play(Math.max(0, s.start - pre));
}
export function listenEnd(s: Song) {
  const pre = useApp.getState().settings.listenPreroll;
  useWs.setState({ lastEdge: "end" });
  void player.play(Math.max(0, s.end - pre), { stopAt: s.end + 3 });
}

function VideoPreview({ p }: { p: Project }) {
  const vs = useVideo((x) => x.byProject[p.id]);
  // no video anywhere yet: the review works by sound (the box says so instead of a cover)
  if (vs && (vs.local || vs.have_sec > 0 || vs.requests.length > 0 || vs.failed_sec > 0 || vodGone(p))) return <VideoPane p={p} />;
  const thumb = p.source?.catalog.thumbnail;
  if (p.audio?.dropped) {
    return (
      <div className="video-box empty">
        {thumb && <img src={thumb} alt="" draggable={false} />}
        <div className="video-note">
          <Headphones size={16} /> 소리를 정리한 작업이에요 · 위의 [소리 다시 받기]로 다시 들을 수 있어요
        </div>
      </div>
    );
  }
  return (
    <div className="video-box empty">
      {thumb && <img src={thumb} alt="" draggable={false} />}
      <div className="video-note">
        <Headphones size={16} /> 소리만 들을 수 있어요{p.source?.platform !== "local" ? " · 영상은 확인한 곡만 나중에 받아요" : ""}
      </div>
    </div>
  );
}

function CandidatePanel({ p, i }: { p: Project; i: number }) {
  const cd = p.detection?.candidates[i];
  if (!cd) return null;
  const why = { too_short: "짧아서", no_singing: "노래가 약해서", talk: "말소리가 많아서" }[cd.reason];
  return (
    <div className="detail-panel">
      <div className="dp-kicker">후보 구간 · 놓친 곡일 수 있어요</div>
      <div className="dp-big mono">
        {fmtTime(cd.start, 1, true)} ~ {fmtTime(cd.end, 1, true)}
      </div>
      <p className="muted">음악은 있지만 {why} 곡으로 넣지 않았어요. 들어 보고 노래라면 추가하세요.</p>
      <div className="row">
        <button className="btn subtle" onClick={() => void player.play(cd.start)}>
          <Headphones size={14} /> 들어 보기 <kbd>Space</kbd>
        </button>
        <button className="btn primary" onClick={() => void addSong(p.id, cd.start, cd.end)}>
          <Plus size={14} /> 곡으로 추가 <kbd>A</kbd>
        </button>
        <button className="btn ghost" onClick={() => useWs.setState({ candidate: null })}>
          닫기
        </button>
      </div>
    </div>
  );
}

function PendingPanel({ p, r }: { p: Project; r: [number, number] }) {
  return (
    <div className="detail-panel">
      <div className="dp-kicker">새 곡 범위</div>
      <div className="dp-big mono">
        {fmtTime(r[0], 1, true)} ~ {fmtTime(r[1], 1, true)}
      </div>
      <p className="muted">길이 {fmtTime(r[1] - r[0], 0)} · 곡으로 추가할까요?</p>
      <div className="row">
        <button className="btn primary" onClick={() => void addSong(p.id, r[0], r[1])}>
          <Plus size={14} /> 추가 <kbd>A</kbd>
        </button>
        <button className="btn subtle" onClick={() => void player.play(r[0])}>
          <Headphones size={14} /> 들어 보기
        </button>
        <button className="btn ghost" onClick={() => useWs.setState({ pendingRange: null })}>
          취소 <kbd>Esc</kbd>
        </button>
      </div>
    </div>
  );
}

export function SongDetail({ p }: { p: Project }) {
  const ws = useWs();
  const { probs } = useMapData(p);
  const shortSec = useApp((s) => s.settings.shortSec);
  const jobs = useJobs();
  const vsAll = useVideo((x) => x.byProject[p.id]);
  const s = p.songs.find((x) => x.id === ws.selected);

  if (ws.pendingRange) return <PendingPanel p={p} r={ws.pendingRange} />;
  if (ws.candidate !== null) return <CandidatePanel p={p} i={ws.candidate} />;
  if (!s) {
    return (
      <div className="detail-panel empty-detail">
        <p className="muted">목록에서 곡을 고르면 여기서 시작과 끝을 확인하고 고칠 수 있어요.</p>
        <p className="faint">
          <kbd>↑</kbd> <kbd>↓</kbd> 곡 이동 · <kbd>S</kbd> 시작 듣기 · <kbd>E</kbd> 끝 듣기 · <kbd>↵</kbd> 확인
        </p>
      </div>
    );
  }
  const n = songNumbers(p).get(s.id);
  const st = reviewStatus(s);
  const sv = songVideo(p, vsAll, s);
  const prog = songProgress(jobs, p.id).get(s.id);
  const labels = songLabels(p, s, shortSec);
  const sorted = [...p.songs].filter((x) => x.enabled).sort((a, b) => a.start - b.start);
  const nextSong = sorted[sorted.findIndex((x) => x.id === s.id) + 1];

  return (
    <div className="detail">
      <VideoPreview p={p} />
      <div className="detail-panel">
        <div className="dp-title-row">
          <span className="dp-num mono">{n ? String(n).padStart(2, "0") : "–"}</span>
          <TextField className="title-input grow" value={s.title} placeholder="제목 없음 · T로 입력" field="title" maxLength={200} onCommit={(v) => void editSong(p.id, s.id, "제목 입력", (x) => void (x.title = v))} />
          <TextField className="artist-input" value={s.artist} placeholder="원곡자" maxLength={120} onCommit={(v) => void editSong(p.id, s.id, "원곡자 입력", (x) => void (x.artist = v))} />
        </div>
        {hasUnsafeFileChars(s.title) && <div className="hint">파일 이름에 쓸 수 없는 문자(/ : ? * 등)는 내보낼 때 다른 문자로 바뀌어요.</div>}
        <div className="dp-labels">
          <span className={`tag ${st === "reviewed" ? "ok" : st === "excluded" ? "" : "outline"}`}>{{ reviewed: "확인", doubt: "미확인", unreviewed: "미확인", excluded: "제외됨" }[st]}</span>
          {labels.map((l) => (
            <span key={l.text} className={`tag ${l.kind === "plain" ? "outline" : l.kind}`} data-tip={l.tip}>
              {l.text}
            </span>
          ))}
        </div>
        <div className="dp-times">
          <span className="label">곡 시작</span>
          <TimeField label="곡 시작" value={s.start} onCommit={(v) => void setEdge(p.id, s.id, "start", v, { typed: true })} />
          <button className="btn sm subtle" onClick={() => listenStart(s)}>
            <SkipBack size={13} /> 시작 듣기 <kbd>S</kbd>
          </button>
          <button className="btn sm ghost" onClick={() => void setEdge(p.id, s.id, "start", player.now())} data-tip="현재 재생 위치를 곡 시작으로">
            여기로 <kbd>I</kbd>
          </button>
          <span className="label">곡 끝</span>
          <TimeField label="곡 끝" value={s.end} onCommit={(v) => void setEdge(p.id, s.id, "end", v, { typed: true })} />
          <button className="btn sm subtle" onClick={() => listenEnd(s)}>
            <SkipForward size={13} /> 끝 듣기 <kbd>E</kbd>
          </button>
          <button className="btn sm ghost" onClick={() => void setEdge(p.id, s.id, "end", player.now())} data-tip="현재 재생 위치를 곡 끝으로">
            여기로 <kbd>O</kbd>
          </button>
        </div>
        <div className="dp-meta">
          <span className="faint">길이</span> <span className="mono">{fmtTime(s.end - s.start, 1)}</span>
          <span className="faint">· 미세 조정</span> <kbd>Alt+←</kbd> <kbd>Alt+→</kbd> <span className="faint">0.1초 ({ws.lastEdge === "start" ? "시작" : "끝"})</span>
        </div>
        <EdgeScopes p={p} s={s} probs={probs} />
        <TextField className="note-input" value={s.note} placeholder="메모 (M) · 예: 다시 부름, 메들리, 2:13 말소리 들림" field="note" maxLength={500} onCommit={(v) => void editSong(p.id, s.id, "메모", (x) => void (x.note = v))} />

        {s.enabled && sv.state !== "none" && sv.state !== "ready" && (
          <div className={`banner ${sv.state === "gone" || sv.state === "failed" ? "warn" : "info"} dp-video`}>
            {sv.pending ? <Download size={15} /> : <AlertTriangle size={15} />}
            <span className="grow">
              {sv.label}
              {sv.state === "partial" && <span className="faint"> · 내보내기 전에 받아야 해요</span>}
            </span>
            {(sv.state === "partial" || sv.state === "failed") && (
              <div className="actions">
                <button className="btn sm subtle" onClick={() => void fetchVideo(p.id, { song_ids: [s.id], priority: "now", reason: "fit", process: (p.stems ?? []).some((x) => x.end > s.start && x.start < s.end) })}>
                  모자란 부분 받기
                </button>
              </div>
            )}
          </div>
        )}
        {prog && (
          <div className="banner info">
            <Download size={15} />
            <span className="grow">
              {prog.label}
              {prog.fraction !== null ? ` · ${Math.floor(prog.fraction * 100)}%` : ""}
            </span>
          </div>
        )}

        <div className="dp-actions">
          <button className="btn sm subtle" onClick={() => void toggleExclude(p.id, s.id)}>
            {s.enabled ? <X size={13} /> : <Undo2 size={13} />} {s.enabled ? "제외" : "복원"} <kbd>X</kbd>
          </button>
          <button className="btn sm subtle" onClick={() => void splitSong(p.id, s.id, player.now())} data-tip="재생 위치에서 두 곡으로 나눠요" data-tip-pos="top">
            <Scissors size={13} /> 나누기 <kbd>/</kbd>
          </button>
          <button className="btn sm subtle" disabled={!nextSong} onClick={() => nextSong && void mergeSongs(p.id, ws.multi.length > 1 ? ws.multi : [s.id, nextSong.id])} data-tip={ws.multi.length > 1 ? "선택한 곡을 하나로" : "다음 곡과 하나로 합쳐요"} data-tip-pos="top">
            <Combine size={13} /> {ws.multi.length > 1 ? `${ws.multi.length}곡 합치기` : "합치기"} <kbd>G</kbd>
          </button>
          <button className="btn sm ghost" onClick={() => void deleteSong(p.id, s.id)} aria-label="삭제">
            <Trash2 size={13} />
          </button>
          <span className="spacer" />
          {sv.state === "none" && s.enabled && !vodGone(p) && (
            <button className="btn sm subtle" onClick={() => void fetchSongs(p.id, [s.id])} data-tip="영상 받기 → 목소리 나누기 → 멘트 제안을 한 번에" data-tip-pos="left">
              <Zap size={13} /> 이 곡만 완성 <kbd>Ctrl+Shift+E</kbd>
            </button>
          )}
          <button className="btn primary" onClick={() => void confirmSong(p.id, s.id)} disabled={!s.enabled}>
            <Check size={14} /> 확인 완료 <kbd>↵</kbd>
          </button>
        </div>
        {s.confidence && s.confidence.doubts.includes("talk_inside") && !s.reviewed && (
          <div className="hint">
            <AlertTriangle size={11} /> 곡 중간에 말소리가 길게 있어요. 두 곡이 하나로 잡혔다면 <kbd>/</kbd>로 나누세요.
          </div>
        )}
        {requestFocusHint()}
      </div>
    </div>
  );
}

function requestFocusHint() {
  return (
    <div className="dp-foot faint">
      <ArrowDownToLine size={11} /> 다음 미확인 곡 <kbd>N</kbd> · 제목 <kbd>T</kbd> · 메모 <kbd>M</kbd> · 모든 단축키 <kbd>?</kbd>
    </div>
  );
}
