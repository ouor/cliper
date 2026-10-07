// App-side settings (stored by the main process in userData/settings.json).
// Engine-side per-project settings live in Project.settings; the values below are
// the defaults applied to new projects plus purely app-level preferences.

export type Theme = "dark" | "light" | "system";
export type CloseBehavior = "ask" | "minimize" | "stop";
export type ExportPreset = "accurate" | "fast" | "archive" | "audio" | "editor";
export type Device = "auto" | "cpu";

export interface AppSettings {
  /** engine --projects root (작업 폴더). null until the first-run wizard ran. */
  projectsRoot: string | null;
  firstRunDone: boolean;

  // 일반
  theme: Theme;
  notifications: boolean;
  closeBehavior: CloseBehavior;
  lowPriority: boolean;

  // 곡 찾기
  sensitivity: number; // 1..5 (3 = engine default)
  minSongSec: number;
  showCandidates: boolean;
  shortSec: number; // "짧음" label threshold
  listenPreroll: number; // 시작/끝 듣기 앞 여유 (s)
  showLanes: { music: boolean; speech: boolean; sing: boolean };

  // 영상 받기
  videoHeight: number | null; // null = 최고 화질
  marginSec: number;
  onlyReviewedFetch: boolean;

  // 멘트 지우기
  suggestMutes: boolean;
  newSongMuteOn: boolean;
  muteFadeIn: number; // s (manual mutes)
  muteFadeOut: number;
  autoMuteFade: number; // engine settings.mute_fade for suggestions
  muteGainDb: number | null; // null = 완전 제거
  loopPad: number; // 구간 반복 앞뒤 여유 (s)

  // 내보내기
  exportPreset: ExportPreset;
  audioFormat: "wav" | "flac" | "mp3";
  songFadeIn: number;
  songFadeOut: number;
  filenameTemplate: string;
  loudness: number | null; // target LUFS, null = 원본 유지
  descTemplate: string;
  chapterFormat: boolean;

  // 단축키 재지정: action id → key combo
  keymap: Record<string, string>;

  // 고급
  device: Device;
  audioSinkId: string;
  cookies: { platform: "chzzk" | "soop"; savedAt: string } | null;
}

export const DEFAULT_DESC_TEMPLATE = `{스트리머} - {곡명}
📅 {방송일} 노래방송
원본 다시보기: {원본링크_시점포함}
스트리머 채널: {채널링크}`;

export const DEFAULT_SETTINGS: AppSettings = {
  projectsRoot: null,
  firstRunDone: false,
  theme: "dark",
  notifications: true,
  closeBehavior: "ask",
  lowPriority: false,
  sensitivity: 3,
  minSongSec: 45,
  showCandidates: true,
  shortSec: 90,
  listenPreroll: 5,
  showLanes: { music: false, speech: false, sing: false },
  videoHeight: null,
  marginSec: 30,
  onlyReviewedFetch: true,
  suggestMutes: true,
  newSongMuteOn: true,
  muteFadeIn: 0.08,
  muteFadeOut: 0.3,
  autoMuteFade: 0.3,
  muteGainDb: null,
  loopPad: 3,
  exportPreset: "accurate",
  audioFormat: "wav",
  songFadeIn: 0.3,
  songFadeOut: 1.5,
  filenameTemplate: "{vod_id}_{idx}_{title}",
  loudness: null,
  descTemplate: DEFAULT_DESC_TEMPLATE,
  chapterFormat: true,
  keymap: {},
  device: "auto",
  audioSinkId: "",
  cookies: null,
};
