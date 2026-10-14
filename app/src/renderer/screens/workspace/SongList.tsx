import { useEffect, useMemo, useRef } from "react";
import { AlertTriangle, Ban, CheckCircle2, ChevronDown, ChevronRight, Circle, Download, Plus, RefreshCw, Scissors, Search, Upload } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import type { Song } from "@ipc/generated/Song";
import { useWs, type SortMode } from "@/state/ws";
import { useApp } from "@/state/app";
import { songProgress, useJobs } from "@/state/jobs";
import { reviewStatus, songLabels, songNumbers, songTitle, unconfirmedMutes } from "@/lib/song";
import { songVideo, spanFor, type SongVideo } from "@/lib/video";
import { useVideo } from "@/state/video";
import { fmtTime } from "@/lib/format";
import { player } from "@/audio/player";
import { addSong } from "@/lib/edits";

const ATTENTION: Record<string, number> = { doubt: 0, unreviewed: 1, reviewed: 2, excluded: 3 };

export function sortSongs(p: Project, mode: SortMode, search: string): Song[] {
  const q = search.trim().toLowerCase();
  let list = p.songs.filter((s) => !q || `${s.title} ${s.artist} ${s.note}`.toLowerCase().includes(q));
  list = [...list].sort((a, b) => a.start - b.start);
  if (mode === "attention") list.sort((a, b) => ATTENTION[reviewStatus(a)] - ATTENTION[reviewStatus(b)] || a.start - b.start);
  if (mode === "length") list.sort((a, b) => a.end - a.start - (b.end - b.start));
  return list;
}

function StatusIcon({ s, sv, ready, prog, talk }: { s: Song; sv: SongVideo; ready: boolean; prog?: { label: string; fraction: number | null; waiting?: boolean }; talk: boolean }) {
  const st = reviewStatus(s);
  if ((prog && !prog.waiting) || (st !== "excluded" && sv.pending)) return <Download size={14} className="si si-progress" aria-label="받는 중" />;
  if (st !== "excluded" && sv.state === "gone") return <Ban size={14} className="si si-warn" aria-label="영상 부족 · 다시보기 없음" />;
  if (st !== "excluded" && (sv.state === "partial" || sv.state === "failed")) return <RefreshCw size={14} className="si si-warn" aria-label="영상 부족" />;
  if (talk && ready && unconfirmedMutes(s) > 0) return <Scissors size={14} className="si si-suggest" aria-label="멘트 확인 대기" />;
  if (st !== "excluded" && s.export && !talk) return <Upload size={14} className="si si-ok" aria-label="내보냄" />;
  if (st === "reviewed") return <CheckCircle2 size={14} className="si si-ok" aria-label="확인" />;
  if (st === "doubt") return <AlertTriangle size={14} className="si si-warn" aria-label="확인 필요" />;
  return <Circle size={9} className="si si-dot" aria-label="미확인" />;
}

function Row({ p, s, sv, n, selected, multi, prog, talk, shortSec }: { p: Project; s: Song; sv: SongVideo; n?: number; selected: boolean; multi: boolean; prog?: { label: string; fraction: number | null; waiting?: boolean }; talk: boolean; shortSec: number }) {
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (selected) ref.current?.scrollIntoView({ block: "nearest" });
  }, [selected]);
  const st = reviewStatus(s);
  // the talk editor works where the song has separated audio
  const ready = !!spanFor(p, s.start, s.end);
  const labels = songLabels(p, s, shortSec).filter((l) => !(talk && l.kind === "plain"));
  const unconf = unconfirmedMutes(s);
  const muteOff = !s.mute_enabled;
  const statusText = (() => {
    if (st === "excluded") return "제외됨";
    const parts: string[] = [st === "reviewed" ? "확인" : "미확인"];
    if (sv.label) parts.push(sv.label);
    if (s.export) parts.push("내보냄");
    return parts.join(" · ");
  })();
  return (
    <div
      ref={ref}
      className={`song-row ${st}${selected ? " selected" : ""}${multi ? " multi" : ""}`}
      role="option"
      aria-selected={selected}
      onClick={(e) => {
        const ws = useWs.getState();
        if (e.shiftKey) {
          const base = ws.multi.length ? ws.multi : ws.selected ? [ws.selected] : [];
          useWs.setState({ multi: base.includes(s.id) ? base.filter((x) => x !== s.id) : [...base, s.id], selected: s.id });
        } else useWs.setState({ selected: s.id, multi: [], selectedMute: null, candidate: null });
      }}
      onDoubleClick={() => void player.play(Math.max(0, s.start - useApp.getState().settings.listenPreroll))}
    >
      <StatusIcon s={s} sv={sv} ready={ready} prog={prog} talk={talk} />
      <span className="sr-num mono">{n ? String(n).padStart(2, "0") : "–"}</span>
      <div className="sr-main">
        <div className="sr-line1">
          <span className={`sr-title ellipsis${s.title ? "" : " untitled"}`}>{songTitle(s)}</span>
          {s.artist && <span className="sr-artist ellipsis">{s.artist}</span>}
          <span className="spacer" />
          <span className="sr-dur mono">{fmtTime(s.end - s.start, 0)}</span>
        </div>
        <div className="sr-line2">
          <span className="mono faint">{fmtTime(s.start, 0, true)}</span>
          {prog ? (
            <span className="sr-prog">
              {prog.label}
              {prog.fraction !== null && ` ${Math.floor(prog.fraction * 100)}%`}
              {prog.fraction !== null && (
                <span className="mini-bar">
                  <i style={{ width: `${prog.fraction * 100}%` }} />
                </span>
              )}
            </span>
          ) : talk ? (
            <span className="sr-status">
              {ready ? (
                muteOff ? (
                  <span className="tag">멘트 지우기 끔</span>
                ) : s.mutes.length === 0 ? (
                  <span className="faint">멘트 구간 없음</span>
                ) : unconf ? (
                  <span className="tag suggest">확인 전 {unconf}</span>
                ) : (
                  <span className="tag mute">멘트 {s.mutes.length} ✓</span>
                )
              ) : sv.state === "ready" ? (
                <span className="faint">영상 ✓ · 나누기 전</span>
              ) : sv.label ? (
                <span className={sv.state === "fetching" ? "faint" : "tag doubt"}>{sv.label}</span>
              ) : (
                <span className="faint">영상 받기 전</span>
              )}
            </span>
          ) : (
            <span className="sr-status faint">{statusText}</span>
          )}
          {!prog &&
            labels.slice(0, 3).map((l) => (
              <span key={l.text} className={`tag ${l.kind === "plain" ? "outline" : l.kind}`} data-tip={l.tip} data-tip-pos="right">
                {l.text}
              </span>
            ))}
        </div>
      </div>
    </div>
  );
}

