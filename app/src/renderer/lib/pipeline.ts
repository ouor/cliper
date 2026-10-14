// App-side orchestration of engine jobs (기획서 5.x 흐름).
import type { Settings } from "@ipc/generated/Settings";
import { call } from "./rpc";
import { sensitivityParams } from "./song";
import { startJob } from "@/state/jobs";
import { getProject, updateProjectSettings } from "@/state/projects";
import { toast, useApp } from "@/state/app";
import type { ExportOptions } from "@ipc/generated/ExportOptions";
import { fetchVideo } from "./video";
import { fmtBytes, fmtDurationKo } from "./format";

/** App defaults (설정) → a new project's engine settings. */
export async function applyDefaultSettings(id: string, extra: Partial<Settings> = {}) {
  const s = useApp.getState().settings;
  await updateProjectSettings(id, {
    margin_before: s.marginSec,
    margin_after: s.marginSec,
    video_height: s.videoHeight,
    suggest_mutes: s.suggestMutes,
    mute_fade: s.autoMuteFade,
    mute_gain_db: s.muteGainDb,
    song_fade_in: s.songFadeIn,
    song_fade_out: s.songFadeOut,
    filename_template: s.filenameTemplate,
    loudness: s.loudness === null ? null : { target_lufs: s.loudness, true_peak: -1 },
    export_height: s.exportHeight,
    video_bitrate_k: s.exportVideoK,
    audio_bitrate_k: s.exportAudioK,
    ...extra,
  });
}

export async function detectParams() {
  const s = useApp.getState().settings;
  const defaults = await call("detect.default_params", {});
  return sensitivityParams(defaults, s.sensitivity, s.minSongSec);
}

/** 소리 받기 → 곡 찾기 (chained). */
export async function startAnalysis(id: string, fetchAudio: boolean) {
  const detect = async () => {
    const params = await detectParams();
    await startJob("detect.run", { id, params }, { label: "곡 찾기" });
  };
  if (fetchAudio) {
    const p = getProject(id);
    await startJob("audio.fetch", { id }, { label: p?.source?.platform === "local" ? "소리 꺼내기" : "소리 받기", then: () => void detect() });
  } else await detect();
}

/**
 * 영상 받기 (+ 나누기 + 멘트 제안) for the given songs: song ± margins, only what is missing
 * (`video.fetch`, process: true separates once the video is in). One song = what the user is
 * looking at (priority now), several = a user request in order.
 */
export async function fetchSongs(id: string, songIds: string[], _label?: string) {
  if (songIds.length === 0) return;
  const plan = await fetchVideo(id, { song_ids: songIds, priority: songIds.length === 1 ? "now" : "user", reason: "song", process: true, quiet: true });
  if (!plan) return;
  if (plan.request_id === null) {
    // all the video is already there (someone else's fetch, or stems were cleaned): separate now (U-16)
    void startJob("stems.ensure", { id, song_ids: songIds }, { label: "목소리 나누기" });
    toast("영상은 이미 있어요. 목소리와 반주를 나눠요.", { kind: "info", ttl: 3000 });
  } else toast(`영상 ${fmtDurationKo(plan.missing_sec)} 분량을 받고 나눠요 · 약 ${fmtBytes(plan.estimated_bytes)}`, { kind: "info", ttl: 3000 });
}

/** "밤새 전부 처리하기": songs → video + stems at batch priority (+ export that waits for its video). */
export async function processAll(id: string, opts: { exportToo: boolean }) {
  const p = getProject(id);
  if (!p) return;
  const s = useApp.getState().settings;
  const songs = p.songs.filter((x) => x.enabled && (!s.onlyReviewedFetch || x.reviewed));
  const ids = songs.map((x) => x.id);
  if (!ids.length) {
    toast("받을 곡이 없어요. 먼저 곡을 확인해 주세요.", { kind: "warn" });
    return;
  }
  const plan = await fetchVideo(id, { song_ids: ids, priority: "batch", reason: "batch", process: true, quiet: true });
  if (!plan) return;
  if (plan.request_id === null) void startJob("stems.ensure", { id, song_ids: ids }, { label: "목소리 나누기" });
  if (opts.exportToo) {
    const opt: Partial<ExportOptions> = { song_ids: ids, format: "mp4", include_unconfirmed: false, fetch: true };
    void startJob("export.run", { id, ...opt }, { label: `밤새 처리 · 내보내기 ${ids.length}곡` });
  }
  toast(`밤새 처리: 영상 ${ids.length}곡을 받고 나눠요${opts.exportToo ? " · 받는 대로 내보내요" : ""}.`, { kind: "info" });
  useApp.setState({ jobsOpen: true });
}
