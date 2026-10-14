// Engine error codes → user-facing Korean (기획서 8.3: 무슨 일 + 이유 + 행동 + 잃은 것 없음).
import { EngineError } from "./rpc";

export type ErrorAction = "retry" | "cookies" | "settings" | "lowerQuality" | "update" | "none";

export interface FriendlyError {
  title: string;
  hint?: string;
  action: ErrorAction;
  /** technical detail for [자세히] */
  detail?: string;
  code: string;
}

const TABLE: Record<string, Omit<FriendlyError, "code" | "detail">> = {
  UNRECOGNIZED_INPUT: {
    title: "치지직이나 SOOP 다시보기 링크가 아닌 것 같아요.",
    hint: "예: https://chzzk.naver.com/video/1234567 · https://vod.sooplive.com/player/123456789 · 내 PC의 녹화 파일은 [녹화 파일 열기]로 넣을 수 있어요.",
    action: "none",
  },
  VOD_NOT_FOUND: { title: "이 다시보기를 찾을 수 없어요.", hint: "삭제됐거나 비공개로 바뀌었을 수 있어요. 이미 받은 곡은 계속 작업할 수 있어요.", action: "retry" },
  AUTH_REQUIRED: { title: "이 다시보기는 로그인이 필요해요 (구독자 전용 등).", hint: "설정 > 고급에서 로그인 쿠키를 넣으면 받을 수 있어요. 쿠키가 있다면 만료된 것 같아요.", action: "cookies" },
  NETWORK: { title: "연결이 끊겼어요.", hint: "이미 받은 곡은 그대로예요. 잠시 뒤 다시 시도해 주세요.", action: "retry" },
  CDN_TRUNCATED: { title: "플랫폼 서버에 있는 영상 일부가 깨져 있어요.", hint: "화질을 낮추거나 나중에 다시 시도해 보세요. 이미 받은 곡은 그대로예요.", action: "lowerQuality" },
  FORMAT: { title: "플랫폼에서 방송 정보를 읽지 못했어요.", hint: "플랫폼이 바뀌었을 수 있어요. 업데이트를 확인해 주세요.", action: "update" },
  FFMPEG_MISSING: { title: "영상 처리 도구(ffmpeg)를 찾을 수 없어요.", hint: "앱을 다시 설치해 주세요.", action: "none" },
  FFMPEG_FAILED: { title: "영상을 처리하다 문제가 생겼어요.", hint: "원본은 그대로예요. 다시 시도해 보고, 계속되면 진단 정보를 보내 주세요.", action: "retry" },
  MODEL_MISSING: { title: "목소리 분리에 필요한 파일이 없어요.", hint: "앱을 다시 설치해 주세요.", action: "none" },
  MODEL: {
    title: "목소리와 반주를 나누지 못했어요.",
    hint: "그래픽카드 메모리가 부족할 수 있어요. 방송 프로그램이나 게임을 닫고 다시 시도해 주세요.",
    action: "retry",
  },
  CANCELLED: { title: "취소했어요.", hint: "끝난 곡은 그대로 남아 있어요.", action: "none" },
  BUSY: { title: "같은 작업이 이미 진행 중이에요.", hint: "끝나면 이어서 시작할게요.", action: "none" },
  CONFLICT: { title: "그사이 작업 내용이 바뀌었어요.", hint: "최신 상태로 다시 불러왔어요. 한 번 더 해 주세요.", action: "retry" },
  PRECONDITION: { title: "아직 이 단계를 할 수 없어요.", action: "none" },
  NOT_FOUND: { title: "작업을 찾을 수 없어요.", hint: "다른 곳에서 지워졌을 수 있어요.", action: "none" },
  INVALID_PARAMS: { title: "요청을 처리하지 못했어요.", hint: "입력한 값을 확인해 주세요.", action: "none" },
  VIDEO_MISSING: { title: "이 범위의 영상을 아직 받지 않았어요.", hint: "[받고 내보내기]로 모자란 부분을 받은 뒤 이어서 할 수 있어요.", action: "none" },
  VOD_GONE: { title: "다시보기를 더 이상 열 수 없어요.", hint: "받아 둔 영상 안에서는 계속 작업할 수 있어요.", action: "none" },
  NO_SPACE: { title: "저장 공간이 부족해요.", hint: "설정 > 저장 공간에서 정리하거나 작업 폴더를 바꿔 주세요.", action: "settings" },
  PINNED: { title: "보관한 작업이라 정리하지 않았어요.", hint: "보관을 풀거나 '그래도 정리하기'를 눌러 주세요.", action: "none" },
  IO: { title: "파일을 읽거나 쓰지 못했어요.", hint: "저장 공간이 부족하거나 폴더 권한이 없을 수 있어요.", action: "settings" },
  INTERNAL: { title: "예상하지 못한 문제가 생겼어요.", hint: "원본과 저장된 작업은 그대로예요. 다시 시도해 주세요.", action: "retry" },
  TIMEOUT: { title: "처리 엔진이 응답하지 않아요.", hint: "잠시 뒤 다시 해 보세요. 계속되면 엔진을 다시 시작해 주세요. 저장된 작업은 그대로예요.", action: "retry" },
  ENGINE_DOWN: { title: "처리 엔진이 응답하지 않아요.", hint: "다시 시작하는 중이에요. 저장된 작업은 그대로예요.", action: "retry" },
};

const PRECONDITION_HINTS: [RegExp, string][] = [
  [/audio/i, "먼저 소리를 받아야 해요."],
  [/detect|probs/i, "먼저 곡 찾기를 해야 해요."],
  [/clip|video|stems/i, "먼저 이 곡의 영상을 받아야 해요."],
];

export function friendly(e: unknown): FriendlyError {
  if (e instanceof EngineError) {
    const base = TABLE[e.engineCode] ?? TABLE.INTERNAL;
    let hint = base.hint;
    if (e.engineCode === "PRECONDITION") hint = PRECONDITION_HINTS.find(([re]) => re.test(e.message))?.[1] ?? e.message;
    if (e.engineCode === "IO" && /space|disk|용량|No space/i.test(e.message)) {
      return { title: "저장 공간이 부족해요.", hint: "작업 폴더를 바꾸거나 받은 영상을 정리해 주세요.", action: "settings", code: "IO", detail: e.message };
    }
    return { ...base, hint, code: e.engineCode, detail: `${e.method}: [${e.engineCode}] ${e.message}` };
  }
  if (e && typeof e === "object" && "code" in e && "message" in e) {
    // ApiError from job.failed
    const ae = e as { code: string; message: string };
    const base = TABLE[ae.code] ?? TABLE.INTERNAL;
    let hint = base.hint;
    if (ae.code === "PRECONDITION") hint = PRECONDITION_HINTS.find(([re]) => re.test(ae.message))?.[1] ?? ae.message;
    if (/out of memory|OOM|CUDA_ERROR_OUT_OF_MEMORY|cudaErrorMemoryAllocation/i.test(ae.message)) {
      return { title: "그래픽카드 메모리가 부족해요.", hint: "다른 프로그램(방송 프로그램, 게임)을 닫고 다시 시도해 주세요.", action: "retry", code: ae.code, detail: ae.message };
    }
    return { ...base, hint, code: ae.code, detail: `[${ae.code}] ${ae.message}` };
  }
  return { ...TABLE.INTERNAL, code: "INTERNAL", detail: String(e) };
}
