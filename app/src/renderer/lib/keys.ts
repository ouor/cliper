// Keyboard shortcuts (기획서 7.6). Combos are derived from KeyboardEvent.code so they keep
// working while the Korean IME is on (e.key would be "ㄴ" instead of "S").

export type KeyScope = "global" | "play" | "review" | "talk";

export interface KeyAction {
  id: string;
  scope: KeyScope;
  keys: string[]; // default combos
  label: string;
}

export const KEY_ACTIONS: KeyAction[] = [
  { id: "mode.review", scope: "global", keys: ["1"], label: "곡 검수 화면" },
  { id: "mode.talk", scope: "global", keys: ["2"], label: "멘트 다듬기 화면" },
  { id: "mode.export", scope: "global", keys: ["3"], label: "내보내기 화면" },
  { id: "jobs", scope: "global", keys: ["Ctrl+J"], label: "작업 현황 패널" },
  { id: "undo", scope: "global", keys: ["Ctrl+Z"], label: "되돌리기" },
  { id: "redo", scope: "global", keys: ["Ctrl+Shift+Z", "Ctrl+Y"], label: "다시 하기" },
  { id: "settings", scope: "global", keys: ["Ctrl+,"], label: "설정" },
  { id: "help", scope: "global", keys: ["?"], label: "단축키 도움말" },
  { id: "search", scope: "global", keys: ["Ctrl+F"], label: "곡 목록 검색" },

  { id: "play", scope: "play", keys: ["Space"], label: "재생 / 정지" },
  { id: "shuttle.back", scope: "play", keys: ["J"], label: "뒤로 재생 (반복 시 배속)" },
  { id: "shuttle.stop", scope: "play", keys: ["K"], label: "정지" },
  { id: "shuttle.fwd", scope: "play", keys: ["L"], label: "앞으로 재생 (반복 시 배속)" },
  { id: "seek.back", scope: "play", keys: ["ArrowLeft"], label: "0.5초 뒤로" },
  { id: "seek.fwd", scope: "play", keys: ["ArrowRight"], label: "0.5초 앞으로" },
  { id: "seek.back2", scope: "play", keys: ["Shift+ArrowLeft"], label: "2초 뒤로" },
  { id: "seek.fwd2", scope: "play", keys: ["Shift+ArrowRight"], label: "2초 앞으로" },
  { id: "seek.fineBack", scope: "play", keys: [","], label: "0.1초 뒤로" },
  { id: "seek.fineFwd", scope: "play", keys: ["."], label: "0.1초 앞으로" },
  { id: "loop", scope: "play", keys: ["Ctrl+L"], label: "구간 반복 켜기/끄기" },

  { id: "song.prev", scope: "review", keys: ["ArrowUp"], label: "이전 곡" },
  { id: "song.next", scope: "review", keys: ["ArrowDown"], label: "다음 곡" },
  { id: "song.nextUnreviewed", scope: "review", keys: ["N"], label: "다음 미확인 곡" },
  { id: "listen.start", scope: "review", keys: ["S"], label: "시작 듣기 (시작 5초 전부터)" },
  { id: "listen.end", scope: "review", keys: ["E"], label: "끝 듣기 (끝 5초 전부터)" },
  { id: "edge.in", scope: "review", keys: ["I"], label: "현재 위치를 곡 시작으로" },
  { id: "edge.out", scope: "review", keys: ["O"], label: "현재 위치를 곡 끝으로" },
  { id: "edge.back", scope: "review", keys: ["Alt+ArrowLeft"], label: "마지막에 고친 경계를 0.1초 앞으로" },
  { id: "edge.fwd", scope: "review", keys: ["Alt+ArrowRight"], label: "마지막에 고친 경계를 0.1초 뒤로" },
  { id: "song.title", scope: "review", keys: ["T"], label: "제목 입력" },
  { id: "song.note", scope: "review", keys: ["M"], label: "메모 입력" },
  { id: "song.confirm", scope: "review", keys: ["Enter"], label: "확인 완료 후 다음 미확인 곡" },
  { id: "song.exclude", scope: "review", keys: ["X"], label: "제외 / 복원" },
  { id: "song.add", scope: "review", keys: ["A"], label: "곡 추가 (재생 위치에서 3분, 또는 선택한 후보 구간)" },
  { id: "song.split", scope: "review", keys: ["/"], label: "현재 위치에서 나누기" },
  { id: "song.merge", scope: "review", keys: ["G"], label: "선택한 곡들 합치기" },
  { id: "song.delete", scope: "review", keys: ["Delete"], label: "곡 삭제" },
  { id: "song.finish", scope: "review", keys: ["Ctrl+Shift+E"], label: "이 곡만 완성 (영상 받기 → 나누기 → 제안)" },

  { id: "listen.orig", scope: "talk", keys: ["Q"], label: "원본 듣기" },
  { id: "listen.result", scope: "talk", keys: ["W"], label: "결과 듣기" },
  { id: "listen.removed", scope: "talk", keys: ["E"], label: "지운 목소리 듣기" },
  { id: "listen.acc", scope: "talk", keys: ["R"], label: "반주 듣기" },
  { id: "mute.next", scope: "talk", keys: ["Tab"], label: "다음 멘트 구간 (선택 시 반복 재생)" },
  { id: "mute.prev", scope: "talk", keys: ["Shift+Tab"], label: "이전 멘트 구간" },
  { id: "mute.confirm", scope: "talk", keys: ["Enter"], label: "선택 구간 적용 확정" },
  { id: "mute.delete", scope: "talk", keys: ["Delete"], label: "선택 구간 삭제" },
  { id: "mute.mark", scope: "talk", keys: ["B"], label: "재생 위치에 새 구간 시작 / 끝" },
  { id: "mute.startHere", scope: "talk", keys: ["["], label: "선택 구간 시작을 재생 위치로" },
  { id: "mute.endHere", scope: "talk", keys: ["]"], label: "선택 구간 끝을 재생 위치로" },
  { id: "mute.split", scope: "talk", keys: ["/"], label: "선택 구간을 재생 위치에서 나누기" },
  { id: "song.endHere", scope: "talk", keys: ["F"], label: "여기서 곡 끝내기" },
  { id: "talk.nextSong", scope: "talk", keys: ["Ctrl+ArrowDown"], label: "다음 곡" },
  { id: "talk.prevSong", scope: "talk", keys: ["Ctrl+ArrowUp"], label: "이전 곡" },
  { id: "song.finish", scope: "talk", keys: ["Ctrl+Shift+E"], label: "이 곡 받기 (영상 받기 → 나누기 → 제안)" },
];

