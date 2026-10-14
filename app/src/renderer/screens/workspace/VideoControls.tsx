// Workspace-level video controls (기획서 14.4, 14.6, 14.9, 14.10): the ⋯ project menu (margin,
// storage, 📌), [영상 맞추기], the replay / cleaned-video banners, and the automatic behaviours
// (boundary top-ups 3 s after the last edit, the selected song first, the background replay check).
import { useEffect, useState } from "react";
import { AlertTriangle, Ban, Download, HardDrive, MoreHorizontal, Pin, PinOff, RefreshCw, Ruler, WifiOff } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import { call } from "@/lib/rpc";
import { estimate, fetchVideo, len, marginOf, partsIn, prioritizeSong, songHasVideo, songsToFit, vodGone } from "@/lib/video";
import { fmtBytes, fmtDurationKo } from "@/lib/format";
import { getProject, putProject, updateProjectSettings } from "@/state/projects";
import { openSettings, toast, useApp } from "@/state/app";
import { useVideo, videoOf } from "@/state/video";
import { useWs } from "@/state/ws";

export const MARGIN_CHOICES = [0, 10, 30, 60, 90, 120];

/** Margin picker: 0 / 10 / 30 / 60 / 90 / 120 s or a typed value (0–300). */
export function MarginPicker({ value, onChange }: { value: number; onChange: (v: number) => void }) {
  const custom = !MARGIN_CHOICES.includes(value);
  const [text, setText] = useState<string | null>(null);
  return (
    <div className="margin-pick">
      <div className="seg sm">
        {MARGIN_CHOICES.map((v) => (
          <button key={v} className={value === v ? "on" : ""} onClick={() => onChange(v)}>
            {v}초
          </button>
        ))}
      </div>
      <label className={`margin-custom${custom ? " on" : ""}`}>
        <span className="faint">직접</span>
        <input
          className="input sm mono"
          value={text ?? (custom ? String(value) : "")}
          placeholder="0~300"
          onFocus={() => setText(custom ? String(value) : "")}
          onChange={(e) => setText(e.target.value.replace(/[^\d]/g, ""))}
          onBlur={() => {
            const v = Number(text);
            if (text !== null && text !== "" && v >= 0 && v <= 300 && v !== value) onChange(v);
            setText(null);
          }}
          onKeyDown={(e) => {
            e.stopPropagation();
            if (e.key === "Enter") (e.target as HTMLInputElement).blur();
          }}
        />
        <span className="faint">초</span>
      </label>
    </div>
  );
}

/** Set a project's margin (both sides, Q-S3) and offer to fill it for songs that have video. */
export async function setProjectMargin(p: Project, v: number) {
  const old = marginOf(p);
  await updateProjectSettings(p.id, { margin_before: v, margin_after: v });
  if (v <= old) {
    useWs.setState({ marginAsk: null });
    return;
  }
  const vs = videoOf(p.id);
  const ids = p.songs.filter((s) => s.enabled && songHasVideo(vs, s) && len(partsIn(vs, s.start - v, s.end + v, ["none", "failed"])) > 0.5).map((s) => s.id);
  if (!ids.length || vodGone(p)) return;
  const est = await estimate(p.id, { song_ids: ids });
  if (est && est.missing_sec > 0.5) useWs.setState({ marginAsk: { songIds: ids, sec: est.missing_sec, bytes: est.estimated_bytes } });
}

