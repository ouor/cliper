import { createRoot } from "react-dom/client";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/app.css";
import { App } from "./App";
import { player, usePlayer, WORKLET_URL } from "./audio/player";
import { useWs } from "./state/ws";
import { applyChanged, editSongs, refreshList, replaceProject, syncStats, useProjects } from "./state/projects";
import { removeLocation } from "./lib/locations";
import { useJobs } from "./state/jobs";
import { useApp } from "./state/app";
import { installTipA11y } from "./lib/a11y-tips";
import { useAddon } from "./state/addon";
import { useVideo } from "./state/video";

// QA handle (the renderer's own state; nothing privileged)
Object.assign(window, { __cliper: { player, usePlayer, useWs, useProjects, useJobs, useApp, useVideo, workletUrl: WORKLET_URL, syncStats, useAddon, qa: { applyChanged, editSongs, refreshList, replaceProject, removeLocation } } });

// A file dropped anywhere except a real drop zone must not make Chromium navigate to it.
for (const type of ["dragover", "drop"] as const) document.addEventListener(type, (e) => e.preventDefault());

createRoot(document.getElementById("root")!).render(<App />);

// CSS tooltips → accessible names / descriptions
installTipA11y();
