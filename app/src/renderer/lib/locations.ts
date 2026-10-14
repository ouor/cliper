// Projects in other locations (docs/ipc.md "Project locations", 기획서 14.9): the previous
// projects folder keeps listing after a change, and single project folders can be opened from
// anywhere. Nothing here deletes files.
import { call, EngineError } from "./rpc";
import { go, toast } from "@/state/app";
import { refreshList, replaceProject, resetProjectState, useProjects } from "@/state/projects";

/** Remember the old projects folder so it is listed again once the engine runs on the new one. */
export async function rememberOldRoot(oldRoot: string | null) {
  if (oldRoot) await window.cliper.setState("pendingAddRoot", oldRoot);
}

/** After the engine (re)started: list the previous projects folder's projects (once). */
export async function addPendingRoot() {
  const old = await window.cliper.getState<string>("pendingAddRoot");
  if (!old) return;
  try {
    const r = await call("projects.add_root", { path: old });
    for (const s of r.projects) resetProjectState(s.id);
    await window.cliper.setState("pendingAddRoot", null);
    await refreshList();
    if (r.projects.length) toast(`이전 작업 폴더의 작업 ${r.projects.length}개도 그대로 목록에 있어요. 원래 자리에서 열려요.`, { kind: "ok", ttl: 6000 });
    if (r.skipped.length) toast(`같은 작업이 이미 있어서 ${r.skipped.length}개는 목록에 넣지 않았어요: ${r.skipped.map((x) => x.dir).join(", ")}`, { kind: "warn", ttl: 10000 });
  } catch (e) {
    // keep it pending (engine down etc.) unless the folder is gone
    if (e instanceof EngineError && (e.engineCode === "NOT_FOUND" || e.engineCode === "INVALID_PARAMS")) await window.cliper.setState("pendingAddRoot", null);
  }
}

/** [다른 위치의 작업 열기]: pick a project folder (moved by hand, another drive) and open it. */
export async function openOtherProject() {
  const path = await window.cliper.pickFolder(undefined);
  if (!path) return;
  try {
    const p = await call("project.open", { path });
    // another folder may hold the same id as something seen before: start from its state (QA-5)
    replaceProject(p);
    await refreshList();
    toast("다른 위치의 작업을 열었어요. 앞으로 목록에 계속 보여요(파일은 그 자리에 그대로).", { kind: "ok" });
    go({ name: "workspace", projectId: p.id, mode: "review" });
  } catch (e) {
    if (e instanceof EngineError && e.engineCode === "CONFLICT") {
      const dir = typeof e.data?.dir === "string" ? e.data.dir : "";
      toast(`같은 방송의 작업이 이미 목록에 있어요${dir ? ` (${dir})` : ""}. 그 작업을 쓰거나, 설정 > 저장 공간에서 그 위치를 목록에서 뺀 뒤 다시 열어 주세요.`, { kind: "warn", ttl: 10000 });
    } else if (e instanceof EngineError && (e.engineCode === "NOT_FOUND" || e.engineCode === "INVALID_PARAMS")) {
      toast("이 폴더에서 작업을 찾지 못했어요. project.json이 있는 작업 폴더를 골라 주세요.", { kind: "warn" });
    } else {
      toast(`작업을 열지 못했어요 · ${(e as Error).message}`, { kind: "error" });
    }
  }
}

/** Stop listing a location (nothing is deleted). */
export async function removeLocation(path: string) {
  try {
    const before = useProjects.getState().summaries.map((s) => s.id);
    await call("projects.remove_location", { path });
    await refreshList();
    // projects that left the list: forget their cached state, so another folder with the same id
    // opened later starts clean (QA-5)
    const now = new Set(useProjects.getState().summaries.map((s) => s.id));
    for (const id of before) if (!now.has(id)) resetProjectState(id);
    toast("목록에서 뺐어요. 폴더와 파일은 그대로예요.", { kind: "ok" });
  } catch (e) {
    toast(`목록에서 빼지 못했어요 · ${(e as Error).message}`, { kind: "error" });
  }
}
