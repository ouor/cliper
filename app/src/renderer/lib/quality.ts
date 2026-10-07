// mp4 export size and bitrate (engine settings export_height / video_bitrate_k / audio_bitrate_k).

export interface ExportQuality {
  /** output height; null = the fetched video's size */
  height: number | null;
  /** video kbps; null = constant quality (settings.video_quality) */
  videoK: number | null;
  /** AAC kbps */
  audioK: number;
}

export type QualityId = "source" | "1080" | "720" | "480" | "360" | "custom";

export const QUALITY_PRESETS: { id: Exclude<QualityId, "custom">; label: string; q: ExportQuality }[] = [
  { id: "source", label: "원본", q: { height: null, videoK: null, audioK: 320 } },
  { id: "1080", label: "1080p", q: { height: 1080, videoK: 4000, audioK: 128 } },
  { id: "720", label: "720p", q: { height: 720, videoK: 2000, audioK: 128 } },
  { id: "480", label: "480p", q: { height: 480, videoK: 1000, audioK: 96 } },
  { id: "360", label: "360p", q: { height: 360, videoK: 600, audioK: 96 } },
];

/** Engine ranges (docs/ipc.md). */
export const RANGES = {
  height: [144, 4320],
  videoK: [100, 100000],
  audioK: [32, 512],
} as const;

export const DEFAULT_QUALITY: ExportQuality = QUALITY_PRESETS[0].q;

export function qualityId(q: ExportQuality): QualityId {
  const hit = QUALITY_PRESETS.find((x) => x.q.height === q.height && x.q.videoK === q.videoK && x.q.audioK === q.audioK);
  return hit?.id ?? "custom";
}

export function fmtKbps(k: number): string {
  return k >= 1000 ? `${+(k / 1000).toFixed(2)} Mbps` : `${k}k`;
}

/** "720p · 영상 2 Mbps · 오디오 128k" / "원본 크기 · 일정 품질 · 오디오 320k" */
export function describeQuality(q: ExportQuality, quality?: number): string {
  const size = q.height ? `${q.height}p` : "원본 크기";
  const video = q.videoK ? `영상 ${fmtKbps(q.videoK)}` : `일정 품질${quality !== undefined ? ` ${quality}` : ""}`;
  return `${size} · ${video} · 오디오 ${q.audioK}k`;
}

/** null = ok, else a short Korean reason. */
export function checkRange(key: keyof typeof RANGES, v: number | null): string | null {
  if (v === null) return key === "audioK" ? "값을 넣어 주세요" : null;
  const [lo, hi] = RANGES[key];
  if (!Number.isInteger(v) || v < lo || v > hi) return `${lo}~${hi} 사이로 넣어 주세요`;
  return null;
}

export function fromSettings(s: { export_height: number | null; video_bitrate_k: number | null; audio_bitrate_k: number }): ExportQuality {
  return { height: s.export_height, videoK: s.video_bitrate_k, audioK: s.audio_bitrate_k };
}

export function toSettings(q: ExportQuality): { export_height: number | null; video_bitrate_k: number | null; audio_bitrate_k: number } {
  return { export_height: q.height, video_bitrate_k: q.videoK, audio_bitrate_k: q.audioK };
}
