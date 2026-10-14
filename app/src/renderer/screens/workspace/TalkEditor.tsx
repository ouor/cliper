// S4 멘트 다듬기.
import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Check, Download, Hand, Headphones, Info, Plus, Repeat, Scissors, SkipForward, Trash2, Wand2, Zap } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import type { Song } from "@ipc/generated/Song";
import type { Mute } from "@ipc/generated/Mute";
import { useWs } from "@/state/ws";
import { ask, useApp } from "@/state/app";
import { projectUi, setProjectUi, editSong } from "@/state/projects";
import { runningJobs, songProgress, startJob, useJobs } from "@/state/jobs";
import { useVideo } from "@/state/video";
import { player, usePlayer, LISTEN_KEY, LISTEN_LABEL, type ListenMode } from "@/audio/player";
import { mutePosition, previewMutes, singOverlap, songNumbers, songTitle, unconfirmedMutes } from "@/lib/song";
import { marginOf, partsIn, songVideo, spanFor, unseparated, vodGone } from "@/lib/video";
import { loadPeaks, mediaUrl, type Peaks } from "@/lib/peaks";
import { fmtTime } from "@/lib/format";
import { fetchSongs } from "@/lib/pipeline";
import { addMute, deleteMute, deleteSuggestions, endSongHere, isSuggestion, splitMute, updateMute } from "@/lib/edits";
import { DetailTimeline, selectMute } from "./DetailTimeline";
import { useMapData } from "./OverviewMap";
import { Progress } from "@/components/common";
import { VideoPane } from "@/components/VideoPane";

/** stems.ensure asked once per song and unseparated state (no loop when it can't separate) */
const askedEnsure = new Set<string>();

const MODE_TIP = [
  "방송 그대로 (기준)",
  "내보낼 소리 · 확인한 멘트 구간만 목소리 제거\n확인 전 제안은 목소리가 그대로 들려요 (P로 적용해 듣기)",
  "결과에서 빠지는 소리만 (확인한 구간, 구간 밖은 무음)\n여기서 노래 멜로디가 들리면 과하게 지운 거예요",
  "분리된 반주 전체 (참고용)",
];

/** accompaniment below this (dBFS) makes removal artefacts audible */
const QUIET_DB = -36;

const STRENGTHS: { label: string; v: number | null }[] = [
  { label: "완전 제거", v: null },
  { label: "-30dB", v: -30 },
  { label: "-24dB", v: -24 },
  { label: "-18dB", v: -18 },
  { label: "-12dB", v: -12 },
  { label: "-6dB", v: -6 },
];

function MsField({ value, onCommit, label }: { value: number; onCommit: (v: number) => void; label: string }) {
  const [text, setText] = useState<string | null>(null);
  return (
    <label className="ms-field">
      <span className="faint">{label}</span>
      <input
        className="input sm mono"
        value={text ?? String(Math.round(value * 1000))}
        onFocus={(e) => {
          setText(String(Math.round(value * 1000)));
          requestAnimationFrame(() => e.target.select());
        }}
        onChange={(e) => setText(e.target.value.replace(/[^\d]/g, ""))}
        onBlur={() => {
          const v = Number(text);
          if (text !== null && v >= 5 && v <= 2000 && Math.abs(v / 1000 - value) > 0.0005) onCommit(v / 1000);
          setText(null);
        }}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          if (e.key === "Escape") {
            setText(null);
            requestAnimationFrame(() => (e.target as HTMLInputElement).blur());
          }
        }}
      />
      <span className="faint">ms</span>
    </label>
  );
}

