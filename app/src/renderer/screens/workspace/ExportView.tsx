// S5 내보내기.
import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, Check, CheckCircle2, ClipboardCopy, FileAudio, FileSpreadsheet, FolderOpen, Headphones, Info, Play, Rocket, Scissors, Upload, Wand2 } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import type { ExportOptions } from "@ipc/generated/ExportOptions";
import type { ExportResult } from "@ipc/generated/ExportResult";
import type { StemsResult } from "@ipc/generated/StemsResult";
import { setMode, toast, updateSettings, useApp } from "@/state/app";
import { projectUi, setProjectUi, updateProjectSettings, useProjects } from "@/state/projects";
import { runningJobs, songProgress, startJob, useJobs } from "@/state/jobs";
import { useWs } from "@/state/ws";
import { hostClip, renderDescription, songNumbers, songTitle, unconfirmedMutes, videoState, vodLink } from "@/lib/song";
import { fromSettings, toSettings, type ExportQuality } from "@/lib/quality";
import { QualityPicker } from "@/components/QualityPicker";
import { fmtBytes, fmtEta, fmtTime } from "@/lib/format";
import { call } from "@/lib/rpc";
import { friendly } from "@/lib/errors";
import type { ExportPreset } from "../../../shared/settings";
import { Progress } from "@/components/common";

const PRESETS: { id: ExportPreset; title: string; sub: string }[] = [
  { id: "accurate", title: "유튜브 업로드용 (정확하게)", sub: "고른 화질로 다시 만들어요. 시작점이 정확해요." },
  { id: "fast", title: "빠르게 (영상 화질 그대로)", sub: "영상은 손대지 않고 잘라요. 곡당 몇 초면 끝나요." },
  { id: "archive", title: "아카이브 (멘트 그대로)", sub: "멘트를 지우지 않은 원본 소리로 정확하게 만들어요." },
  { id: "audio", title: "오디오만", sub: "WAV · FLAC · MP3 파일로 내보내요." },
  { id: "editor", title: "편집기로 넘기기", sub: "원본 · 결과 · 목소리 · 반주 WAV(같은 길이) + 마커 CSV + Reaper 프로젝트" },
];

const TOKENS = ["{date}", "{yymmdd}", "{platform}", "{vod_id}", "{channel}", "{vod_title}", "{idx}", "{title}", "{artist}", "{start}"];

