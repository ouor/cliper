// Workspace UI state (selection, view ranges, pending ranges, focus requests).
import { create } from "zustand";

export type SortMode = "attention" | "time" | "length";

interface WsStore {
  projectId: string | null;
  selected: string | null;
  multi: string[]; // Shift+click selection (for merge)
  selectedMute: string | null;
  candidate: number | null; // index into detection.candidates
  lastEdge: "start" | "end";
  sort: SortMode;
  search: string;
  showExcluded: boolean;
  /** overview map view (VOD s) */
  mapView: [number, number] | null;
  mapHeight: number;
  /** detail timeline view (VOD s) */
  detailView: [number, number] | null;
  /** dragged-out range on the map waiting for "곡으로 추가" */
  pendingRange: [number, number] | null;
  /** B key: mute start mark */
  markStart: number | null;
  focus: { field: "title" | "note" | "search" | null; n: number };
  /** shows the detect settings popover */
  detectOpen: boolean;
  setlistOpen: boolean;
}

export const useWs = create<WsStore>(() => ({
  projectId: null,
  selected: null,
  multi: [],
  selectedMute: null,
  candidate: null,
  lastEdge: "start",
  sort: "attention",
  search: "",
  showExcluded: false,
  mapView: null,
  mapHeight: 168,
  detailView: null,
  pendingRange: null,
  markStart: null,
  focus: { field: null, n: 0 },
  detectOpen: false,
  setlistOpen: false,
}));

export function requestFocus(field: "title" | "note" | "search") {
  useWs.setState((s) => ({ focus: { field, n: s.focus.n + 1 } }));
}

export function resetWs(projectId: string) {
  useWs.setState({
    projectId,
    selected: null,
    multi: [],
    selectedMute: null,
    candidate: null,
    mapView: null,
    detailView: null,
    pendingRange: null,
    markStart: null,
    search: "",
    detectOpen: false,
    setlistOpen: false,
  });
}
