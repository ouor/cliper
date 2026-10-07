// VOD link recognition on the app side.
//
// The engine (`project.create` → UNRECOGNIZED_INPUT) is the source of truth for what is a valid
// input. The patterns here only serve two non-blocking purposes:
//   1. pick candidate links out of a multi-line paste (so a pasted setlist or chat log queues
//      only the VOD links, not every URL in it);
//   2. a soft hint under the link box while typing.
// A single typed/pasted input is always sent to the engine, even when nothing here matches.
//
// Keep in sync with core/crates/cliper-core/src/platforms/{soop,chzzk}.rs `matches`
// (soop is checked first; chzzk catches bare numbers):
//   soop:  (sooplive.com|sooplive.co.kr|afreecatv.com)/player/<digits>
//          vod.(same hosts)/…<6+ digits>
//          soop:<digits>
//   chzzk: chzzk.naver.com/video/<digits>
//          <digits> or chzzk:<digits>

const SOOP_HOST = String.raw`(?:sooplive\.com|sooplive\.co\.kr|afreecatv\.com)`;
const SOOP_RES = [new RegExp(`${SOOP_HOST}/player/\\d+`, "i"), new RegExp(`vod\\.${SOOP_HOST}/.*?\\d{6,}`, "i"), /^soop:\s*\d+\s*$/i];
const CHZZK_RES = [/chzzk\.naver\.com\/video\/\d+/i, /^(?:chzzk:)?\s*\d+\s*$/i];

export type InputKind = "soop" | "chzzk" | "local" | null;

export function isLocalPath(s: string): boolean {
  const t = s.trim().replace(/^"|"$/g, "");
  return /^[a-zA-Z]:[\\/]/.test(t) || t.startsWith("\\\\");
}

/** Which platform the engine would pick for this input, or null if it probably won't recognize it. */
export function inputKind(input: string): InputKind {
  const s = input.trim();
  if (!s) return null;
  if (isLocalPath(s)) return "local";
  if (SOOP_RES.some((re) => re.test(s))) return "soop";
  if (CHZZK_RES.some((re) => re.test(s))) return "chzzk";
  return null;
}

/**
 * VOD inputs found in pasted text. Tokens that look like VOD links are picked out (plus bare
 * 6–10 digit chzzk numbers). A single line that recognizes as a whole, or in which nothing is
 * picked out, is returned as-is so the engine decides — the app never rejects it on its own.
 */
export function extractInputs(text: string): string[] {
  const lines = text.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
  if (lines.length === 0) return [];
  if (lines.length === 1 && inputKind(lines[0])) return [lines[0]];
  const found = new Set<string>();
  for (const raw of text.split(/\s+/)) {
    const tok = raw.replace(/^[(<["'「『]+/, "").replace(/[)>\]"'」』,.;!?]+$/, "");
    if (!tok) continue;
    const kind = inputKind(tok);
    if (kind === "soop" || (kind === "chzzk" && (/[a-z]/i.test(tok) || /^\d{6,10}$/.test(tok)))) found.add(tok);
  }
  if (found.size === 0 && lines.length === 1) return [lines[0]];
  return [...found];
}
