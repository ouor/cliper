import { useState } from "react";
import { RefreshCw } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import { call } from "@/lib/rpc";
import { SENSITIVITY_LABEL, sensitivityFromParams, sensitivityParams } from "@/lib/song";
import { toast, updateSettings, useApp } from "@/state/app";
import { editSongs, getProject } from "@/state/projects";
import { useWs } from "@/state/ws";
import { friendly } from "@/lib/errors";

export function DetectPopover({ p }: { p: Project }) {
  const settings = useApp((s) => s.settings);
  const [level, setLevel] = useState(sensitivityFromParams(p.detection?.params));
  const [minSec, setMinSec] = useState(p.detection?.params.min_song_sec ?? settings.minSongSec);
  const [busy, setBusy] = useState(false);
  const kept = p.songs.filter((s) => s.origin === "manual" || s.reviewed || !s.enabled || s.title || s.note || s.mutes.length || s.clip_id && p.clips.find((c) => c.id === s.clip_id)?.video).length;

  const run = async () => {
    setBusy(true);
    const before = getProject(p.id)!.songs;
    try {
      const defaults = await call("detect.default_params", {});
      const params = sensitivityParams(defaults, level, minSec);
      const res = await call("detect.rerun", { id: p.id, params });
      void updateSettings({ sensitivity: level, minSongSec: minSec });
      const beforeIds = new Set(before.map((s) => s.id));
      const added = res.songs.filter((s) => !beforeIds.has(s.id) && s.enabled);
      const removed = before.filter((s) => !res.songs.some((x) => x.id === s.id)).length;
      useWs.setState({ detectOpen: false });
      const parts = [added.length ? `곡 ${added.length}개가 새로 찾아졌어요` : "새로 찾은 곡은 없어요", removed ? `${removed}개는 빠졌어요` : "", kept ? `직접 고친 곡 ${kept}개는 그대로예요` : ""].filter(Boolean);
      toast(parts.join(". ") + ".", {
        kind: "ok",
        action: {
          label: "되돌리기",
          run: () => void editSongs(p.id, "다시 찾기 되돌리기", () => structuredClone(before)),
        },
      });
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="popover detect-pop" onKeyDown={(e) => e.stopPropagation()}>
      <div className="label">찾기 민감도</div>
      <input type="range" className="range" min={1} max={5} step={1} value={level} onChange={(e) => setLevel(Number(e.target.value))} style={{ width: "100%" }} />
      <div className="sens-scale">
        <span>낮게: 확실한 곡만</span>
        <b>{SENSITIVITY_LABEL[level - 1]}</b>
        <span>높게: 애매한 곡도</span>
      </div>
      <div className="row" style={{ marginTop: 8 }}>
        <span className="label grow">최소 곡 길이</span>
        <input className="input sm" type="number" min={15} max={180} step={5} value={minSec} onChange={(e) => setMinSec(Number(e.target.value) || 45)} style={{ width: 70 }} />
        <span className="faint">초</span>
      </div>
      <div className="hint">다시 찾기는 몇 초면 끝나요. 확인했거나 직접 고친 곡, 제외한 곡은 그대로 유지돼요.</div>
      <div className="row" style={{ justifyContent: "flex-end", marginTop: 4 }}>
        <button className="btn sm ghost" onClick={() => useWs.setState({ detectOpen: false })}>
          닫기
        </button>
        <button className="btn sm primary" onClick={() => void run()} disabled={busy}>
          <RefreshCw size={13} /> 다시 찾기
        </button>
      </div>
    </div>
  );
}
