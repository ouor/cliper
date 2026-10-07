// App-side orchestration of engine jobs (기획서 5.x 흐름).
import type { Settings } from "@ipc/generated/Settings";
import { call } from "./rpc";
import { sensitivityParams } from "./song";
import { startJob } from "@/state/jobs";
import { getProject, updateProjectSettings } from "@/state/projects";
import { toast, useApp } from "@/state/app";
import type { ExportOptions } from "@ipc/generated/ExportOptions";

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

/** 영상 받기 (+ 나누기 + 멘트 제안) for the given songs. */
export async function fetchSongs(id: string, songIds: string[], label?: string) {
  if (songIds.length === 0) return;
  // exact: only these songs (± margins), not close neighbours that share a merged clip
  const jobId = await startJob("clips.fetch", { id, song_ids: songIds, exact: true }, { label: label ?? (songIds.length === 1 ? "이 곡 받기" : `영상 받기 ${songIds.length}곡`) });
  if (jobId === null) toast("앞 작업이 끝나면 시작해요.", { kind: "info" });
}

/** "밤새 전부 처리하기": reviewed songs → clips (+ optional export without mute removal). */
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
  await startJob(
    "clips.fetch",
    { id, song_ids: ids, exact: true },
    {
      label: `밤새 처리 · 영상 받기 ${ids.length}곡`,
      then: opts.exportToo
        ? () => {
            const opt: Partial<ExportOptions> = { song_ids: ids, format: "mp4", include_unconfirmed: false };
            void startJob("export.run", { id, ...opt }, { label: `밤새 처리 · 내보내기 ${ids.length}곡` });
          }
        : undefined,
    },
  );
  useApp.setState({ jobsOpen: true });
}