export function SongList({ p, talk = false }: { p: Project; talk?: boolean }) {
  const ws = useWs();
  const jobs = useJobs();
  const vsAll = useVideo((x) => x.byProject[p.id]);
  const shortSec = useApp((s) => s.settings.shortSec);
  const searchRef = useRef<HTMLInputElement>(null);
  const nums = useMemo(() => songNumbers(p), [p]);
  const prog = useMemo(() => songProgress(jobs, p.id), [jobs, p]);
  const list = useMemo(() => sortSongs(p, talk ? "time" : ws.sort, ws.search), [p, ws.sort, ws.search, talk]);
  const active = list.filter((s) => s.enabled);
  const excluded = list.filter((s) => !s.enabled);

  const handled = useRef(ws.focus.n);
  useEffect(() => {
    if (ws.focus.field === "search" && ws.focus.n !== handled.current) searchRef.current?.focus();
    handled.current = ws.focus.n;
  }, [ws.focus]);

  const addAtPlayhead = () => {
    const t = player.now();
    const total = p.audio?.duration ?? p.source?.duration ?? t + 180;
    void addSong(p.id, t, Math.min(total, t + 180));
  };

  return (
    <aside className={`songlist${talk ? " compact" : ""}`}>
      <div className="sl-head">
        <span className="sl-title">곡 목록</span>
        <span className="faint">{active.length}</span>
        <span className="spacer" />
        {!talk && (
          <button className="btn xs subtle" onClick={addAtPlayhead} disabled={!p.audio} data-tip="재생 위치에서 3분 길이로 추가 (A)" data-tip-pos="left">
            <Plus size={13} /> 추가
          </button>
        )}
      </div>
      <div className="sl-tools">
        {!talk && (
          <select className="select sm" value={ws.sort} onChange={(e) => useWs.setState({ sort: e.target.value as SortMode })} aria-label="정렬">
            <option value="attention">확인 필요 순</option>
            <option value="time">방송 순서</option>
            <option value="length">길이</option>
          </select>
        )}
        <div className="search grow">
          <Search size={13} />
          <input
            ref={searchRef}
            className="input sm"
            placeholder="곡 검색 (Ctrl+F)"
            value={ws.search}
            onChange={(e) => useWs.setState({ search: e.target.value })}
            onKeyDown={(e) => {
              if (e.key === "Escape") {
                useWs.setState({ search: "" });
                (e.target as HTMLInputElement).blur();
              }
              if (e.key === "Enter" || e.key === "ArrowDown") (e.target as HTMLInputElement).blur();
            }}
          />
        </div>
      </div>
      <div className="sl-body" role="listbox" aria-label="곡 목록">
        {active.map((s) => (
          <Row key={s.id} p={p} s={s} sv={songVideo(p, vsAll, s)} n={nums.get(s.id)} selected={ws.selected === s.id} multi={ws.multi.includes(s.id) && ws.multi.length > 1} prog={prog.get(s.id)} talk={talk} shortSec={shortSec} />
        ))}
        {active.length === 0 && p.detection && <div className="sl-empty">{ws.search ? "검색 결과가 없어요." : "곡이 없어요."}</div>}
        {excluded.length > 0 && (
          <>
            <button className="sl-group" onClick={() => useWs.setState({ showExcluded: !ws.showExcluded })}>
              {ws.showExcluded ? <ChevronDown size={14} /> : <ChevronRight size={14} />} 제외됨 ({excluded.length})
              <span className="faint">· X로 복원</span>
            </button>
            {ws.showExcluded && excluded.map((s) => <Row key={s.id} p={p} s={s} sv={songVideo(p, vsAll, s)} selected={ws.selected === s.id} multi={false} talk={talk} shortSec={shortSec} />)}
          </>
        )}
      </div>
      {ws.multi.length > 1 && !talk && (
        <div className="sl-multi">
          {ws.multi.length}곡 선택 · <kbd>G</kbd> 합치기
        </div>
      )}
    </aside>
  );
}