export function ProjectMenu({ p }: { p: Project }) {
  const [open, setOpen] = useState(false);
  const vs = useVideo((s) => s.byProject[p.id]);
  const margin = marginOf(p);
  const togglePin = async () => {
    try {
      putProject(await call("project.set_pinned", { id: p.id, pinned: !p.pinned }));
      toast(p.pinned ? "보관을 풀었어요. 이제 정리할 수 있어요." : "보관했어요. 정리 제안과 일괄 정리에서 빠져요.", { kind: "ok" });
    } catch {
      toast("보관 상태를 바꾸지 못했어요.", { kind: "error" });
    }
  };
  return (
    <div className="popover-anchor">
      <button className={`btn ghost sm${p.pinned ? " pinned" : ""}`} onClick={() => setOpen(!open)} aria-label="이 작업 메뉴" data-tip={open ? undefined : "앞뒤 여유 · 저장 공간 · 보관"} data-tip-pos="left">
        {p.pinned && <Pin size={13} />}
        <MoreHorizontal size={16} />
      </button>
      {open && (
        <div className="popover right project-menu" onMouseLeave={() => setOpen(false)}>
          <div className="pm-sec">
            <div className="pm-title">
              <Ruler size={13} /> 이 작업의 앞뒤 여유 <span className="faint">· 지금 {margin}초</span>
            </div>
            <MarginPicker value={margin} onChange={(v) => void setProjectMargin(p, v)} />
            <p className="faint pm-hint">곡 앞뒤로 미리 받아 두는 영상이에요. 늘리면 곡을 넓힐 때 기다리지 않아요. 앞으로 받을 영상에만 적용돼요.</p>
          </div>
          <div className="pm-sec">
            <div className="pm-title">
              <HardDrive size={13} /> 이 작업의 저장 공간
            </div>
            <p className="faint pm-hint">{vs ? (vs.local ? "녹화 파일을 그대로 써요" : `받은 영상 ${fmtDurationKo(vs.have_sec)} · ${fmtBytes(vs.bytes)}`) : "…"}</p>
            <button
              className="btn sm subtle"
              onClick={() => {
                setOpen(false);
                useApp.setState({ storageFocus: { ids: [p.id], level: null } });
                openSettings("storage");
              }}
            >
              저장 공간 보기 · 정리
            </button>
          </div>
          <div className="pm-sec">
            <button className="btn sm subtle" onClick={() => void togglePin()}>
              {p.pinned ? <PinOff size={13} /> : <Pin size={13} />} {p.pinned ? "보관 풀기" : "보관하기 (정리에서 빼기)"}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

/** [영상 맞추기 · 3곡 · 1분 20초 분량]: songs whose own range lacks video (14.4). */
export function FitVideoButton({ p }: { p: Project }) {
  const vs = useVideo((s) => s.byProject[p.id]);
  const short = songsToFit(p, vs).map((x) => ({ s: x.song, sv: x.sv }));
  if (!short.length) return null;
  const sec = short.reduce((n, x) => n + x.sv.missingSec, 0);
  return (
    <button
      className="btn sm subtle fit-video"
      onClick={() => void fetchSongsVideo(p, short.map((x) => x.s.id), { priority: "user", reason: "fit" })}
      data-tip={"곡 범위의 영상이 모자란 곡을 채워요\n받은 영상은 그대로 두고 모자란 부분만 받아요"}
      data-tip-pos="bottom"
    >
      <RefreshCw size={13} /> 영상 맞추기 · {short.length}곡 · {fmtDurationKo(sec)} 분량
    </button>
  );
}

/** Replay gone / unchecked (14.10), and "영상을 정리한 작업" with one re-fetch button (Q-S2). */
export function ReplayBanner({ p }: { p: Project }) {
  const vs = useVideo((s) => s.byProject[p.id]);
  const [est, setEst] = useState<{ ids: string[]; sec: number } | null>(null);
  const status = p.vod?.status;
  const local = p.source?.platform === "local";
  // songs that had video before (scanned for talk or exported) but have none now
  const cleanedIds = !vs || vs.local || vs.have_sec > 0 || vs.requests.length > 0 || status === "gone" ? [] : p.songs.filter((s) => s.enabled && (s.export || (p.talk_scanned ?? []).some(([a, b]) => b > s.start && a < s.end))).map((s) => s.id);
  const cleanedKey = cleanedIds.join(",");
  useEffect(() => {
    if (!cleanedIds.length) {
      setEst(null);
      return;
    }
    let alive = true;
    void estimate(p.id, { song_ids: cleanedIds }).then((r) => alive && r && setEst({ ids: cleanedIds, sec: r.missing_sec }));
    return () => {
      alive = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cleanedKey, p.id]);

  if (status === "gone") {
    const have = vs?.have_sec ?? 0;
    return (
      <div className="banner warn replay-banner">
        <Ban size={15} />
        <span className="grow">
          {local ? "녹화 파일을 찾을 수 없어요." : "이 방송의 다시보기를 더 이상 열 수 없어요(삭제, 비공개 전환, 보관 기간 끝)."}{" "}
          {have > 0 ? `받아 둔 영상 ${fmtDurationKo(have)} 안에서는 경계 조정, 멘트 다듬기, 내보내기가 모두 돼요.` : p.audio && !p.audio.dropped ? "받아 둔 소리로 곡 검수는 계속할 수 있어요." : "소리를 정리한 작업이에요. 다시보기가 없어 다시 받을 수 없어요."}
          {!local && <b> ⚠ 비공개로 바뀐 방송일 수 있어요. 올리기 전에 확인해 주세요.</b>}
        </span>
        {!p.pinned && (
          <button className="btn sm subtle" onClick={() => void call("project.set_pinned", { id: p.id, pinned: true }).then(putProject).then(() => toast("보관했어요. 정리에서 빠져요.", { kind: "ok" }))}>
            <Pin size={13} /> 보관하기
          </button>
        )}
      </div>
    );
  }
  if (est && est.sec > 0.5 && cleanedIds.length) {
    return (
      <div className="banner info replay-banner">
        <Download size={15} />
        <span className="grow">영상을 정리한 작업이에요. 곡 목록, 경계, 멘트 구간은 그대로예요.</span>
        <button className="btn sm subtle" onClick={() => void fetchVideo(p.id, { song_ids: est.ids, priority: "user", reason: "song", process: true })}>
          필요한 곡 영상 다시 받기 · {est.ids.length}곡 · 약 {fmtDurationKo(est.sec)} 분량
        </button>
      </div>
    );
  }
  if (status === "unknown" && p.vod?.code === "NETWORK") {
    return (
      <div className="replay-note faint">
        <WifiOff size={12} /> 인터넷 연결이 없어 다시보기를 확인하지 못했어요. 받은 범위 안에서는 그대로 작업할 수 있어요.
      </div>
    );
  }
  if (status === "unknown" && p.vod?.code === "AUTH_REQUIRED") {
    return (
      <div className="replay-note faint">
        <AlertTriangle size={12} /> 이 다시보기는 로그인이 필요해요. 영상을 더 받으려면 설정 &gt; 고급에서 로그인 쿠키를 넣어 주세요.
      </div>
    );
  }
  return null;
}

/** Background replay check when a project opens (never waits for it, 14.10). */
export function useVodCheck(projectId: string, ready: boolean) {
  useEffect(() => {
    if (!ready) return;
    let alive = true;
    void call("vod.check", { id: projectId })
      .then(async (v) => {
        if (!alive) return;
        const p = getProject(projectId);
        if (p && p.vod?.status !== v.status) {
          // project.changed usually follows; make sure the banner updates either way
          putProject({ ...p, vod: v });
        }
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [projectId, ready]);
}

/** The song the user looks at is fetched first (14.7 ①). */
export function usePrioritizeSelected(projectId: string, selected: string | null) {
  useEffect(() => {
    if (!selected) return;
    void prioritizeSong(projectId, videoOf(projectId), selected);
  }, [projectId, selected]);
}

/** Fetch songs' video, separating afterwards only the songs that are already separated (QA-18). */
export async function fetchSongsVideo(p: Project, ids: string[], o: { priority: "now" | "user" | "auto" | "batch"; reason: string; quiet?: boolean }) {
  const separated = (id: string) => {
    const s = p.songs.find((x) => x.id === id);
    return !!s && (p.stems ?? []).some((sp) => sp.end > s.start && sp.start < s.end);
  };
  const withStems = ids.filter(separated);
  const without = ids.filter((id) => !separated(id));
  if (withStems.length) await fetchVideo(p.id, { song_ids: withStems, ...o, process: true, quiet: o.quiet || without.length > 0 });
  if (without.length) await fetchVideo(p.id, { song_ids: without, ...o, process: false });
}
