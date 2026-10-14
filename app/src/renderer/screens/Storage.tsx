// 설정 > 저장 공간 (기획서 14.9): per-project usage, the four cleanup levels, 📌 keep.
// Nothing is deleted unless the user presses [정리하기]; decisions always stay.
import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, FolderOpen, HardDrive, Pin, RefreshCw, Trash2 } from "lucide-react";
import type { DropLevel } from "@ipc/generated/DropLevel";
import type { StorageUsage } from "@ipc/generated/StorageUsage";
import { ask, openSettings, toast, useApp } from "@/state/app";
import { refreshList, useProjects, putProject, getProject } from "@/state/projects";
import { call, EngineError } from "@/lib/rpc";
import { storageAll } from "@/lib/video";
import { fmtBytes, fmtRelative } from "@/lib/format";
import { friendly } from "@/lib/errors";
import { loadVideo } from "@/state/video";
import { removeLocation } from "@/lib/locations";

export const LEVELS: { id: DropLevel; label: string; deletes: string; keeps: string; again: string }[] = [
  { id: "unused", label: "곡에 안 쓰는 영상", deletes: "어떤 곡의 앞뒤 여유에도 들지 않는 영상 (제외한 곡, 직접 받아 본 범위)", keeps: "나머지 전부", again: "필요하면 다시 받아요." },
  { id: "stems", label: "나눈 소리", deletes: "목소리·반주로 나눈 소리", keeps: "영상, 소리, 모든 결정", again: "멘트 다듬기에 들어가면 다시 나눠요 (GPU 곡당 약 8초)." },
  { id: "video", label: "영상과 나눈 소리 (추천)", deletes: "받은 영상 전부 + 나눈 소리", keeps: "방송 전체 소리, 지도, 모든 결정", again: "다시보기가 있으면 같은 경계 그대로 영상을 다시 받을 수 있어요." },
  { id: "audio", label: "소리까지", deletes: "위 + 방송 전체 소리", keeps: "모든 결정 (곡 목록, 경계, 멘트 구간, 설정, 내보내기 기록)", again: "작업을 열면 [소리 다시 받기]로 다시 받을 수 있어요(약 1분, 다시보기가 있을 때). 지도와 곡 목록은 그대로예요." },
];

/** the end of a long path (the part that tells folders apart): …\scratchpad\qa2\projects */
function tail(p: string, n = 3): string {
  const parts = p.split(/[\\/]+/).filter(Boolean);
  return parts.length > n ? "…\\" + parts.slice(-n).join("\\") : p;
}

/** bytes a level would free for one project */
export function freedBy(u: StorageUsage, level: DropLevel): number {
  switch (level) {
    case "unused":
      return u.unused_video + u.cache + (u.orphan ?? 0);
    case "stems":
      return u.stems + u.cache + (u.orphan ?? 0);
    case "video":
      return u.video + u.stems + u.cache + (u.orphan ?? 0);
    case "audio":
      return u.total;
  }
}

