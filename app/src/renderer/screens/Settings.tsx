// S7 설정.
import { useEffect, useState, type ReactNode } from "react";
import { Cookie, Copy, Cpu, Download, Film, FolderOpen, HardDrive, Info, Keyboard, Mic2, RotateCcw, Scissors, Search, Settings2, Upload, X } from "lucide-react";
import { StorageTab } from "./Storage";
import { MarginPicker } from "./workspace/VideoControls";
import { useApp, updateSettings, toast, ask, applyTheme, type SettingsTab } from "@/state/app";
import { getProject } from "@/state/projects";
import { KEY_ACTIONS, comboFromEvent, keysFor, prettyKey, SCOPE_LABEL, type KeyScope } from "@/lib/keys";
import { DESC_VARS, SENSITIVITY_LABEL, renderDescription } from "@/lib/song";
import { applyDefaultSettings } from "@/lib/pipeline";
import { call } from "@/lib/rpc";
import { player } from "@/audio/player";
import { DEFAULT_SETTINGS } from "../../shared/settings";
import { friendly } from "@/lib/errors";
import { CudaAddonCard } from "@/components/CudaAddonCard";
import { QualityPicker } from "@/components/QualityPicker";
import { cudaAddon } from "@/state/addon";

const TABS: { id: SettingsTab; label: string; icon: ReactNode }[] = [
  { id: "general", label: "일반", icon: <Settings2 size={15} /> },
  { id: "detect", label: "곡 찾기", icon: <Search size={15} /> },
  { id: "video", label: "영상 받기", icon: <Download size={15} /> },
  { id: "storage", label: "저장 공간", icon: <HardDrive size={15} /> },
  { id: "talk", label: "멘트 지우기", icon: <Scissors size={15} /> },
  { id: "export", label: "내보내기", icon: <Upload size={15} /> },
  { id: "keys", label: "단축키", icon: <Keyboard size={15} /> },
  { id: "advanced", label: "고급", icon: <Cpu size={15} /> },
];

function Row({ label, hint, children }: { label: string; hint?: ReactNode; children: ReactNode }) {
  return (
    <div className="set-row">
      <div className="set-label">
        <div>{label}</div>
        {hint && <div className="hint">{hint}</div>}
      </div>
      <div className="set-control">{children}</div>
    </div>
  );
}

function Num({ value, onChange, min, max, step = 1, unit, width = 80 }: { value: number; onChange: (v: number) => void; min: number; max: number; step?: number; unit?: string; width?: number }) {
  const [t, setT] = useState<string | null>(null);
  return (
    <span className="row">
      <input
        className="input sm mono"
        style={{ width }}
        value={t ?? String(value)}
        onChange={(e) => setT(e.target.value)}
        onBlur={() => {
          const v = Number(t);
          if (t !== null && Number.isFinite(v)) onChange(Math.max(min, Math.min(max, Math.round(v / step) * step)));
          setT(null);
        }}
        onKeyDown={(e) => e.key === "Enter" && (e.target as HTMLInputElement).blur()}
      />
      {unit && <span className="faint">{unit}</span>}
    </span>
  );
}

function Toggle({ checked, onChange }: { checked: boolean; onChange: (v: boolean) => void }) {
  return <input type="checkbox" className="switch" checked={checked} onChange={(e) => onChange(e.target.checked)} />;
}

function ApplyToProject() {
  const route = useApp((s) => s.route);
  if (route.name !== "workspace") return null;
  return (
    <div className="banner info set-apply">
      <Info size={14} />
      <span className="grow">여기 기본값은 새로 만드는 작업에 쓰여요. 지금 열린 작업에도 적용할 수 있어요.</span>
      <button
        className="btn sm subtle"
        onClick={async () => {
          await applyDefaultSettings(route.projectId, { video_height: getProject(route.projectId)?.settings.video_height ?? null });
          toast("지금 작업에 적용했어요.", { kind: "ok" });
        }}
      >
        이 작업에도 적용
      </button>
    </div>
  );
}

