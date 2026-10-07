// GPU 가속 구성요소 (CUDA add-on): status, progress, cancel / retry / reinstall / remove.
import { Download, ExternalLink, FileArchive, FolderOpen, RotateCcw, Trash2, X, Zap } from "lucide-react";
import { useApp, ask } from "@/state/app";
import { Progress } from "@/components/common";
import { addonLine, cancelCudaAddon, cudaAddon, CUDA_RELEASE_PAGE, fmtMB, installCudaAddon, installCudaAddonFromZip, removeCudaAddon, restartNow, useAddon } from "@/state/addon";

const failed0 = (p: string) => p === "failed";

export function CudaAddonCard({ compact }: { compact?: boolean }) {
  useApp((s) => s.system); // re-render when system info changes
  const st = useAddon();
  const info = cudaAddon();
  if (!info) return null;
  // nothing to say on a PC that doesn't need it and doesn't have it
  if (compact && !info.needed && !info.installed && st.phase === "idle") return null;

  const active = st.phase === "download" || st.phase === "verify" || st.phase === "extract";
  const frac = (st.phase === "download" || st.phase === "verify") && st.total ? st.done / st.total : null;
  const manual = failed0(st.phase) && (st.errorCode === "NETWORK" || st.errorCode === "ADDON" || st.errorCode === "INVALID_PARAMS");
  const failed = st.phase === "failed" || st.phase === "cancelled";

  return (
    <div className={`addon-card${compact ? " compact" : ""}`}>
      <div className="row">
        <Zap size={15} />
        <span className="grow">
          <b>GPU 가속 구성요소</b> <span className="faint">(CUDA, 약 {fmtMB(info.size)})</span>
        </span>
        {active && (
          <button className="btn xs ghost" onClick={() => void cancelCudaAddon()} data-tip="취소해도 CPU로 계속 처리해요">
            <X size={12} /> 취소
          </button>
        )}
        {failed && (
          <button className="btn xs subtle" onClick={() => void installCudaAddon()}>
            <RotateCcw size={12} /> 다시 시도
          </button>
        )}
        {st.phase === "restart_wait" && (
          <button className="btn xs subtle" onClick={() => void restartNow()} data-tip="진행 중인 작업은 멈춰요">
            지금 다시 시작
          </button>
        )}
        {!compact && st.phase === "idle" && !info.installed && (
          <button className="btn xs subtle" onClick={() => void installCudaAddon()}>
            <Download size={12} /> 설치
          </button>
        )}
      </div>
      <div className="hint">{addonLine(st, info)}</div>
      {(manual || (!compact && !info.installed && !active && st.phase !== "done")) && (
        <div className="row" style={{ gap: 6, flexWrap: "wrap" }}>
          {manual && (
            <button className="btn xs subtle" onClick={() => void window.cliper.openExternal(CUDA_RELEASE_PAGE)}>
              <ExternalLink size={12} /> 릴리스 페이지 열기
            </button>
          )}
          <button className={`btn xs ${manual ? "primary" : "ghost"}`} onClick={() => void installCudaAddonFromZip()}>
            <FileArchive size={12} /> zip 파일로 설치
          </button>
        </div>
      )}
      {active && <Progress value={frac} indeterminate={st.phase === "extract"} />}
      {active && <div className="hint faint">받는 동안에도 곡 찾기와 영상 받기는 그대로 할 수 있어요(CPU로 처리).</div>}
      {!compact && (info.installed || st.phase === "done") && (
        <div className="row" style={{ marginTop: 6 }}>
          <span className="faint grow" title={info.dir}>
            {info.dir}
          </span>
          <button className="btn xs ghost" onClick={() => void window.cliper.openPath(info.dir)}>
            <FolderOpen size={12} /> 폴더
          </button>
          <button className="btn xs ghost" disabled={active} onClick={() => void installCudaAddon({ reinstall: true })}>
            <RotateCcw size={12} /> 다시 설치
          </button>
          <button className="btn xs ghost" disabled={active} onClick={() => void installCudaAddonFromZip()}>
            <FileArchive size={12} /> zip 파일로 설치
          </button>
          <button
            className="btn xs ghost"
            disabled={active}
            onClick={async () => {
              const r = await ask({
                title: "GPU 가속 구성요소를 지울까요?",
                body: ["엔진이 다시 시작하고, 멘트 지우기는 CPU로 처리해요(느려요).", "필요하면 다시 설치할 수 있어요."],
                icon: "question",
                buttons: [
                  { id: "cancel", label: "취소", kind: "ghost" },
                  { id: "ok", label: "지우기", kind: "primary" },
                ],
              });
              if (r === "ok") await removeCudaAddon();
            }}
          >
            <Trash2 size={12} /> 지우기
          </button>
        </div>
      )}
    </div>
  );
}

/** Status-bar item while the add-on is being installed. */
export function CudaAddonStatus() {
  const st = useAddon();
  if (!["download", "verify", "extract", "restart_wait", "restarting", "probing"].includes(st.phase)) return null;
  const pct = (st.phase === "download" || st.phase === "verify") && st.total ? ` ${Math.floor((st.done / st.total) * 100)}%` : "";
  return (
    <button className="sb-item" onClick={() => useApp.setState({ overlay: "settings", settingsTab: "advanced" })} data-tip={addonLine(st, cudaAddon())} data-tip-pos="top">
      <Zap size={13} /> GPU 구성요소{pct || (st.phase === "restart_wait" ? " 재시작 대기" : " 설치 중")}
    </button>
  );
}
