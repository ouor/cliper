import { createRoot } from "react-dom/client";
import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/app.css";
import { App } from "./App";
import { player, usePlayer, WORKLET_URL } from "./audio/player";
import { useWs } from "./state/ws";
import { useProjects } from "./state/projects";
import { useJobs } from "./state/jobs";
import { useApp } from "./state/app";

// QA handle (the renderer's own state; nothing privileged)
Object.assign(window, { __cliper: { player, usePlayer, useWs, useProjects, useJobs, useApp, workletUrl: WORKLET_URL } });

createRoot(document.getElementById("root")!).render(<App />);