/** Song fade (seconds, null = project default) edited in seconds with one decimal. */
function SongFadeField({ label, value, fallback, onCommit }: { label: string; value: number | null; fallback: number; onCommit: (v: number | null) => void }) {
  const [text, setText] = useState<string | null>(null);
  return (
    <label className="ms-field" data-tip={`비우면 작업 기본값 (${fallback}초)`} data-tip-pos="top">
      <span className="faint">{label}</span>
      <input
        className="input sm mono"
        placeholder={String(fallback)}
        value={text ?? (value === null ? "" : String(value))}
        onFocus={() => setText(value === null ? "" : String(value))}
        onChange={(e) => setText(e.target.value.replace(/[^\d.]/g, ""))}
        onBlur={() => {
          if (text !== null) {
            const v = text.trim() === "" ? null : Math.max(0.01, Math.min(5, Number(text)));
            if (v === null || Number.isFinite(v)) if (v !== value) onCommit(v);
          }
          setText(null);
        }}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter") (e.target as HTMLInputElement).blur();
        }}
      />
      <span className="faint">초</span>
    </label>
  );
}

function MuteRow({ p, s, m, selected, warn }: { p: Project; s: Song; m: Mute; selected: boolean; warn: string[] }) {
  return (
    <button className={`mute-row${selected ? " selected" : ""}${m.confirmed ? " confirmed" : " suggested"}`} onClick={() => selectMute(m, usePlayer.getState().playing)}>
      <span className={`mute-dot${m.confirmed ? " on" : ""}`} />
      <span className="mr-pos">{mutePosition(m, s)}</span>
      <span className="mono mr-time">
        {fmtTime(m.start - s.start, 1)} ~ {fmtTime(m.end - s.start, 1)}
      </span>
      <span className="spacer" />
      {m.origin === "manual" && <Hand size={12} className="faint" aria-label="직접 만듦" />}
      {m.gain_db !== null && <span className="tag outline">{m.gain_db}dB</span>}
      {warn
        .filter((w) => w === "노래 겹침" || w === "조용한 반주" || w === "받은 영상 밖")
        .map((w) => (
          <span key={w} className="tag doubt">
            <AlertTriangle size={10} /> {w}
          </span>
        ))}
      <span className={`tag ${m.confirmed ? "mute" : "suggest"}`}>{m.confirmed ? (m.origin === "manual" ? "직접 만듦" : "확인함") : "제안 · 확인 전"}</span>
    </button>
  );
}