function KeysTab() {
  const keymap = useApp((s) => s.settings.keymap);
  const [capture, setCapture] = useState<string | null>(null);
  useEffect(() => {
    if (!capture) return;
    const onKey = (e: KeyboardEvent) => {
      e.preventDefault();
      e.stopPropagation();
      if (e.key === "Escape") return setCapture(null);
      const combo = comboFromEvent(e);
      if (!combo || ["Ctrl", "Alt", "Shift"].includes(combo)) return;
      void updateSettings({ keymap: { ...keymap, [capture]: combo } });
      setCapture(null);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [capture, keymap]);
  const scopes: KeyScope[] = ["global", "play", "review", "talk"];
  return (
    <>
      <div className="row">
        <span className="hint grow">바꿀 동작의 [바꾸기]를 누르고 새 키를 누르세요. Esc로 취소해요.</span>
        <button className="btn sm subtle" onClick={() => void updateSettings({ keymap: {} })}>
          <RotateCcw size={13} /> 기본값으로
        </button>
      </div>
      {scopes.map((sc) => (
        <section key={sc} className="set-section">
          <h4>{SCOPE_LABEL[sc]}</h4>
          {KEY_ACTIONS.filter((a) => a.scope === sc).map((a) => {
            const id = `${a.scope}:${a.id}`;
            const changed = !!keymap[id];
            return (
              <div key={id} className="key-row">
                <span className="grow">{a.label}</span>
                {capture === id ? (
                  <span className="tag accent">새 키를 누르세요…</span>
                ) : (
                  keysFor(a.id, sc, keymap).map((k) => <kbd key={k}>{prettyKey(k)}</kbd>)
                )}
                <button className="btn xs ghost" onClick={() => setCapture(id)}>
                  바꾸기
                </button>
                {changed && (
                  <button className="btn xs ghost" onClick={() => {
                    const n = { ...keymap };
                    delete n[id];
                    void updateSettings({ keymap: n });
                  }}>
                    되돌리기
                  </button>
                )}
              </div>
            );
          })}
        </section>
      ))}
    </>
  );
}

export function SettingsScreen() {
  const st = useApp((s) => s.settings);
  const tab = useApp((s) => s.settingsTab);
  const system = useApp((s) => s.system);
  const engine = useApp((s) => s.engine);
  const route = useApp((s) => s.route);
  const [devices, setDevices] = useState<MediaDeviceInfo[]>([]);
  const close = () => useApp.setState({ overlay: null });
  const set = (patch: Partial<typeof st>) => void updateSettings(patch);
  const sampleProject = route.name === "workspace" ? getProject(route.projectId) : undefined;
  const sampleSong = sampleProject?.songs.find((s) => s.enabled);

  useEffect(() => {
    if (tab === "advanced") void navigator.mediaDevices?.enumerateDevices().then((d) => setDevices(d.filter((x) => x.kind === "audiooutput")));
  }, [tab]);

  const changeRoot = async () => {
    const p = await window.cliper.pickFolder(st.projectsRoot ?? undefined);
    if (!p || p === st.projectsRoot) return;
    const chk = await window.cliper.checkFolder(p);
    if (!chk.ok) return toast("이 폴더에는 저장할 수 없어요. 다른 폴더를 골라 주세요.", { kind: "error" });
    const r = await ask({
      title: "작업 폴더를 바꿀까요?",
      body: ["새 작업부터 새 폴더에 만들어요. 처리 엔진이 다시 시작하고, 진행 중인 작업은 멈춰요.", "기존 작업은 원래 폴더에 그대로 두고, 목록에서 계속 보이고 그 자리에서 열려요."],
      icon: "question",
      buttons: [
        { id: "cancel", label: "취소", kind: "ghost" },
        { id: "ok", label: "바꾸기", kind: "primary" },
      ],
    });
    if (r === "ok") {
      useApp.setState({ route: { name: "home" } });
      await updateSettings({ projectsRoot: p });
    }
  };

  const diag = () =>
    [
      `Cliper desktop 0.2.1 · Electron ${window.cliper.versions.electron} · Chrome ${window.cliper.versions.chrome}`,
      `engine ${system?.version ?? "?"} · state ${engine.state} · restarts ${engine.restarts}`,
      `ffmpeg: ${system?.ffmpeg ?? "없음"}`,
      `models: ${system ? Object.entries(system.models).map(([k, v]) => `${k}=${v ? "ok" : "missing"}`).join(", ") : "?"}`,
      `cuda: ${system?.cuda_dirs.join("; ") || "없음"} · prefer_cpu ${system?.prefer_cpu}`,
      `gpu: ${system?.gpu ? `${system.gpu.name} ${system.gpu.vram_mb}MB cuda_ok=${system.gpu.cuda_ok} ${system.gpu.reason ?? ""}` : "없음"} · keep_awake ${system?.keep_awake}`,
      `cuda_addon: ${(() => {
        const a = cudaAddon();
        return a ? `v${a.version} installed=${a.installed} needed=${a.needed} dir=${a.dir}` : "n/a";
      })()}`,
      `projects: ${st.projectsRoot}`,
    ].join("\n");

  return (
    <div className="settings-overlay" onKeyDown={(e) => e.key === "Escape" && close()}>
      <div className="settings">
        <nav className="set-nav">
          <div className="set-nav-title">설정</div>
          {TABS.map((t) => (
            <button key={t.id} className={tab === t.id ? "on" : ""} onClick={() => useApp.setState({ settingsTab: t.id })}>
              {t.icon} {t.label}
            </button>
          ))}
        </nav>
        <div className="set-body">
          <header>
            <h2>{TABS.find((t) => t.id === tab)?.label}</h2>
            <span className="spacer" />
            <span className="faint">자동으로 저장돼요</span>
            <button className="btn ghost icon sm" onClick={close} aria-label="닫기">
              <X size={16} />
            </button>
          </header>
          <div className="set-scroll">
            {tab === "general" && (
              <>
                <Row label="작업 폴더" hint="받은 소리와 영상, 작업을 저장하는 곳이에요.">
                  <span className="row grow">
                    <input className="input sm grow" style={{ minWidth: 340 }} readOnly value={st.projectsRoot ?? ""} title={st.projectsRoot ?? ""} />
                    <button className="btn sm subtle" onClick={() => void changeRoot()}>
                      <FolderOpen size={13} /> 바꾸기
                    </button>
                    <button className="btn sm ghost" onClick={() => st.projectsRoot && void window.cliper.openPath(st.projectsRoot)}>
                      열기
                    </button>
                  </span>
                </Row>
                <Row label="작업 중 절전 방지" hint="처리 엔진이 작업하는 동안 PC가 잠들지 않게 해요. 작업이 끝나면 자동으로 풀려요.">
                  <Toggle
                    checked={system?.keep_awake ?? true}
                    onChange={async (v) => {
                      try {
                        await call("system.set_keep_awake", { enabled: v });
                        useApp.setState({ system: await call("system.info", {}) });
                      } catch (e) {
                        toast(friendly(e).title, { kind: "error" });
                      }
                    }}
                  />
                </Row>
                <Row label="완료 알림" hint="앱을 보고 있지 않을 때 Windows 알림으로 알려 줘요.">
                  <Toggle checked={st.notifications} onChange={(v) => set({ notifications: v })} />
                </Row>
                <Row label="창을 닫을 때 작업이 있으면">
                  <select className="select sm" value={st.closeBehavior} onChange={(e) => set({ closeBehavior: e.target.value as typeof st.closeBehavior })}>
                    <option value="ask">물어보기</option>
                    <option value="minimize">최소화해서 계속</option>
                    <option value="stop">멈추고 닫기</option>
                  </select>
                </Row>
                <Row label="처리 우선순위" hint="방송 중에 돌릴 때는 '낮게'로 PC 부담을 줄이세요.">
                  <div className="seg">
                    <button className={!st.lowPriority ? "on" : ""} onClick={() => set({ lowPriority: false })}>
                      보통
                    </button>
                    <button className={st.lowPriority ? "on" : ""} onClick={() => set({ lowPriority: true })}>
                      낮게
                    </button>
                  </div>
                </Row>
                <Row label="테마">
                  <div className="seg">
                    {(["dark", "light", "system"] as const).map((t) => (
                      <button
                        key={t}
                        className={st.theme === t ? "on" : ""}
                        onClick={() => {
                          set({ theme: t });
                          applyTheme(t);
                        }}
                      >
                        {{ dark: "어둡게", light: "밝게", system: "시스템" }[t]}
                      </button>
                    ))}
                  </div>
                </Row>
              </>
            )}
            {tab === "detect" && (
              <>
                <Row label="찾기 민감도" hint="낮게: 확실한 곡만 · 높게: 애매한 곡도. 방송마다 지도에서 바꿔 다시 찾을 수 있어요.">
                  <span className="row" style={{ width: 320 }}>
                    <input type="range" className="range grow" min={1} max={5} value={st.sensitivity} onChange={(e) => set({ sensitivity: Number(e.target.value) })} />
                    <b style={{ width: 96 }}>{SENSITIVITY_LABEL[st.sensitivity - 1]}</b>
                  </span>
                </Row>
                <Row label="최소 곡 길이">
                  <Num value={st.minSongSec} onChange={(v) => set({ minSongSec: v })} min={15} max={180} step={5} unit="초" />
                </Row>
                <Row label="후보 구간 표시" hint="기준에 조금 못 미친 '놓친 곡일 수 있는' 구간을 점선으로 보여 줘요.">
                  <Toggle checked={st.showCandidates} onChange={(v) => set({ showCandidates: v })} />
                </Row>
                <Row label={`"짧음" 라벨 기준`}>
                  <Num value={st.shortSec} onChange={(v) => set({ shortSec: v })} min={30} max={180} step={5} unit="초 미만" />
                </Row>
                <Row label="시작/끝 듣기 앞 여유">
                  <Num value={st.listenPreroll} onChange={(v) => set({ listenPreroll: v })} min={1} max={10} unit="초" />
                </Row>
              </>
            )}
            {tab === "video" && (
              <>
                <ApplyToProject />
                <Row label="화질" hint="방송마다 확인 카드에서 바꿀 수 있어요.">
                  <select className="select sm" value={st.videoHeight ?? ""} onChange={(e) => set({ videoHeight: e.target.value ? Number(e.target.value) : null })}>
                    <option value="">최고 화질</option>
                    {[1080, 720, 480, 360].map((h) => (
                      <option key={h} value={h}>
                        {h}p
                      </option>
                    ))}
                  </select>
                </Row>
                <Row label="앞뒤 여유" hint="곡 앞뒤로 미리 받아 두는 영상이에요. 늘리면 곡을 넓힐 때 기다리지 않아요. 곡 하나에 1분 늘릴 때마다 1080p 기준 약 60MB를 더 써요. 새 작업의 기본값이고, 작업마다 작업공간 ⋯ 메뉴에서 바꿀 수 있어요.">
                  <MarginPicker value={st.marginSec} onChange={(v) => set({ marginSec: v })} />
                </Row>
                <Row label="모자란 영상 자동으로 받기" hint="영상을 받은 곡의 경계를 넓히면, 마지막으로 고친 뒤 3초 뒤에 모자란 부분만 받아요(한 번에 5분까지). 끄면 작업공간 위 [영상 맞추기]로 한꺼번에 받아요.">
                  <Toggle checked={st.autoTopUp} onChange={(v) => set({ autoTopUp: v })} />
                </Row>
                <Row label="확인한 곡만 받기" hint="끄면 '밤새 전부 처리하기'가 확인하지 않은 곡도 받아요.">
                  <Toggle checked={st.onlyReviewedFetch} onChange={(v) => set({ onlyReviewedFetch: v })} />
                </Row>
              </>
            )}
            {tab === "talk" && (
              <>
                <ApplyToProject />
                <Row label="멘트 자동 제안" hint="영상을 받은 뒤 멘트가 겹친 부분을 찾아 '확인 전'으로 제안해요. 확인한 구간만 내보내요.">
                  <Toggle checked={st.suggestMutes} onChange={(v) => set({ suggestMutes: v })} />
                </Row>
                <Row label="직접 만든 구간의 페이드" hint="앞은 짧게, 뒤는 리버브 꼬리를 생각해 길게 (5~2000ms)">
                  <span className="row">
                    <span className="faint">앞</span>
                    <Num value={Math.round(st.muteFadeIn * 1000)} onChange={(v) => set({ muteFadeIn: v / 1000 })} min={5} max={2000} unit="ms" />
                    <span className="faint">뒤</span>
                    <Num value={Math.round(st.muteFadeOut * 1000)} onChange={(v) => set({ muteFadeOut: v / 1000 })} min={5} max={2000} unit="ms" />
                  </span>
                </Row>
                <Row label="자동 제안의 페이드">
                  <Num value={Math.round(st.autoMuteFade * 1000)} onChange={(v) => set({ autoMuteFade: v / 1000 })} min={5} max={2000} unit="ms" />
                </Row>
                <Row label="기본 제거 강도" hint="'줄이기'는 목소리를 완전히 지우지 않고 작게 해요. 지운 흔적이 덜 들려요.">
                  <select className="select sm" value={st.muteGainDb === null ? "" : String(st.muteGainDb)} onChange={(e) => set({ muteGainDb: e.target.value === "" ? null : Number(e.target.value) })}>
                    <option value="">완전 제거</option>
                    {[-30, -24, -18, -12, -6].map((d) => (
                      <option key={d} value={d}>
                        줄이기 {d}dB
                      </option>
                    ))}
                  </select>
                </Row>
                <Row label="구간 반복 재생 앞뒤 여유">
                  <Num value={st.loopPad} onChange={(v) => set({ loopPad: v })} min={1} max={5} unit="초" />
                </Row>
              </>
            )}
            {tab === "export" && (
              <>
                <ApplyToProject />
                <Row label="곡 페이드 인 / 아웃" hint="내보낼 때 곡 시작과 끝을 부드럽게 (0.01~5초)">
                  <span className="row">
                    <Num value={st.songFadeIn} onChange={(v) => set({ songFadeIn: v })} min={0.01} max={5} step={0.01} unit="초" />
                    <Num value={st.songFadeOut} onChange={(v) => set({ songFadeOut: v })} min={0.01} max={5} step={0.01} unit="초" />
                  </span>
                </Row>
                <Row label="화질 (mp4)" hint="원본은 받은 영상 크기 그대로 일정 품질로 만들어요. 작은 화질은 키우지 않고, '빠르게'에는 적용되지 않아요.">
                  <QualityPicker value={{ height: st.exportHeight, videoK: st.exportVideoK, audioK: st.exportAudioK }} onChange={(q) => set({ exportHeight: q.height, exportVideoK: q.videoK, exportAudioK: q.audioK })} />
                </Row>
                <Row label="파일 이름" hint="{date} {yymmdd} {platform} {vod_id} {channel} {vod_title} {idx} {title} {artist} {start}">
                  <input className="input sm mono" style={{ width: 320 }} value={st.filenameTemplate} onChange={(e) => set({ filenameTemplate: e.target.value })} />
                </Row>
                <Row label="소리 크기" hint="유튜브 기준은 -14 LUFS, 최대 -1 dBTP예요.">
                  <div className="seg">
                    <button className={st.loudness === null ? "on" : ""} onClick={() => set({ loudness: null })}>
                      원본 유지
                    </button>
                    <button className={st.loudness !== null ? "on" : ""} onClick={() => set({ loudness: -14 })}>
                      -14 LUFS로 맞추기
                    </button>
                  </div>
                </Row>
                <Row label="타임스탬프 목록" hint="유튜브 챕터 형식은 첫 줄에 00:00을 넣어요.">
                  <label className="check">
                    <input type="checkbox" checked={st.chapterFormat} onChange={(e) => set({ chapterFormat: e.target.checked })} /> 유튜브 챕터 형식
                  </label>
                </Row>
                <div className="set-row col-row">
                  <div className="set-label">
                    <div>설명란 템플릿</div>
                    <div className="hint">원본 링크와 스트리머 채널을 기본으로 넣어요. 값이 없는 줄은 빠져요.</div>
                  </div>
                  <textarea className="textarea mono" rows={5} value={st.descTemplate} onChange={(e) => set({ descTemplate: e.target.value })} onKeyDown={(e) => e.stopPropagation()} />
                  <div className="tokens">
                    {DESC_VARS.map((v) => (
                      <button key={v} className="tag outline as-btn" onClick={() => set({ descTemplate: st.descTemplate + v })}>
                        {v}
                      </button>
                    ))}
                    <button className="btn xs ghost" onClick={() => set({ descTemplate: DEFAULT_SETTINGS.descTemplate })}>
                      기본값
                    </button>
                  </div>
                  {sampleProject && sampleSong && (
                    <>
                      <div className="hint">미리보기 (지금 작업의 첫 곡)</div>
                      <pre className="detail">{renderDescription(st.descTemplate, sampleProject, sampleSong)}</pre>
                    </>
                  )}
                </div>
              </>
            )}
            {tab === "keys" && <KeysTab />}
            {tab === "storage" && <StorageTab />}
            {tab === "advanced" && (
              <>
                <Row label="로그인 쿠키" hint="구독자 전용이나 비공개 다시보기를 받을 때만 필요해요.">
                  <span className="row">
                    <span className={`tag ${system?.cookies.set ? (system.cookies.valid === false ? "danger" : "ok") : ""}`}>{system?.cookies.set ? (system.cookies.valid === false ? "만료됨" : "설정됨") : "없음"}</span>
                    <button className="btn sm subtle" onClick={() => useApp.setState({ overlay: "cookies" })}>
                      <Cookie size={13} /> 설정…
                    </button>
                  </span>
                </Row>
                <Row label="처리 장치" hint="CPU는 느리지만 그래픽카드 문제를 피할 수 있어요. 바꾸면 엔진이 다시 시작해요.">
                  <div className="seg">
                    <button className={st.device === "auto" ? "on" : ""} onClick={() => set({ device: "auto" })}>
                      자동 (GPU 우선)
                    </button>
                    <button className={st.device === "cpu" ? "on" : ""} onClick={() => set({ device: "cpu" })}>
                      CPU
                    </button>
                  </div>
                </Row>
                <Row label="GPU 가속" hint="NVIDIA 그래픽카드에서 멘트 지우기를 빠르게 하는 구성요소예요. 필요한 PC에서는 자동으로 받아요.">
                  <span className="grow">
                    {cudaAddon() ? <CudaAddonCard /> : <span className="faint">이 엔진 버전에서는 지원하지 않아요</span>}
                  </span>
                </Row>
                <Row label="오디오 출력 장치" hint="모니터 헤드폰을 따로 쓰는 경우">
                  <select
                    className="select sm"
                    value={st.audioSinkId}
                    onChange={(e) => {
                      set({ audioSinkId: e.target.value });
                      void player.setSink(e.target.value);
                    }}
                  >
                    <option value="">시스템 기본</option>
                    {devices
                      .filter((d) => d.deviceId && d.deviceId !== "default")
                      .map((d) => (
                        <option key={d.deviceId} value={d.deviceId}>
                          {d.label || "출력 장치"}
                        </option>
                      ))}
                  </select>
                </Row>
                <Row label="저장 공간 정리" hint="받은 영상과 나눈 소리를 단계별로 정리해요.">
                  <button className="btn sm subtle" onClick={() => useApp.setState({ settingsTab: "storage" })}>
                    <HardDrive size={13} /> 저장 공간 열기
                  </button>
                </Row>
                <Row label="로그와 진단">
                  <span className="row">
                    <button className="btn sm subtle" onClick={() => void window.cliper.logDir().then((d) => window.cliper.openPath(d))}>
                      <FolderOpen size={13} /> 로그 폴더 열기
                    </button>
                    <button
                      className="btn sm subtle"
                      onClick={() => {
                        void navigator.clipboard.writeText(diag());
                        toast("진단 정보를 복사했어요. 영상·제목·링크는 들어 있지 않아요.", { kind: "ok" });
                      }}
                    >
                      <Copy size={13} /> 진단 정보 복사
                    </button>
                  </span>
                </Row>
                <div className="set-row col-row">
                  <div className="set-label">
                    <div>정보</div>
                  </div>
                  <div className="about">
                    <div>
                      <Mic2 size={13} /> Cliper 0.2.1 · 처리 엔진 {system?.version ?? "?"}
                    </div>
                    <div>
                      <Film size={13} /> 목소리 분리: MDX-Net (UVR-MDX-NET-Voc_FT) · 곡 찾기: PANNs
                    </div>
                    <div>
                      <Cpu size={13} /> {system?.gpu?.name ?? "NVIDIA 그래픽카드 없음"} · {system?.gpu?.cuda_ok === true ? "GPU로 처리" : system?.gpu?.cuda_ok === false ? `GPU 사용 불가${system.gpu.reason ? ` (${system.gpu.reason})` : ""}` : "GPU 사용 여부는 첫 처리 때 확인"}
                      {system?.prefer_cpu ? " (CPU 사용 중)" : ""}
                    </div>
                    <div className="faint">ffmpeg: {system?.ffmpeg?.split(" Copyright")[0] ?? "없음"}</div>
                    <div className="faint">모든 처리는 이 PC에서 해요. 사용 통계는 보내지 않아요.</div>
                  </div>
                </div>
              </>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