export function StorageTab() {
  const root = useApp((s) => s.settings.projectsRoot);
  const focus = useApp((s) => s.storageFocus);
  const summaries = useProjects((s) => s.summaries);
  const [data, setData] = useState<{ projects: StorageUsage[]; free: number } | null>(null);
  const [sel, setSel] = useState<Set<string>>(new Set());
  const [level, setLevel] = useState<DropLevel>("video");
  const [busy, setBusy] = useState(false);

  const [locs, setLocs] = useState<{ roots: string[]; projects: string[] } | null>(null);
  const load = async () => {
    const r = await storageAll();
    if (r) setData({ projects: r.projects, free: r.free });
    const l = await call("projects.locations", {}).catch(() => null);
    setLocs(l ? { roots: l.roots, projects: l.projects } : null);
  };
  // one separator style, no doubled or trailing separators, case-insensitive (Windows)
  const norm = (x: string) => x.replace(/[\\/]+/g, "\\").replace(/\\$/, "").toLowerCase();
  const outside = (dir?: string) => !!dir && !!root && !norm(dir).startsWith(norm(root) + "\\");
  useEffect(() => {
    void load();
    void refreshList();
  }, []);
  // opened from the home card / ⋯ menu: preselect those projects (and the level)
  useEffect(() => {
    if (!focus) return;
    setSel(new Set(focus.ids));
    if (focus.level) setLevel(focus.level);
    useApp.setState({ storageFocus: null });
  }, [focus]);

  const rows = useMemo(() => {
    const byId = new Map(summaries.map((s) => [s.id, s]));
    return (data?.projects ?? [])
      .map((u) => ({ u, s: byId.get(u.project_id) }))
      .sort((a, b) => b.u.total - a.u.total);
  }, [data, summaries]);
  const pinned = (id: string) => rows.find((r) => r.u.project_id === id)?.u.pinned ?? false;
  const chosen = rows.filter((r) => sel.has(r.u.project_id) && !r.u.pinned);
  const freed = chosen.reduce((n, r) => n + freedBy(r.u, level), 0);
  const totalAll = rows.reduce((n, r) => n + r.u.total, 0);
  const exportsAll = rows.reduce((n, r) => n + r.u.exports, 0);
  const L = LEVELS.find((x) => x.id === level)!;

  const togglePin = async (id: string, on: boolean) => {
    try {
      const p = await call("project.set_pinned", { id, pinned: on });
      if (getProject(id)) putProject(p);
      if (on) setSel((x) => new Set([...x].filter((y) => y !== id)));
      await Promise.all([load(), refreshList()]);
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    }
  };

  const clean = async () => {
    if (!chosen.length) return;
    const goneOnes = level !== "unused" && level !== "stems" ? chosen.filter((r) => r.s?.vod?.status === "gone" && (r.u.video > 0 || level === "audio")) : [];
    const r = await ask({
      title: `${chosen.length}개 작업을 정리할까요?`,
      body: [
        `지우는 것: ${L.deletes}. 약 ${fmtBytes(freed)} 확보.`,
        `남는 것: ${L.keeps}. ${L.again}`,
        ...(goneOnes.length ? [`⚠ ${goneOnes.map((x) => x.s?.title || x.u.project_id).join(", ")}: 다시보기가 사라져서 지운 영상을 다시 받을 수 없어요.`] : []),
        "내보낸 파일은 지우지 않아요.",
      ],
      icon: "warn",
      buttons: [
        { id: "cancel", label: "취소", kind: "ghost" },
        { id: "ok", label: goneOnes.length ? "그래도 정리하기" : "정리하기", kind: "danger" },
      ],
    });
    if (r !== "ok") return;
    setBusy(true);
    let total = 0;
    const skipped: string[] = [];
    for (const row of chosen) {
      try {
        const res = await call("media.drop", { id: row.u.project_id, level });
        total += res.freed_bytes;
        if (getProject(row.u.project_id)) {
          putProject(res.project);
          void loadVideo(row.u.project_id);
        }
      } catch (e) {
        const code = e instanceof EngineError ? e.engineCode : "";
        skipped.push(`${row.s?.title || row.u.project_id}${code === "BUSY" ? " (작업 중)" : code === "VIDEO_BUSY" ? " (영상 받는 중 — 받기를 멈춘 뒤 다시)" : code === "PINNED" ? " (보관함)" : ""}`);
      }
    }
    setBusy(false);
    setSel(new Set());
    await Promise.all([load(), refreshList()]);
    toast(`${fmtBytes(total)}를 확보했어요. 곡 목록, 경계, 멘트 구간은 그대로예요.${skipped.length ? ` 건너뜀: ${skipped.join(", ")} — 받기나 작업이 끝나면 다시 해 주세요.` : ""}`, { kind: skipped.length ? "warn" : "ok", ttl: 8000 });
  };

  return (
    <div className="storage">
      <div className="st-root">
        <HardDrive size={15} />
        <span className="grow ellipsis mono" title={root ?? ""}>
          {root ?? "작업 폴더가 없어요"}
        </span>
        {data && <span className="faint">남은 공간 {fmtBytes(data.free)}</span>}
        <button className="btn xs ghost" onClick={() => openSettings("general")}>
          바꾸기
        </button>
        <button className="btn xs ghost" onClick={() => root && void window.cliper.openPath(root)}>
          <FolderOpen size={12} /> 폴더 열기
        </button>
        <button className="btn icon xs ghost" onClick={() => void load()} aria-label="다시 계산">
          <RefreshCw size={12} />
        </button>
      </div>
      <div className="faint st-total">
        작업 파일 합계 {fmtBytes(totalAll)}
        {exportsAll > 0 && ` · 작업 폴더 안의 내보낸 파일 ${fmtBytes(exportsAll)}은 정리하지 않아요`}
      </div>

      <div className="st-table" role="table">
        <div className="st-row st-headrow" role="row">
          <span />
          <span>작업</span>
          <span>소리</span>
          <span>영상</span>
          <span>나눈 소리</span>
          <span>합계</span>
        </div>
        {!data && <div className="faint st-empty">계산 중…</div>}
        {data && rows.length === 0 && <div className="faint st-empty">작업이 없어요.</div>}
        {rows.map(({ u, s }) => {
          const id = u.project_id;
          const gone = s?.vod?.status === "gone";
          return (
            <div key={id} className={`st-row${u.pinned ? " pinned" : ""}${sel.has(id) ? " sel" : ""}`} role="row">
              <span className="check">
                <input
                  type="checkbox"
                  disabled={u.pinned}
                  checked={sel.has(id) && !u.pinned}
                  onChange={(e) => setSel((x) => (e.target.checked ? new Set([...x, id]) : new Set([...x].filter((y) => y !== id))))}
                  aria-label="정리할 작업으로 고르기"
                  data-tip={u.pinned ? "보관한 작업은 정리할 수 없어요. 보관을 먼저 풀어 주세요." : undefined}
                />
              </span>
              <span className="st-name">
                <button className={`btn icon xs ghost st-pin${u.pinned ? " on" : ""}`} onClick={() => void togglePin(id, !pinned(id))} aria-label={u.pinned ? "보관 풀기" : "보관하기"} data-tip={u.pinned ? "보관 풀기" : "📌 보관: 정리 제안과 일괄 정리에서 빼요"} data-tip-pos="right">
                  <Pin size={12} />
                </button>
                <span className="grow ellipsis">
                  {s?.title || id}
                  <span className="faint">
                    {" "}
                    · {s?.exported ? `내보냄 ${s.exported} · ` : ""}
                    {fmtRelative(u.updated_at)}
                  </span>
                  {gone && <span className="tag doubt st-tag">다시보기 없음</span>}
                </span>
                {u.unused_video > 1e6 && <span className="faint st-unused">곡에 안 쓰는 영상 {fmtBytes(u.unused_video)}</span>}
                {(u.orphan ?? 0) > 1e6 && <span className="faint st-unused" data-tip="중간에 멈춘 받기·나누기가 남긴 파일이에요. 어떤 정리 단계에서도 지워져요." data-tip-pos="right">남은 임시 파일 {fmtBytes(u.orphan)}</span>}
                {outside(s?.dir) && (
                  <span className="faint st-unused mono ellipsis" title={s?.dir}>
                    위치: {tail(s?.dir ?? "")}
                  </span>
                )}
              </span>
              <span className="mono">{fmtBytes(u.audio)}</span>
              <span className="mono">{fmtBytes(u.video)}</span>
              <span className="mono">{fmtBytes(u.stems)}</span>
              <span className="mono">{fmtBytes(u.total)}</span>
            </div>
          );
        })}
      </div>

      <div className="st-go">
        <span>
          선택 {chosen.length}개 · 정리
        </span>
        <select className="select sm" value={level} onChange={(e) => setLevel(e.target.value as DropLevel)}>
          {LEVELS.map((x) => (
            <option key={x.id} value={x.id}>
              {x.label}
            </option>
          ))}
        </select>
        <span className="grow">→ 약 {fmtBytes(freed)} 확보</span>
        <button className="btn sm danger" disabled={!chosen.length || busy} onClick={() => void clean()}>
          <Trash2 size={13} /> {busy ? "정리하는 중…" : "정리하기"}
        </button>
      </div>
      <div className="st-explain">
        <div>
          <b>지우는 것</b> {L.deletes}
        </div>
        <div>
          <b>남는 것</b> {L.keeps}. 내보낸 파일은 어떤 정리에서도 지우지 않아요.
        </div>
        <div>
          <b>다시 쓰려면</b> {L.again}
        </div>
        {chosen.some((r) => r.s?.vod?.status === "gone") && level !== "unused" && level !== "stems" && (
          <div className="err">
            <AlertTriangle size={12} /> 다시보기가 사라진 작업이 있어요. 그 작업의 영상은 지우면 다시 받을 수 없어요.
          </div>
        )}
        <div className="faint">앱은 정리를 제안만 하고, 스스로 지우지 않아요.</div>
      </div>

      {locs && (locs.roots.length > 0 || locs.projects.length > 0) && (
        <div className="st-locs">
          <div className="st-locs-title">다른 위치 · 목록에 함께 보이는 곳</div>
          {[...locs.roots.map((p) => ({ p, kind: "이전 작업 폴더" })), ...locs.projects.map((p) => ({ p, kind: "작업 하나" }))].map(({ p, kind }) => (
            <div key={p} className="st-loc">
              <span className="tag outline">{kind}</span>
              <span className="grow ellipsis mono" title={p}>
                {tail(p)}
              </span>
              <button className="btn xs ghost" onClick={() => void window.cliper.openPath(p)}>
                <FolderOpen size={12} /> 열기
              </button>
              <button className="btn xs ghost" onClick={() => void removeLocation(p).then(load)} data-tip="목록에서만 빼요. 폴더와 파일은 그대로예요." data-tip-pos="left">
                목록에서 빼기
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}