const CODE_KEY: Record<string, string> = {
  Space: "Space",
  Enter: "Enter",
  NumpadEnter: "Enter",
  Escape: "Escape",
  Tab: "Tab",
  Delete: "Delete",
  Backspace: "Backspace",
  ArrowLeft: "ArrowLeft",
  ArrowRight: "ArrowRight",
  ArrowUp: "ArrowUp",
  ArrowDown: "ArrowDown",
  Comma: ",",
  Period: ".",
  Slash: "/",
  BracketLeft: "[",
  BracketRight: "]",
  Minus: "-",
  Equal: "=",
  Home: "Home",
  End: "End",
};

/** "Ctrl+Shift+E", "?", "Space"… (null for bare modifiers). */
export function comboFromEvent(e: KeyboardEvent): string | null {
  const code = e.code;
  let key: string | undefined;
  if (/^Key[A-Z]$/.test(code)) key = code.slice(3);
  else if (/^Digit\d$/.test(code)) key = code.slice(5);
  else if (/^Numpad\d$/.test(code)) key = code.slice(6);
  else if (/^F\d{1,2}$/.test(code)) key = code;
  else key = CODE_KEY[code];
  if (!key) return null;
  let shift = e.shiftKey;
  if (key === "/" && shift) {
    key = "?";
    shift = false;
  }
  const mods: string[] = [];
  if (e.ctrlKey || e.metaKey) mods.push("Ctrl");
  if (e.altKey) mods.push("Alt");
  if (shift) mods.push("Shift");
  return [...mods, key].join("+");
}

export function bindings(overrides: Record<string, string>): Map<string, KeyAction[]> {
  // combo → actions (several scopes may share a key)
  const out = new Map<string, KeyAction[]>();
  for (const a of KEY_ACTIONS) {
    const ov = overrides[`${a.scope}:${a.id}`];
    const keys = ov ? [ov] : a.keys;
    for (const k of keys) out.set(k, [...(out.get(k) ?? []), a]);
  }
  return out;
}

export function keysFor(id: string, scope: KeyScope | undefined, overrides: Record<string, string>): string[] {
  const a = KEY_ACTIONS.find((x) => x.id === id && (!scope || x.scope === scope));
  if (!a) return [];
  const ov = overrides[`${a.scope}:${a.id}`];
  return ov ? [ov] : a.keys;
}

export function prettyKey(k: string): string {
  return k
    .replace("ArrowLeft", "←")
    .replace("ArrowRight", "→")
    .replace("ArrowUp", "↑")
    .replace("ArrowDown", "↓")
    .replace("Delete", "Del")
    .replace("Enter", "↵")
    .replace("Space", "Space");
}

export const SCOPE_LABEL: Record<KeyScope, string> = { global: "전역", play: "재생", review: "곡 검수", talk: "멘트 다듬기" };