function previewName(p: Project, tpl: string, songId: string | undefined): string {
  const s = p.songs.find((x) => x.id === songId);
  const src = p.source;
  const date = (src?.catalog.published_at ?? p.created_at).slice(0, 10);
  const n = s ? songNumbers(p).get(s.id) ?? 0 : 1;
  const st = s ? Math.floor(s.start) : 0;
  const vars: Record<string, string> = {
    date,
    yymmdd: date.replace(/-/g, "").slice(2),
    platform: src?.platform ?? "",
    vod_id: src?.video_id ?? "",
    channel: src?.channel ?? "",
    vod_title: src?.title ?? "",
    idx: String(n).padStart(2, "0"),
    title: s?.title || "",
    artist: s?.artist ?? "",
    start: `${Math.floor(st / 3600)}-${String(Math.floor((st % 3600) / 60)).padStart(2, "0")}-${String(st % 60).padStart(2, "0")}`,
  };
  vars.title = s?.title.trim() || vars.start; // the engine names untitled songs by their start time
  let out = tpl.replace(/\{(\w+)\}/g, (m, k: string) => vars[k] ?? m).replace(/[\\/:*?"<>|]/g, "_");
  for (const [a, b] of [["__", "_"], ["()", ""], ["[]", ""], ["  ", " "]]) while (out.includes(a)) out = out.split(a).join(b);
  return out.replace(/^[_\- ]+|[_\- ]+$/g, "") || "_";
}

export function ExportView({ p }: { p: Project }) {
  const settings = useApp((s) => s.settings);
  const jobs = useJobs();
  const ui = useProjects((s) => s.ui[p.id]) ?? projectUi(p.id);
  const songs = useMemo(() => [...p.songs].filter((s) => s.enabled).sort((a, b) => a.start - b.start), [p.songs]);
  const nums = useMemo(() => songNumbers(p), [p]);
  const local = p.source?.platform === "local";
  const hasVideoTrack = !local || p.source?.variants.some((v) => v.width > 0);
  const [preset, setPreset] = useState<ExportPreset>(settings.exportPreset);
  const [format, setFormat] = useState<"wav" | "flac" | "mp3">(settings.audioFormat);
  const [refVideo, setRefVideo] = useState(false);
  const [alsoRender, setAlsoRender] = useState(false);
  const [tpl, setTpl] = useState(p.settings.filename_template);
  const [checked, setChecked] = useState<Set<string>>(() => new Set(songs.filter((s) => (s.reviewed || !songs.some((x) => x.reviewed)) && videoState(p, s) === "ready").map((s) => s.id)));
  const [free, setFree] = useState<number | null>(null);
  const [results, setResults] = useState<{ exports: { song_id: string; result: ExportResult }[]; stems: { song_id: string; stems: StemsResult }[] } | null>(null);
  const [lastJob, setLastJob] = useState<string | null>(null);
  const outDir = p.settings.output_dir || `${useApp.getState().engine.projectsRoot ?? ""}\\${p.id}\\exports`;

  useEffect(() => setTpl(p.settings.filename_template), [p.settings.filename_template]);
  useEffect(() => {
    void window.cliper.checkFolder(outDir).then((r) => setFree(r.ok ? r.free : null));
  }, [outDir]);

  const running = runningJobs(jobs, p.id, "export.run")[0];
  const finished = lastJob ? jobs.jobs[lastJob] : undefined;
  const resultsRef = useRef<HTMLElement>(null);
  useEffect(() => {
    if (finished?.status === "done" && finished.result) {
      const r = finished.result as NonNullable<typeof results>;
      setResults(r);
      const first = r.exports[0]?.result.path ?? r.stems[0]?.stems.dir;
      toast(`${r.exports.length || r.stems.length}곡 내보내기를 마쳤어요. 원본은 그대로예요.`, { kind: "ok", action: first ? { label: "폴더 열기", run: () => void window.cliper.showItem(first) } : undefined });
      requestAnimationFrame(() => resultsRef.current?.scrollIntoView({ behavior: "smooth", block: "start" }));
    }
  }, [finished?.status, finished?.result]);
  const prog = songProgress(jobs, p.id);

  const ready = (id: string) => {
    const s = p.songs.find((x) => x.id === id)!;
    return videoState(p, s) === "ready";
  };
  const selected = songs.filter((s) => checked.has(s.id) && ready(s.id));
  const totalDur = selected.reduce((n, s) => n + (s.end - s.start), 0);
  const quality = fromSettings(p.settings);
  // 화질 applies to rendered mp4 (not to stream copy, audio-only or editor stems without a render)
  const showQuality = hasVideoTrack && (preset === "accurate" || preset === "archive" || (preset === "editor" && alsoRender));
  // smallest fetched video among the selection (engine never upscales)
  const sourceHeight = useMemo(() => {
    const hs = selected.map((s) => Number(hostClip(p, s)?.video?.resolution.split("x")[1])).filter((h) => h > 0);
    return hs.length ? Math.min(...hs) : null;
  }, [p, selected.map((s) => s.id).join()]);
  const mp4Bps = quality.videoK && preset !== "fast" ? ((quality.videoK + quality.audioK) * 1000) / 8 : (p.source?.variants[0]?.bandwidth || 8e6) / 8;
  const estBytes = preset === "audio" ? totalDur * (format === "wav" ? 384000 : format === "flac" ? 200000 : 40000) : preset === "editor" ? totalDur * 384000 * 4 + (alsoRender ? totalDur * mp4Bps : 0) : totalDur * mp4Bps;
  const estSec = preset === "fast" ? selected.length * 4 : preset === "audio" ? selected.length * 3 : preset === "editor" ? selected.length * 8 : totalDur * 0.15;
  const unconfSongs = selected.filter((s) => s.mute_enabled && unconfirmedMutes(s) > 0);
  const notListened = selected.filter((s) => s.mute_enabled && s.mutes.some((m) => m.confirmed) && !ui.listened.includes(s.id));
  const reexport = selected.filter((s) => s.export);
  const diskShort = free !== null && estBytes > free * 0.95;

  const toggle = (id: string) => {
    const n = new Set(checked);
    if (n.has(id)) n.delete(id);
    else n.add(id);
    setChecked(n);
  };

  const goConfirm = (id: string) => {
    useWs.setState({ selected: id, selectedMute: null });
    setMode("talk");
  };

  // the 화질 choice is a project default (project.update_settings); export.run waits for it
  const qualitySave = useRef<Promise<void>>(Promise.resolve());
  const setQuality = (q: ExportQuality) => {
    qualitySave.current = updateProjectSettings(p.id, toSettings(q));
  };

  const start = async () => {
    if (!selected.length || diskShort) return;
    await qualitySave.current;
    if (!ui.firstExportHintShown) setProjectUi(p.id, { firstExportHintShown: true });
    void updateSettings({ exportPreset: preset, audioFormat: format });
    if (tpl !== p.settings.filename_template) await updateProjectSettings(p.id, { filename_template: tpl });
    const ids = selected.map((s) => s.id);
    const opts: Partial<ExportOptions> = {
      song_ids: ids,
      include_unconfirmed: false,
      // 아카이브: original audio for this run only; the songs' mute_enabled stays as it is
      apply_mutes: preset === "archive" ? false : null,
      format: preset === "audio" ? format : "mp4",
      video_codec: preset === "fast" ? "copy" : null,
      stems: preset === "editor",
      also_render: preset === "editor" ? alsoRender : false,
      handles: preset === "editor" ? 3 : null,
      reference_video: preset === "editor" ? refVideo : false,
    };
    setResults(null);
    try {
      const id = await startJob("export.run", { id: p.id, ...opts }, { label: `내보내기 ${ids.length}곡` });
      setLastJob(id);
      if (id === null) toast("앞 내보내기가 끝나면 시작해요.", { kind: "info" });
    } catch (e) {
      const f = friendly(e);
      toast(`${f.title} ${f.hint ?? ""}`, { kind: "error" });
    }
  };

  useEffect(() => {
    const h = () => void start();
    window.addEventListener("cliper:export-start", h);
    return () => window.removeEventListener("cliper:export-start", h);
  });

  const copy = async (text: string, what: string) => {
    await navigator.clipboard.writeText(text);
    toast(`${what}을(를) 복사했어요.`, { kind: "ok" });
  };
  const copyTimestamps = async () => {
    const r = await call("project.export_metadata", { id: p.id, format: settings.chapterFormat ? "chapters" : "timestamps" });
    await copy(r.text, "타임스탬프 목록");
  };
  const saveMeta = async (format: "csv" | "json") => {
    const path = await window.cliper.pickSaveFile({ title: "곡 목록 저장", defaultPath: `${p.source?.video_id ?? p.id}_songs.${format}`, filters: [{ name: format.toUpperCase(), extensions: [format] }] });
    if (!path) return;
    try {
      await call("project.export_metadata", { id: p.id, format, path });
      toast("곡 목록을 저장했어요.", { kind: "ok", action: { label: "폴더 열기", run: () => void window.cliper.showItem(path) } });
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    }
  };
  const allDescriptions = () =>
    songs
      .filter((s) => s.export || checked.has(s.id))
      .map((s) => renderDescription(settings.descTemplate, p, s))
      .join("\n\n――――――――\n\n");

  const exportedSongs = songs.filter((s) => s.export);

  return (
    <div className="export">
      <div className="ex-left">
        <div className="ex-head">
          <span className="ex-title">내보낼 곡</span>
          <span className="faint">
            {selected.length}/{songs.length}곡
          </span>
          <span className="spacer" />
          <button className="btn xs ghost" onClick={() => setChecked(new Set(songs.filter((s) => ready(s.id)).map((s) => s.id)))}>
            모두 선택
          </button>
          <button className="btn xs ghost" onClick={() => setChecked(new Set())}>
            선택 해제
          </button>
        </div>
        <div className="ex-list">
          {songs.map((s) => {
            const vs = videoState(p, s);
            const unconf = unconfirmedMutes(s);
            const off = !s.mute_enabled;
            const pr = prog.get(s.id);
            const n = nums.get(s.id);
            return (
              <label key={s.id} className={`ex-row${vs !== "ready" ? " disabled" : ""}`}>
                <span className="check">
                  <input type="checkbox" checked={checked.has(s.id) && vs === "ready"} disabled={vs !== "ready"} onChange={() => toggle(s.id)} />
                </span>
                <span className="mono faint">{n ? String(n).padStart(2, "0") : "–"}</span>
                <span className={`grow ellipsis${s.title ? "" : " faint"}`}>{songTitle(s)}</span>
                <span className="mono faint">{fmtTime(s.end - s.start, 0)}</span>
                <span className="ex-state">
                  {pr ? (
                    <span className="sr-prog">
                      {pr.label}
                      {pr.fraction !== null && ` ${Math.floor(pr.fraction * 100)}%`}
                    </span>
                  ) : vs === "none" ? (
                    <span className="faint">영상 없음</span>
                  ) : vs === "refetch" ? (
                    <span className="tag doubt">다시 받기 필요</span>
                  ) : vs === "stemsMissing" ? (
                    <span className="faint">나누기 전</span>
                  ) : off ? (
                    <span className="tag">멘트 지우기 끔</span>
                  ) : unconf ? (
                    <button className="tag suggest as-btn" onClick={(e) => (e.preventDefault(), goConfirm(s.id))}>
                      <AlertTriangle size={10} /> 확인 전 {unconf}
                    </button>
                  ) : s.mutes.length ? (
                    <span className="tag mute">멘트 {s.mutes.length} ✓</span>
                  ) : (
                    <span className="faint">멘트 없음</span>
                  )}
                </span>
                {s.export && (
                  <span className="tag ok" data-tip={`${s.export.path}\n다시 내보내면 _v2로 따로 저장돼요`} data-tip-pos="left">
                    <Upload size={10} /> 내보냄
                  </span>
                )}
              </label>
            );
          })}
          {songs.some((s) => videoState(p, s) !== "ready") && <div className="hint ex-hint">영상을 받은 곡만 내보낼 수 있어요. 곡 검수 화면에서 [확인한 곡 영상 받기]를 눌러 주세요.</div>}
        </div>
      </div>

      <div className="ex-right">
        <section className="ex-card">
          <div className="ex-card-title">프리셋</div>
          <div className="presets">
            {PRESETS.filter((x) => hasVideoTrack || x.id === "audio" || x.id === "editor").map((x) => (
              <label key={x.id} className={`preset${preset === x.id ? " on" : ""}`}>
                <span className="check">
                  <input type="radio" name="preset" checked={preset === x.id} onChange={() => setPreset(x.id)} />
                </span>
                <span className="grow">
                  <b>{x.title}</b>
                  <span className="faint">{x.sub}</span>
                </span>
              </label>
            ))}
          </div>
          {preset === "fast" && (
            <div className="banner info">
              <Info size={14} /> 시작이 최대 2초 일찍 시작될 수 있어요 (영상 구조상 자를 수 있는 지점에서 잘라요).
            </div>
          )}
          {preset === "audio" && (
            <div className="row">
              <span className="label">형식</span>
              <div className="seg">
                {(["wav", "flac", "mp3"] as const).map((f) => (
                  <button key={f} className={format === f ? "on" : ""} onClick={() => setFormat(f)}>
                    {f.toUpperCase()}
                  </button>
                ))}
              </div>
            </div>
          )}
          {preset === "editor" && (
            <div className="col">
              <label className="check">
                <input type="checkbox" checked={refVideo} onChange={(e) => setRefVideo(e.target.checked)} /> 참조용 가벼운 영상(720p)도 함께
              </label>
              <label className="check">
                <input type="checkbox" checked={alsoRender} onChange={(e) => setAlsoRender(e.target.checked)} /> 완성 mp4도 함께 만들기
              </label>
              <div className="hint">앞뒤로 3초씩 여유를 붙여요. 원본 링크와 시점은 마커 CSV에 들어가요.</div>
            </div>
          )}
          {showQuality && (
            <div className="row ex-quality">
              <span className="label ex-lab">화질</span>
              <QualityPicker value={quality} onChange={setQuality} quality={p.settings.video_quality} sourceHeight={sourceHeight} />
            </div>
          )}
          <details>
            <summary>자세히</summary>
            <div className="hint">
              영상: {preset === "fast" ? "스트림 복사(무손실)" : `${p.settings.video_codec} · ${quality.height ? `${quality.height}p` : "원본 크기"} · ${quality.videoK ? `${quality.videoK}kbps (VBR)` : `품질 ${p.settings.video_quality}`}`} · 오디오 AAC {p.settings.audio_bitrate_k}kbps · 확인한 멘트 구간만 적용 · 곡 페이드 인 {p.settings.song_fade_in}초 / 아웃 {p.settings.song_fade_out}초
            </div>
          </details>
        </section>

        <section className="ex-card">
          <div className="ex-card-title">저장</div>
          <div className="row">
            <span className="label ex-lab">폴더</span>
            <input className="input sm grow" value={p.settings.output_dir ?? "작업 폴더 안 exports (기본)"} readOnly title={outDir} />
            <button
              className="btn sm subtle"
              onClick={async () => {
                const d = await window.cliper.pickFolder(p.settings.output_dir ?? undefined);
                if (d) void updateProjectSettings(p.id, { output_dir: d });
              }}
            >
              <FolderOpen size={13} /> 바꾸기
            </button>
            {p.settings.output_dir && (
              <button className="btn sm ghost" onClick={() => void updateProjectSettings(p.id, { output_dir: null })}>
                기본으로
              </button>
            )}
          </div>
          <div className="row">
            <span className="label ex-lab">파일 이름</span>
            <input className="input sm grow mono" value={tpl} onChange={(e) => setTpl(e.target.value)} onBlur={() => tpl !== p.settings.filename_template && void updateProjectSettings(p.id, { filename_template: tpl })} onKeyDown={(e) => e.stopPropagation()} />
          </div>
          <div className="tokens">
            {TOKENS.map((t) => (
              <button key={t} className="tag outline as-btn" onClick={() => setTpl(tpl + t)}>
                {t}
              </button>
            ))}
          </div>
          <div className="hint">
            예: {previewName(p, tpl, selected[0]?.id)}.{preset === "audio" ? format : "mp4"} · 같은 이름이 있으면 _v2로 따로 저장해요
          </div>
          <div className="row">
            <span className="label ex-lab">소리 크기</span>
            <div className="seg">
              <button className={!p.settings.loudness ? "on" : ""} onClick={() => void updateProjectSettings(p.id, { loudness: null })}>
                원본 유지
              </button>
              <button className={p.settings.loudness ? "on" : ""} onClick={() => void updateProjectSettings(p.id, { loudness: { target_lufs: -14, true_peak: -1 } })}>
                유튜브 기준 맞추기 (-14 LUFS)
              </button>
            </div>
          </div>
        </section>

        <section className="ex-card">
          <div className="ex-card-title">내보내기 전 점검</div>
          {unconfSongs.map((s) => (
            <div key={s.id} className="check-line warn">
              <AlertTriangle size={14} />
              <span className="grow">
                {String(nums.get(s.id) ?? "").padStart(2, "0")} {songTitle(s)}: 멘트 제안 {unconfirmedMutes(s)}개가 확인 전이에요 (적용되지 않아요)
              </span>
              <button className="btn xs subtle" onClick={() => goConfirm(s.id)}>
                확인하러 가기
              </button>
            </div>
          ))}
          {notListened.length > 0 && (
            <div className="check-line">
              <Headphones size={14} />
              <span className="grow">멘트 지운 구간을 아직 안 들어 본 곡이 {notListened.length}개 있어요.</span>
              <button className="btn xs subtle" onClick={() => goConfirm(notListened[0].id)}>
                들어 보기
              </button>
            </div>
          )}
          {!ui.firstExportHintShown && selected.some((s) => s.mutes.length) && (
            <div className="check-line info">
              <Wand2 size={14} />
              <span className="grow">처음이라면 결과 모드(W)로 한 곡을 처음부터 끝까지 들어 볼까요?</span>
              <button className="btn xs subtle" onClick={() => goConfirm(selected.find((s) => s.mutes.length)!.id)}>
                들어 보기
              </button>
            </div>
          )}
          {reexport.length > 0 && (
            <div className="check-line">
              <Info size={14} />
              <span className="grow">이미 내보낸 곡 {reexport.length}개는 기존 파일을 덮어쓰지 않고 _v2로 따로 저장해요.</span>
            </div>
          )}
          <div className="check-line">
            <Headphones size={14} />
            <span className="grow faint">이어폰이나 헤드폰으로 한 번 들어 보세요. 노트북 스피커로는 잘 안 들려요.</span>
          </div>
          {diskShort && (
            <div className="check-line err">
              <AlertTriangle size={14} />
              <span className="grow">
                저장 공간이 부족해요 (필요 약 {fmtBytes(estBytes)}, 남은 공간 {fmtBytes(free ?? 0)}). 폴더를 바꿔 주세요.
              </span>
            </div>
          )}
        </section>

        <div className="ex-go">
          <div className="grow">
            <div>
              선택 {selected.length}곡 · {fmtEta(estSec).replace(" 남음", "").replace("곧 끝나요", "몇 초")} 걸려요 · 약 {fmtBytes(estBytes)}
            </div>
            {running && (
              <div className="col" style={{ gap: 4, marginTop: 6 }}>
                <Progress value={running.fraction} />
                <span className="faint">
                  {running.itemsDone.length}/{running.items.length}곡 · {running.eta !== null ? fmtEta(running.eta) : "계산 중…"}
                </span>
              </div>
            )}
          </div>
          <button className="btn primary lg" onClick={() => void start()} disabled={!selected.length || diskShort || !!running}>
            <Rocket size={16} /> 내보내기 시작 <kbd>↵</kbd>
          </button>
        </div>

        <section className="ex-card" ref={resultsRef}>
          <div className="ex-card-title">
            결과 <span className="faint">· 원본은 바뀌지 않았어요</span>
          </div>
          {results && results.stems.length > 0 && (
            <div className="result-row">
              <Scissors size={14} />
              <span className="grow">편집기용 파일 {results.stems.length}곡</span>
              <button className="btn xs subtle" onClick={() => void window.cliper.openPath(results.stems[0].stems.dir)}>
                <FolderOpen size={12} /> 폴더
              </button>
            </div>
          )}
          {exportedSongs.length === 0 && !results && <div className="faint">아직 내보낸 곡이 없어요.</div>}
          {exportedSongs.map((s) => {
            const r = results?.exports.find((x) => x.song_id === s.id)?.result;
            const path = r?.path ?? s.export!.path;
            const name = path.split(/[\\/]/).pop();
            return (
              <div key={s.id} className="result-row">
                {r ? <CheckCircle2 size={14} color="var(--success)" /> : <Check size={14} className="faint" />}
                <span className="grow ellipsis mono" title={path}>
                  {name}
                </span>
                {r?.loudness && (
                  <span className="faint mono" data-tip={`최대 ${r.loudness.true_peak.toFixed(1)} dBTP · 범위 ${r.loudness.range.toFixed(1)} LU${r.loudness.normalized_to !== null ? `\n${r.loudness.normalized_to} LUFS로 맞춤` : ""}`} data-tip-pos="left">
                    {r.loudness.integrated.toFixed(1)} LUFS
                  </span>
                )}
                <button className="btn xs subtle" onClick={() => void window.cliper.openPath(path)}>
                  <Play size={11} /> 재생
                </button>
                <button className="btn xs subtle" onClick={() => void window.cliper.showItem(path)}>
                  <FolderOpen size={11} /> 폴더
                </button>
                <button className="btn xs subtle" onClick={() => void copy(renderDescription(settings.descTemplate, p, s), "설명란")}>
                  <ClipboardCopy size={11} /> 설명란 복사
                </button>
              </div>
            );
          })}
          <div className="row wrap" style={{ marginTop: 8 }}>
            <button className="btn sm subtle" onClick={() => void copyTimestamps()} disabled={!songs.length}>
              <ClipboardCopy size={13} /> 타임스탬프 목록 복사
            </button>
            <label className="check faint" style={{ fontSize: 12 }}>
              <input type="checkbox" checked={settings.chapterFormat} onChange={(e) => void updateSettings({ chapterFormat: e.target.checked })} /> 유튜브 챕터 형식(00:00 포함)
            </label>
            <button className="btn sm subtle" onClick={() => void copy(allDescriptions(), "모든 설명란")} disabled={!songs.length}>
              <ClipboardCopy size={13} /> 모든 설명란 복사
            </button>
            <button className="btn sm subtle" onClick={() => void window.cliper.openPath(outDir)}>
              <FolderOpen size={13} /> 출력 폴더 열기
            </button>
            <button className="btn sm ghost" onClick={() => void saveMeta("csv")}>
              <FileSpreadsheet size={13} /> CSV 저장
            </button>
            <button className="btn sm ghost" onClick={() => void saveMeta("json")}>
              <FileAudio size={13} /> JSON 저장
            </button>
          </div>
          {!local && <div className="hint">설명란에는 원본 링크({vodLink(p) ? "시점 포함" : ""})와 스트리머 채널이 들어가요. 템플릿은 설정 &gt; 내보내기에서 바꿀 수 있어요.</div>}
        </section>
      </div>
    </div>
  );
}
