import { useEffect, useRef } from "react";
import { AlertTriangle, RotateCw } from "lucide-react";
import type { SystemInfo } from "@ipc/generated/SystemInfo";
import { PROTOCOL_VERSION } from "@ipc/protocol";
import { applyTheme, ask, go, openSettings, toast, useApp } from "./state/app";
import { applyChanged, loadProject, refreshList } from "./state/projects";
import { markEngineLost, useJobs, wireJobs } from "./state/jobs";
import { onNotification } from "./lib/rpc";
import { comboFromEvent, keysFor } from "./lib/keys";
import { DialogHost, isTyping, Toasts } from "./components/common";
import { StatusBar } from "./components/StatusBar";
import { JobsPanel } from "./components/JobsPanel";
import { Home } from "./screens/Home";
import { FirstRun } from "./screens/FirstRun";
import { Workspace } from "./screens/Workspace";
import { SettingsScreen } from "./screens/Settings";
import { HelpOverlay } from "./screens/HelpOverlay";
import { CookiesDialog } from "./screens/CookiesDialog";
import { player } from "./audio/player";

let booted = false;

async function boot() {
  if (booted) return;
  booted = true;
  const settings = await window.cliper.getSettings();
  useApp.setState({ settings, settingsLoaded: true });
  applyTheme(settings.theme);
  window.matchMedia("(prefers-color-scheme: dark)").addEventListener("change", () => applyTheme(useApp.getState().settings.theme));
  if (settings.audioSinkId) void player.setSink(settings.audioSinkId);

  wireJobs();
  onNotification("project.changed", (p) => applyChanged(p.project));

  let wasReady = false;
  const onStatus = (s: Awaited<ReturnType<typeof window.cliper.engineStatus>>) => {
    useApp.setState({ engine: s, system: (s.system as SystemInfo | null) ?? useApp.getState().system });
    if (s.state === "ready") {
      void refreshList();
      const r = useApp.getState().route;
      if (r.name === "workspace") void loadProject(r.projectId).catch(() => go({ name: "home" }));
      if (wasReady) toast("처리 엔진을 다시 시작했어요. 저장된 작업은 그대로예요.", { kind: "ok" });
      wasReady = true;
    } else if (wasReady && (s.state === "restarting" || s.state === "failed")) {
      markEngineLost();
    }
  };
  window.cliper.onEngineStatus(onStatus);
  onStatus(await window.cliper.engineStatus());

  window.cliper.onCloseRequest(async () => {
    const n = Object.values(useJobs.getState().jobs).filter((j) => j.status === "running").length + useJobs.getState().waiting.length;
    const r = await ask({
      title: `처리 중인 작업 ${n || ""}개가 있어요`.replace("  ", " "),
      body: ["닫으면 작업이 멈추고, 다음에 열 때 이어서 할 수 있어요.", "끝난 곡은 그대로 남아 있어요."],
      icon: "question",
      buttons: [
        { id: "cancel", label: "취소", kind: "ghost" },
        { id: "stop", label: "멈추고 닫기", kind: "subtle" },
        { id: "minimize", label: "최소화해서 계속", kind: "primary" },
      ],
    });
    window.cliper.answerClose(r === "cancel" ? "cancel" : (r as "stop" | "minimize"));
  });
}

function EngineBanner() {
  const engine = useApp((s) => s.engine);
  const system = useApp((s) => s.system);
  if (engine.state === "restarting" || engine.state === "failed") {
    return (
      <div className={`engine-banner ${engine.state}`}>
        {engine.state === "restarting" ? <div className="spinner" /> : <AlertTriangle size={16} />}
        <span className="grow">{engine.reason ?? (engine.state === "failed" ? "처리 엔진을 시작하지 못했어요." : "처리 엔진을 다시 시작하는 중이에요.")}</span>
        {engine.detail && (
          <details>
            <summary>자세히</summary>
            <pre className="detail">{engine.detail}</pre>
          </details>
        )}
        {engine.state === "failed" && (
          <button className="btn sm subtle" onClick={() => void window.cliper.restartEngine()}>
            <RotateCw size={14} /> 다시 시작
          </button>
        )}
      </div>
    );
  }
  // compatible when min_client_protocol <= PROTOCOL_VERSION <= protocol
  const incompatible = engine.protocol !== undefined && (PROTOCOL_VERSION > engine.protocol || PROTOCOL_VERSION < (engine.minClientProtocol ?? engine.protocol));
  if (engine.state === "ready" && incompatible) {
    return (
      <div className="engine-banner failed">
        <AlertTriangle size={16} />
        <span className="grow">처리 엔진과 앱의 버전이 맞지 않아요 (엔진 {engine.protocol}, 앱 {PROTOCOL_VERSION}). 일부 기능이 동작하지 않을 수 있어요. 앱을 다시 설치해 주세요.</span>
      </div>
    );
  }
  if (engine.state === "ready" && system) {
    const missing = Object.entries(system.models).filter(([, ok]) => !ok).map(([k]) => k);
    if (missing.length || !system.ffmpeg) {
      return (
        <div className="engine-banner failed">
          <AlertTriangle size={16} />
          <span className="grow">
            {!system.ffmpeg ? "영상 처리 도구(ffmpeg)를 찾을 수 없어요. " : ""}
            {missing.length ? `필요한 모델 파일이 없어요 (${missing.join(", ")}). ` : ""}앱을 다시 설치해 주세요.
          </span>
        </div>
      );
    }
  }
  return null;
}

export function App() {
  const loaded = useApp((s) => s.settingsLoaded);
  const firstRunDone = useApp((s) => s.settings.firstRunDone && !!s.settings.projectsRoot);
  const route = useApp((s) => s.route);
  const overlay = useApp((s) => s.overlay);
  const jobsOpen = useApp((s) => s.jobsOpen);
  const startedRef = useRef(false);

  useEffect(() => {
    if (startedRef.current) return;
    startedRef.current = true;
    void boot();
  }, []);

  // global shortcuts
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const combo = comboFromEvent(e);
      if (!combo) return;
      const st = useApp.getState();
      if (st.dialog) return;
      const km = st.settings.keymap;
      const is = (id: string, scope: "global") => keysFor(id, scope, km).includes(combo);
      if (is("jobs", "global")) {
        e.preventDefault();
        useApp.setState({ jobsOpen: !st.jobsOpen });
      } else if (is("settings", "global")) {
        e.preventDefault();
        if (st.overlay === "settings") useApp.setState({ overlay: null });
        else openSettings();
      } else if (is("help", "global") && !isTyping(e) && st.overlay !== "settings") {
        e.preventDefault();
        useApp.setState({ overlay: st.overlay === "help" ? null : "help" });
      } else if (combo === "Escape" && (st.overlay === "help" || st.jobsOpen) && !isTyping(e)) {
        useApp.setState({ overlay: st.overlay === "help" ? null : st.overlay, jobsOpen: false });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  if (!loaded) return <div className="app boot" />;
  if (!firstRunDone) {
    return (
      <div className="app">
        <FirstRun />
        <Toasts />
        <DialogHost />
      </div>
    );
  }
  return (
    <div className="app">
      <EngineBanner />
      <div className="app-main">{route.name === "home" ? <Home /> : <Workspace key={route.projectId} projectId={route.projectId} />}</div>
      <StatusBar />
      {jobsOpen && <JobsPanel />}
      {overlay === "settings" && <SettingsScreen />}
      {overlay === "help" && <HelpOverlay />}
      {overlay === "cookies" && <CookiesDialog />}
      <Toasts />
      <DialogHost />
    </div>
  );
}