export function TalkEditor({ p }: { p: Project }) {
  const ws = useWs();
  const settings = useApp((s) => s.settings);
  const mode = usePlayer((s) => s.mode);
  const loopOn = usePlayer((s) => s.loopOn);
  const stems = usePlayer((s) => s.stems);
  const playing = usePlayer((s) => s.playing);
  const jobs = useJobs();
  const { probs } = useMapData(p);
  const vs = useVideo((x) => x.byProject[p.id]);
  const s = p.songs.find((x) => x.id === ws.selected);
  const total = p.audio?.duration || p.source?.duration || 0;
  const margin = marginOf(p);
  // the editor's range: song ± margin (14.6); stems follow the separated spans (14.8)
  const lo = s ? Math.max(0, s.start - margin) : 0;
  const hi = s ? (total > 0 ? Math.min(total, s.end + margin) : s.end + margin) : 0;
  const span = s ? spanFor(p, s.start, s.end) : null;
  const sv = s ? songVideo(p, vs, s) : null;
  const unsep = s ? unseparated(p, vs, lo, hi) : [];
  const unsepKey = unsep.map(([a, b]) => `${a.toFixed(1)}-${b.toFixed(1)}`).join(",");
  const separating = runningJobs(jobs, p.id).some((j) => j.kind === "stems.ensure" || j.kind === "clips.fetch") || jobs.waiting.some((w) => w.projectId === p.id && w.method === "stems.ensure");
  const muteOn = s ? s.mute_enabled : true;
  const [peaks, setPeaks] = useState<{ orig: Peaks | null; voc: Peaks | null }>({ orig: null, voc: null });
  const [notesOpen, setNotesOpen] = useState(false);

  // video arrived but isn't separated (new song, widened boundary, cleaned stems): separate it
  // now — only the new parts (stems.ensure, 14.8); once per state so a failure doesn't loop
  useEffect(() => {
    if (!s || !s.enabled || !unsep.length || separating) return;
    const key = `${p.id}|${s.id}|${unsepKey}`;
    if (askedEnsure.has(key)) return;
    askedEnsure.add(key);
    void startJob("stems.ensure", { id: p.id, song_ids: [s.id] }, { label: "목소리 나누기" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [p.id, s?.id, unsepKey, separating]);

  // pick a song with video when entering with nothing useful selected
  useEffect(() => {
    if (s && s.enabled) return;
    const withVideo = [...p.songs].filter((x) => x.enabled && spanFor(p, x.start, x.end)).sort((a, b) => a.start - b.start);
    const first = withVideo.find((x) => unconfirmedMutes(x) > 0) ?? withVideo[0];
    if (first) useWs.setState({ selected: first.id, selectedMute: null });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // first visit: open the "할 수 없는 것" notes once (coach mark)
  useEffect(() => {
    void window.cliper.getState<boolean>("talkCoachSeen").then((seen) => {
      if (!seen) {
        setNotesOpen(true);
        void window.cliper.setState("talkCoachSeen", true);
      }
    });
  }, []);

  // stems window + peaks
  const audioOk = !!p.audio && !p.audio.dropped;
  useEffect(() => {
    if (!s || !span) return;
    // song ± margins (+ loop padding) of the span; outside the span the original plays (fallback)
    const pad = useApp.getState().settings.loopPad + 2;
    const a = Math.max(0, lo - pad);
    const b = hi + pad;
    void player.useStems(p.id, span.dir, span.start, span.frames, a, b, span.id, audioOk ? total : 0);
    const now = player.now();
    if (now < a || now > b) player.seek(Math.max(a, s.start - 2));
    let alive = true;
    void Promise.all([loadPeaks(mediaUrl(p.id, `${span.dir}/orig.peaks`, span.id)).catch(() => null), loadPeaks(mediaUrl(p.id, `${span.dir}/vocals.peaks`, span.id)).catch(() => null)]).then(([orig, voc]) => alive && setPeaks({ orig, voc }));
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [s?.id, lo, hi, span?.id, audioOk]);

  // mutes → worklet: what the export applies (confirmed only) + the auditioned suggestion (#4)
  useEffect(() => {
    if (s) player.setMutes(previewMutes(s, ws.selectedMute, ws.audition), muteOn);
  }, [s, muteOn, ws.selectedMute, ws.audition]);

  // "listened" (export pre-check): result mode played for a while on this song
  const listenAcc = useRef(0);
  useEffect(() => {
    listenAcc.current = 0;
  }, [s?.id]);
  useEffect(() => {
    if (!s || !playing || mode !== 1) return;
    const t = setInterval(() => {
      listenAcc.current += 1;
      if (listenAcc.current >= 3 && !projectUi(p.id).listened.includes(s.id)) setProjectUi(p.id, { listened: [...projectUi(p.id).listened, s.id] });
    }, 1000);
    return () => clearInterval(t);
  }, [s?.id, playing, mode, p.id, s]);

  // 조용한 반주: exact accompaniment level inside each mute (computed in the worklet once loaded)
  const [quiet, setQuiet] = useState<Set<string>>(new Set());
  const stemsReady = !!stems && stems.progress >= 1 && !stems.error && stems.dir === span?.dir;
  const muteKey = s ? s.mutes.map((m) => `${m.id}:${m.start.toFixed(2)}:${m.end.toFixed(2)}`).join("|") : "";
  useEffect(() => {
    if (!s || !stemsReady) return;
    let alive = true;
    const ms = s.mutes;
    void player.accompanimentRms(ms.map((m) => [m.start, m.end])).then((db) => {
      if (!alive) return;
      setQuiet(new Set(ms.filter((_, i) => db[i] !== null && (db[i] as number) < QUIET_DB).map((m) => m.id)));
    });
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [muteKey, stemsReady]);

  const warnings = useMemo(() => {
    const out = new Map<string, string[]>();
    if (!s) return out;
    for (const m of s.mutes) {
      const w: string[] = [];
      if (singOverlap(m, probs) >= 0.3) w.push("노래 겹침");
      if (quiet.has(m.id) && m.start < s.end && m.end > s.start) w.push("조용한 반주");
      // inside the song (what is exported) but without video on disk
      if (partsIn(vs, Math.max(m.start, s.start), Math.min(m.end, s.end)).length) w.push("받은 영상 밖");
      if (m.end <= s.start || m.start >= s.end) w.push("곡 밖");
      out.set(m.id, w);
    }
    return out;
  }, [s, probs, vs, quiet]);

  if (!s) {
    return (
      <div className="talk-empty">
        <p>멘트를 다듬을 곡을 목록에서 골라 주세요.</p>
        <p className="faint">영상을 받은 곡만 멘트 다듬기를 할 수 있어요.</p>
      </div>
    );
  }
  const prog = songProgress(jobs, p.id).get(s.id);
  const n = songNumbers(p).get(s.id);
  if (!span) {
    const fetching = sv?.pending;
    return (
      <div className="talk-empty">
        <div className="te-title">
          {n ? String(n).padStart(2, "0") + " " : ""}
          {songTitle(s)}
        </div>
        {prog || (unsep.length > 0 && separating) ? (
          <div className="te-progress">
            <div className="row">
              <Download size={15} /> {prog?.label ?? "목소리와 반주 나누는 중"}
              {prog?.fraction != null ? ` · ${Math.floor(prog.fraction * 100)}%` : ""}
            </div>
            <Progress value={prog?.fraction ?? null} />
            <div className="faint">{prog?.waiting ? "앞 작업이 끝나면 시작해요. 다른 곡을 먼저 확인해도 돼요." : "목소리와 반주를 나누는 데 GPU에서 5분 곡 약 8초 걸려요. 다른 곡을 먼저 확인해도 돼요."}</div>
          </div>
        ) : fetching ? (
          <div className="te-progress">
            <div className="row">
              <Download size={15} /> {sv?.label}
            </div>
            <Progress value={sv?.fraction ?? null} />
            <div className="faint">영상을 받으면 목소리와 반주를 나누고 멘트를 찾아요. 다른 곡을 먼저 확인해도 돼요.</div>
          </div>
        ) : unsep.length > 0 ? (
          <div className="te-progress">
            <div className="row">
              <Download size={15} /> 목소리와 반주를 나눌 준비 중이에요
            </div>
            <Progress value={null} />
          </div>
        ) : !s.enabled ? (
          <p className="muted">제외한 곡이에요. 곡 검수 화면에서 복원할 수 있어요.</p>
        ) : vodGone(p) ? (
          <p className="muted">다시보기를 더 이상 열 수 없어서 이 곡의 영상을 받을 수 없어요. 받아 둔 영상이 있는 곡은 계속 다듬을 수 있어요.</p>
        ) : (
          <>
            <p>이 곡의 영상을 아직 받지 않았어요. 멘트 다듬기는 영상을 받은 뒤에 할 수 있어요.</p>
            <button className="btn primary" onClick={() => void fetchSongs(p.id, [s.id])}>
              <Zap size={14} /> 이 곡 받기 <kbd>Ctrl+Shift+E</kbd>
            </button>
            <p className="faint">영상 받기 → 목소리와 반주 나누기 → 멘트 제안을 한 번에 해요. 1080p 5분 곡 기준 약 15~20초.</p>
          </>
        )}
      </div>
    );
  }

  const mutes = [...s.mutes].sort((a, b) => a.start - b.start);
  const sel = mutes.find((m) => m.id === ws.selectedMute) ?? null;
  const unconf = unconfirmedMutes(s);
  // 확인 전 제안 across all (enabled) songs, for "모든 곡의 제안 지우기" (#2)
  const suggestSongs = p.songs.filter((x) => x.enabled && x.mutes.some(isSuggestion));
  const allSuggest = suggestSongs.reduce((n, x) => n + x.mutes.filter(isSuggestion).length, 0);
  const deleteAllSuggestions = async () => {
    const r = await ask({
      title: "모든 곡의 확인 전 제안을 지울까요?",
      body: [`곡 ${suggestSongs.length}개에서 확인 전 제안 ${allSuggest}개를 지워요. 확인한 구간과 직접 만든 구간은 그대로예요.`, "Ctrl+Z 한 번으로 모두 되돌릴 수 있어요."],
      icon: "warn",
      buttons: [
        { id: "cancel", label: "취소", kind: "ghost" },
        { id: "delete", label: `제안 ${allSuggest}개 지우기`, kind: "danger" },
      ],
    });
    if (r === "delete") void deleteSuggestions(p.id, suggestSongs.map((x) => x.id), "모든 곡의 제안 지우기");
  };
  const selWarn = sel ? warnings.get(sel.id) ?? [] : [];
  const loadingStems = stems && stems.dir === span.dir && stems.progress < 1 && !stems.error;

  return (
    <div className="talk">
      <div className="talk-top">
        <div className="talk-video">
          <VideoPane p={p}>
            {loadingStems && (
              <div className="video-loading">
                <span className="spinner" /> 소리 불러오는 중 {Math.floor((stems?.progress ?? 0) * 100)}%
              </div>
            )}
            {stems?.error && <div className="video-loading err">소리를 불러오지 못했어요 · {stems.error}</div>}
          </VideoPane>
        </div>
        <div className="mute-panel">
          <div className="mp-head">
            <div className="grow">
              <div className="mp-song ellipsis">
                <span className="mono faint">{n ? String(n).padStart(2, "0") : "–"}</span> {songTitle(s)}
              </div>
              <div className="faint mp-sub">
                멘트 구간 {mutes.length}개{unconf ? ` · 확인 전 ${unconf}개는 아직 적용 안 됨` : mutes.length ? " · 모두 확인함" : ""}
              </div>
            </div>
            <button
              className="btn xs subtle"
              onClick={() => {
                const t = player.now();
                void addMute(p.id, s.id, t, t + 2);
              }}
              data-tip="재생 위치에 2초 구간 추가 (또는 목소리 줄을 드래그)"
              data-tip-pos="left"
            >
              <Plus size={13} />
            </button>
          </div>
          <label className="mute-toggle">
            <input type="checkbox" className="switch" checked={muteOn} onChange={(e) => {
                const on = e.target.checked;
                void editSong(p.id, s.id, on ? "멘트 지우기 켜기" : "멘트 지우기 끄기", (x) => void (x.mute_enabled = on));
              }} />
            <span className="grow">멘트 지우기 사용</span>
            <span className="faint">{muteOn ? "확인한 구간만 내보내요" : "원본 소리로 내보내요"}</span>
          </label>
          <div className="song-fades">
            <span className="faint">곡 페이드</span>
            <SongFadeField label="인" value={s.fade_in} fallback={p.settings.song_fade_in} onCommit={(v) => void editSong(p.id, s.id, "곡 페이드 인", (x) => void (x.fade_in = v))} />
            <SongFadeField label="아웃" value={s.fade_out} fallback={p.settings.song_fade_out} onCommit={(v) => void editSong(p.id, s.id, "곡 페이드 아웃", (x) => void (x.fade_out = v))} />
          </div>
          <div className="mp-list">
            {mutes.length === 0 && <div className="mp-empty">멘트가 겹친 부분을 찾지 못했어요. 필요하면 목소리 줄을 드래그해서 직접 추가하세요.</div>}
            {mutes.map((m) => (
              <MuteRow key={m.id} p={p} s={s} m={m} selected={m.id === ws.selectedMute} warn={warnings.get(m.id) ?? []} />
            ))}
          </div>
          {(unconf > 1 || allSuggest > unconf) && (
            <div className="mp-bulk">
              {unconf > 1 && (
                <div className="mp-bulk-row">
                  <button
                    className="btn sm subtle"
                    onClick={() =>
                      void editSong(p.id, s.id, "제안 모두 확정", (x) => {
                        for (const m of x.mutes) m.confirmed = true;
                      })
                    }
                  >
                    <Check size={13} /> 제안 {unconf}개 모두 확정
                  </button>
                  <button className="btn sm subtle" onClick={() => void deleteSuggestions(p.id, [s.id])} data-tip={"이 곡의 확인 전 제안만 지워요\n확인한 구간·직접 만든 구간은 그대로, Ctrl+Z로 되돌려요"} data-tip-pos="top">
                    <Trash2 size={13} /> 제안 {unconf}개 지우기
                  </button>
                </div>
              )}
              {allSuggest > unconf && (
                <button className="btn xs ghost mp-bulk-all" onClick={() => void deleteAllSuggestions()}>
                  <Trash2 size={12} /> 모든 곡의 확인 전 제안 지우기 · {suggestSongs.length}곡 {allSuggest}개
                </button>
              )}
            </div>
          )}
        </div>
      </div>

      <div className="listen-bar">
        <span className="label">듣기</span>
        <div className="seg listen-seg">
          {LISTEN_LABEL.map((l, i) => (
            <button key={l} className={`${mode === i ? "on" : ""} m${i}`} onClick={() => player.setMode(i as ListenMode)} data-tip={MODE_TIP[i]} data-tip-pos="top">
              <kbd>{LISTEN_KEY[i]}</kbd> {l}
            </button>
          ))}
        </div>
        <button className={`btn sm ${loopOn ? "active" : "ghost"}`} onClick={() => player.toggleLoop()} aria-pressed={loopOn} data-tip={`선택한 구간 앞 ${settings.loopPad}초 ~ 뒤 ${settings.loopPad}초를 반복해요
켜고 끄는 건 이 버튼으로만 바뀌어요`} data-tip-pos="top">
          <Repeat size={13} /> 구간 반복 {loopOn ? "켬" : "끔"} <kbd>Ctrl+L</kbd>
        </button>
        <span className="spacer" />
        <span className="faint lb-hint">
          <kbd>Tab</kbd> 다음 구간 · 목소리 줄 드래그 = 새 구간 · <kbd>Alt</kbd> 스냅 끄기 · <kbd>Ctrl</kbd>+휠 확대
        </span>
      </div>

      <DetailTimeline p={p} s={s} bounds={[lo, hi]} origin={span.start} orig={peaks.orig} voc={peaks.voc} probs={probs} muteOn={muteOn} />

      <div className="mute-inspector">
        {sel ? (
          <>
            <div className="mi-row">
              <span className="mi-name">
                선택: {mutePosition(sel, s)} <span className="mono">{fmtTime(sel.start - s.start, 2)} ~ {fmtTime(sel.end - s.start, 2)}</span>
                <span className="faint"> ({(sel.end - sel.start).toFixed(2)}초)</span>
              </span>
              <MsField label="페이드 앞" value={sel.fade_in} onCommit={(v) => void updateMute(p.id, s.id, sel.id, "페이드 길이", (m) => void (m.fade_in = v))} />
              <MsField label="뒤" value={sel.fade_out} onCommit={(v) => void updateMute(p.id, s.id, sel.id, "페이드 길이", (m) => void (m.fade_out = v))} />
              <label className="ms-field">
                <span className="faint">강도</span>
                <select
                  className="select sm"
                  value={sel.gain_db === null ? "" : String(sel.gain_db)}
                  onChange={(e) => {
                    const v = e.target.value === "" ? null : Number(e.target.value);
                    void updateMute(p.id, s.id, sel.id, "제거 강도", (m) => void (m.gain_db = v));
                  }}
                >
                  {STRENGTHS.map((x) => (
                    <option key={x.label} value={x.v === null ? "" : String(x.v)}>
                      {x.v === null ? "완전 제거" : `줄이기 ${x.label}`}
                    </option>
                  ))}
                  {sel.gain_db !== null && !STRENGTHS.some((x) => x.v === sel.gain_db) && <option value={String(sel.gain_db)}>{sel.gain_db}dB</option>}
                </select>
              </label>
              <span className="spacer" />
              {!sel.confirmed && (
                <button
                  className={`btn sm ${ws.audition ? "active" : "subtle"}`}
                  aria-pressed={ws.audition}
                  onClick={() => useWs.setState({ audition: !ws.audition })}
                  data-tip={ws.audition ? "이 제안을 결과에 적용해서 들려주는 중이에요\n확정한 건 아니에요. 다시 누르면 제안 없이 들어요" : "확정하지 않고 이 제안을 결과에 적용해 들어 봐요\n고른 제안 하나에만 적용돼요"}
                  data-tip-pos="top"
                >
                  <Headphones size={13} /> 적용해 듣기 {ws.audition ? "켬" : "끔"} <kbd>P</kbd>
                </button>
              )}
              {!sel.confirmed ? (
                <button className="btn sm primary" onClick={() => void updateMute(p.id, s.id, sel.id, "적용 확정", (m) => void (m.confirmed = true))}>
                  <Check size={13} /> 적용 확정 <kbd>↵</kbd>
                </button>
              ) : (
                <button className="btn sm subtle" onClick={() => void updateMute(p.id, s.id, sel.id, "확정 취소", (m) => void (m.confirmed = false))} disabled={sel.origin === "manual"} data-tip={sel.origin === "manual" ? "직접 만든 구간은 항상 확정이에요" : "확인 전으로 되돌려요"} data-tip-pos="top">
                  확정 취소
                </button>
              )}
              <button className="btn sm subtle" onClick={() => void deleteMute(p.id, s.id, sel.id)}>
                <Trash2 size={13} /> 삭제 <kbd>Del</kbd>
              </button>
              <button className="btn sm subtle" onClick={() => void splitMute(p.id, s.id, sel.id, player.now())}>
                <Scissors size={13} /> 나누기 <kbd>/</kbd>
              </button>
              <button className="btn sm subtle" onClick={() => void endSongHere(p.id, s.id, sel.id)} data-tip={"곡 끝을 이 구간 시작으로 옮기고\n페이드 아웃해요 (되돌리기 가능)"} data-tip-pos="top">
                <SkipForward size={13} /> 여기서 곡 끝내기 <kbd>F</kbd>
              </button>
            </div>
            {selWarn.includes("노래 겹침") && (
              <div className="banner warn mi-warn">
                <AlertTriangle size={14} />
                <span>이 구간에는 노래가 일부 포함되어 있어요. 노래도 함께 빠질 수 있어요. <b>E 지운 목소리</b>로 들어 보세요.</span>
              </div>
            )}
            {selWarn.includes("조용한 반주") && (
              <div className="banner warn mi-warn">
                <AlertTriangle size={14} />
                <span>이 구간은 반주가 조용해요. 지운 흔적이 들릴 수 있어요. 구간을 짧게 하거나 페이드를 길게, 또는 강도를 '줄이기'로 바꿔 보세요.</span>
              </div>
            )}
            {selWarn.includes("받은 영상 밖") && (
              <div className="banner warn mi-warn">
                <AlertTriangle size={14} />
                <span>구간이 받은 영상 범위를 벗어났어요. 내보낼 때 문제가 될 수 있어요.</span>
              </div>
            )}
            {selWarn.includes("곡 밖") && <div className="hint">곡 범위 밖의 구간은 내보내는 파일에 들어가지 않아요.</div>}
          </>
        ) : (
          <div className="mi-row faint">
            <Wand2 size={14} /> 구간을 고르면 앞 {settings.loopPad}초부터 들려줘요. 구간 반복을 켜 두면 뒤 {settings.loopPad}초까지 반복해요. <kbd>Tab</kbd>으로 다음 구간, <kbd>B</kbd>로 재생 위치에 새 구간을 시작해요.
          </div>
        )}
        <details className="honest" open={notesOpen} onToggle={(e) => setNotesOpen((e.target as HTMLDetailsElement).open)}>
          <summary>
            <Info size={11} /> 멘트 지우기로 할 수 없는 것
          </summary>
          <ul>
            <li>후원 알림음 같은 효과음은 반주로 남아요. 지울 수 없어요.</li>
            <li>노래와 말이 겹친 부분은 노래도 함께 빠져요.</li>
            <li>반주가 조용한 부분(피아노만 있는 전주 등)에서는 지운 흔적이 들릴 수 있어요. 이럴 땐 구간을 짧게 하거나 페이드를 길게, 또는 강도를 '줄이기'로 바꿔 보세요.</li>
          </ul>
        </details>
      </div>
    </div>
  );
}
