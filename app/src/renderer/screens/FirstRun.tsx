// S0 첫 실행 마법사: 작업 폴더 → PC 확인 → 안내.
import { useEffect, useState } from "react";
import { AlertTriangle, ArrowRight, Check, Cpu, FolderOpen, Gauge, Mic2, ShieldCheck } from "lucide-react";
import { updateSettings, useApp } from "@/state/app";
import { call } from "@/lib/rpc";
import { fmtBytes } from "@/lib/format";
import type { DriveInfo } from "../../shared/bridge";
import { CudaAddonCard } from "@/components/CudaAddonCard";
import { maybeAutoInstall } from "@/state/addon";

export function FirstRun() {
  const settings = useApp((s) => s.settings);
  const system = useApp((s) => s.system);
  const engine = useApp((s) => s.engine);
  const [step, setStep] = useState(0);
  const [folder, setFolder] = useState(settings.projectsRoot ?? "");
  const [drives, setDrives] = useState<DriveInfo[]>([]);
  const [check, setCheck] = useState<{ ok: boolean; free: number; reason?: string } | null>(null);
  const [probing, setProbing] = useState(false);
  const [probed, setProbed] = useState(false);

  useEffect(() => {
    void window.cliper.drives().then((d) => {
      setDrives(d);
      if (!folder && d.length) {
        const best = [...d].sort((a, b) => b.free - a.free)[0];
        setFolder(`${best.path}Cliper`);
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!folder) return;
    const t = setTimeout(() => void window.cliper.checkFolder(folder).then(setCheck), 250);
    return () => clearTimeout(t);
  }, [folder]);

  const drive = drives.find((d) => folder.toUpperCase().startsWith(d.path.toUpperCase()));
  const cLow = drive?.path.toUpperCase().startsWith("C") && drive.free < 50e9;
  const better = [...drives].sort((a, b) => b.free - a.free)[0];
  const gpu = system?.gpu ?? null;

  // PC 확인: start the CUDA provider once (a few seconds) so the answer is real
  useEffect(() => {
    if (step !== 1 || engine.state !== "ready" || probed || probing) return;
    setProbing(true);
    call("system.probe_gpu", {})
      .then((info) => useApp.setState({ system: info }))
      .catch(() => {})
      .finally(() => {
        setProbing(false);
        setProbed(true);
        // NVIDIA GPU without a CUDA runtime: fetch the add-on in the background now
        maybeAutoInstall({ firstRun: true });
      });
  }, [step, engine.state, probed, probing]);

  const next = async () => {
    if (step === 0) {
      if (!check?.ok) return;
      await updateSettings({ projectsRoot: folder });
      setStep(1);
    } else if (step === 1) setStep(2);
    else await updateSettings({ firstRunDone: true });
  };

  return (
    <div className="firstrun">
      <div className="firstrun-card">
        <div className="fr-hero">
          <div className="brand-mark lg">
            <Mic2 size={22} />
          </div>
          <div>
            <h1>Cliper</h1>
            <p className="muted">노래방송 다시보기에서 노래만 골라 곡별 영상으로 만들어요.</p>
          </div>
        </div>
        <ul className="trust-list">
          <li>
            <ShieldCheck size={15} /> 모든 처리는 이 PC에서 해요. 영상과 목소리를 외부로 보내지 않아요.
          </li>
          <li>
            <Check size={15} /> 원본은 바뀌지 않아요. 언제든 처음 상태로 되돌릴 수 있어요.
          </li>
          <li>
            <Check size={15} /> 작업은 자동으로 저장돼요.
          </li>
        </ul>

        <div className="fr-steps">
          {["작업 폴더", "PC 확인", "안내"].map((t, i) => (
            <div key={t} className={`fr-step${i === step ? " on" : ""}${i < step ? " done" : ""}`}>
              <span className="n">{i < step ? <Check size={12} /> : i + 1}</span>
              {t}
            </div>
          ))}
        </div>

        {step === 0 && (
          <div className="fr-body">
            <div className="label">받은 소리와 영상, 작업을 저장할 곳</div>
            <div className="row">
              <input className={`input grow${check && !check.ok ? " invalid" : ""}`} value={folder} onChange={(e) => setFolder(e.target.value)} spellCheck={false} />
              <button
                className="btn subtle"
                onClick={async () => {
                  const p = await window.cliper.pickFolder(folder || undefined);
                  if (p) setFolder(p.endsWith("Cliper") ? p : `${p.replace(/[\\/]$/, "")}\\Cliper`);
                }}
              >
                <FolderOpen size={14} /> 바꾸기
              </button>
            </div>
            {check?.ok && (
              <div className="hint">
                {drive ? `${drive.path.slice(0, 1)}드라이브 여유 공간 ${fmtBytes(check.free)}` : `여유 공간 ${fmtBytes(check.free)}`} · 4시간 방송 하나에 보통 1~3GB 정도 써요(곡 수에 따라 달라요).
              </div>
            )}
            {check && !check.ok && <div className="hint" style={{ color: "var(--danger)" }}>이 폴더에는 저장할 수 없어요. 다른 폴더를 골라 주세요.</div>}
            {cLow && better && !better.path.toUpperCase().startsWith("C") && (
              <div className="banner warn">
                <AlertTriangle size={15} />
                <span className="grow">C드라이브 여유 공간이 적어요. {better.path.slice(0, 1)}드라이브({fmtBytes(better.free)} 남음)를 추천해요.</span>
                <button className="btn sm subtle" onClick={() => setFolder(`${better.path}Cliper`)}>
                  {better.path.slice(0, 1)}드라이브로
                </button>
              </div>
            )}
          </div>
        )}

        {step === 1 && (
          <div className="fr-body">
            <div className="pc-check">
              <Cpu size={18} />
              <div className="grow">
                <div className="pc-gpu">{gpu ? `${gpu.name}${gpu.vram_mb ? ` · ${Math.round(gpu.vram_mb / 1024)}GB` : ""}` : system ? "NVIDIA 그래픽카드를 찾지 못했어요" : "그래픽카드 확인 중…"}</div>
                {engine.state !== "ready" ? (
                  <div className="row faint">
                    <div className="spinner" /> 처리 엔진을 준비하는 중…
                  </div>
                ) : probing || (gpu && gpu.cuda_ok === null) ? (
                  <div className="row faint">
                    <div className="spinner" /> 그래픽카드로 처리할 수 있는지 확인하는 중… (몇 초)
                  </div>
                ) : gpu?.cuda_ok && !system?.prefer_cpu ? (
                  <div className="ok-line">
                    <Gauge size={14} /> 멘트 지우기가 빠르게 돌아가요 (5분 곡 약 8초).
                  </div>
                ) : (
                  <div className="banner warn" style={{ marginTop: 8 }}>
                    <AlertTriangle size={15} />
                    <div>
                      <span>멘트 지우기가 느릴 수 있어요. 곡 찾기와 영상 받기는 문제없어요. 멘트 지우기는 밤에 돌려 두는 걸 추천해요.</span>
                      {gpu?.reason && (
                        <details>
                          <summary>자세히</summary>
                          <pre className="detail">{gpu.reason}</pre>
                        </details>
                      )}
                    </div>
                  </div>
                )}
              </div>
            </div>
            <CudaAddonCard compact />
            {system && (
              <div className="hint">
                처리 엔진 {system.version} · 영상 도구 {system.ffmpeg ? "준비됨" : "없음"} · 분리 모델 {Object.values(system.models).every(Boolean) ? "준비됨" : "일부 없음"}
              </div>
            )}
          </div>
        )}

        {step === 2 && (
          <div className="fr-body">
            <div className="banner info">
              <AlertTriangle size={15} />
              <div>
                <p>클립을 올리기 전에 스트리머의 2차 창작 가이드라인을 확인해 주세요.</p>
                <p className="muted">커버곡은 유튜브 저작권 처리(Content ID)를 받을 수 있어요. 설명란에는 원본 링크와 스트리머 채널이 기본으로 들어가요.</p>
              </div>
            </div>
          </div>
        )}

        <div className="fr-foot">
          {step > 0 && (
            <button className="btn ghost" onClick={() => setStep(step - 1)}>
              이전
            </button>
          )}
          <span className="spacer" />
          <button className="btn primary lg" onClick={() => void next()} disabled={(step === 0 && !check?.ok) || (step === 1 && engine.state !== "ready" && engine.state !== "failed")}>
            {step === 2 ? "시작하기" : "다음"} <ArrowRight size={16} />
          </button>
        </div>
      </div>
    </div>
  );
}
