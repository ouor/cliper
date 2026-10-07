// 세트리스트 / 팬 타임스탬프 붙여넣기 → songs.match_setlist (미리보기 → 적용).
import { useState } from "react";
import { ListMusic } from "lucide-react";
import type { Project } from "@ipc/generated/Project";
import type { SetlistMatch } from "@ipc/generated/SetlistMatch";
import { Modal } from "@/components/common";
import { useWs } from "@/state/ws";
import { call } from "@/lib/rpc";
import { friendly } from "@/lib/errors";
import { toast } from "@/state/app";
import { fmtTime } from "@/lib/format";
import { songNumbers, songTitle } from "@/lib/song";

export function SetlistDialog({ p }: { p: Project }) {
  const [text, setText] = useState("");
  const [overwrite, setOverwrite] = useState(false);
  const [preview, setPreview] = useState<SetlistMatch | null>(null);
  const [busy, setBusy] = useState(false);
  const nums = songNumbers(p);
  const close = () => useWs.setState({ setlistOpen: false });

  const run = async (apply: boolean) => {
    setBusy(true);
    try {
      const r = await call("songs.match_setlist", { id: p.id, text, apply, overwrite });
      if (apply) {
        toast(`곡 ${r.applied}개에 제목을 넣었어요.`, { kind: "ok" });
        close();
      } else setPreview(r);
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    } finally {
      setBusy(false);
    }
  };

  const matched = preview?.entries.filter((e) => e.song_id).length ?? 0;
  return (
    <Modal
      title="세트리스트 붙여넣기"
      icon={<ListMusic size={18} />}
      onClose={close}
      width={720}
      footer={
        <>
          <label className="check" style={{ marginRight: "auto" }}>
            <input type="checkbox" checked={overwrite} onChange={(e) => setOverwrite(e.target.checked)} /> 이미 입력한 제목도 바꾸기
          </label>
          <button className="btn ghost" onClick={close}>
            취소
          </button>
          <button className="btn subtle" onClick={() => void run(false)} disabled={!text.trim() || busy}>
            미리보기
          </button>
          <button className="btn primary" onClick={() => void run(true)} disabled={!text.trim() || busy || !preview}>
            적용 ({matched}곡)
          </button>
        </>
      }
    >
      <p className="muted">방송 고정 댓글이나 팬이 정리한 타임스탬프를 그대로 붙여넣으세요. 시간이 있으면 가까운 곡에, 없으면 순서대로 맞춰요. 비어 있는 제목·원곡자만 채워요.</p>
      <textarea
        className="textarea"
        rows={8}
        placeholder={"00:23:10 아이돌 - YOASOBI\n00:31:40 밤양갱 - 비비\n…"}
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          setPreview(null);
        }}
        onKeyDown={(e) => e.stopPropagation()}
        autoFocus
      />
      {preview && (
        <div className="setlist-table">
          <div className="st-row head">
            <span>붙여넣은 줄</span>
            <span>시간</span>
            <span>맞춘 곡</span>
          </div>
          {preview.entries.map((e, i) => {
            const s = p.songs.find((x) => x.id === e.song_id);
            return (
              <div key={i} className={`st-row${s ? "" : " miss"}`}>
                <span className="ellipsis">
                  {e.title}
                  {e.artist && <span className="faint"> · {e.artist}</span>}
                </span>
                <span className="mono faint">{e.time !== null ? fmtTime(e.time, 0, true) : "–"}</span>
                <span className="ellipsis">
                  {s ? (
                    <>
                      <span className="mono">{String(nums.get(s.id) ?? "").padStart(2, "0")}</span> {fmtTime(s.start, 0, true)} {s.title ? <span className="faint">({songTitle(s)})</span> : null}
                      {e.distance !== null && e.distance > 30 && <span className="tag doubt">{Math.round(e.distance)}초 차이</span>}
                    </>
                  ) : (
                    <span className="faint">가까운 곡 없음</span>
                  )}
                </span>
              </div>
            );
          })}
        </div>
      )}
    </Modal>
  );
}
