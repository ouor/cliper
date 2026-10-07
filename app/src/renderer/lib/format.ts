// Time/size formatting in the app's user language.

/** 2:01:44.2 / 4:21.7 (h omitted below one hour). `digits` = fractional digits. */
export function fmtTime(sec: number, digits = 1, forceHours = false): string {
  if (!Number.isFinite(sec)) return "--:--";
  const neg = sec < 0;
  let s = Math.abs(sec);
  const scale = 10 ** digits;
  s = Math.round(s * scale) / scale;
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const rest = s - h * 3600 - m * 60;
  const ss = digits > 0 ? rest.toFixed(digits).padStart(3 + digits, "0") : String(Math.floor(rest)).padStart(2, "0");
  const body = h > 0 || forceHours ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
  return (neg ? "-" : "") + body;
}

/** 4시간 2분 / 3분 38초 / 42초 */
export function fmtDurationKo(sec: number): string {
  if (!Number.isFinite(sec) || sec < 0) return "";
  const s = Math.round(sec);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const r = s % 60;
  if (h > 0) return m > 0 ? `${h}시간 ${m}분` : `${h}시간`;
  if (m > 0) return r > 0 && m < 10 ? `${m}분 ${r}초` : `${m}분`;
  return `${r}초`;
}

/** "약 40초" / "약 3분" for ETAs. */
export function fmtEta(sec: number): string {
  if (!Number.isFinite(sec) || sec < 0) return "";
  if (sec < 10) return "곧 끝나요";
  if (sec < 60) return `약 ${Math.round(sec / 5) * 5}초 남음`;
  if (sec < 3600) return `약 ${Math.round(sec / 60)}분 남음`;
  return `약 ${Math.floor(sec / 3600)}시간 ${Math.round((sec % 3600) / 60)}분 남음`;
}

export function fmtBytes(n: number): string {
  if (!Number.isFinite(n)) return "";
  if (n >= 1e12) return `${(n / 1e12).toFixed(1)}TB`;
  if (n >= 1e9) return `${(n / 1e9).toFixed(n >= 1e11 ? 0 : 1)}GB`;
  if (n >= 1e6) return `${Math.round(n / 1e6)}MB`;
  if (n >= 1e3) return `${Math.round(n / 1e3)}KB`;
  return `${n}B`;
}

/** Accepts 1:12:05, 72:05, 72:05.3, 4325, 4325.5 → seconds (null if invalid). */
export function parseTime(text: string): number | null {
  const t = text.trim().replace(/,/g, ".");
  if (!t) return null;
  if (!/^-?[\d:.]+$/.test(t)) return null;
  const parts = t.split(":");
  if (parts.length > 3) return null;
  let s = 0;
  for (const p of parts) {
    if (p === "" || Number.isNaN(Number(p))) return null;
    s = s * 60 + Number(p);
  }
  return s;
}

export function fmtDate(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso.replace(" ", "T"));
  if (Number.isNaN(d.getTime())) return iso;
  return `${d.getFullYear()}.${String(d.getMonth() + 1).padStart(2, "0")}.${String(d.getDate()).padStart(2, "0")}`;
}

export function fmtShortDate(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso.replace(" ", "T"));
  if (Number.isNaN(d.getTime())) return "";
  return `${String(d.getMonth() + 1).padStart(2, "0")}/${String(d.getDate()).padStart(2, "0")}`;
}

export function fmtRelative(iso: string | null | undefined): string {
  if (!iso) return "";
  const d = new Date(iso);
  const diff = (Date.now() - d.getTime()) / 1000;
  if (diff < 60) return "방금";
  if (diff < 3600) return `${Math.floor(diff / 60)}분 전`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}시간 전`;
  if (diff < 86400 * 7) return `${Math.floor(diff / 86400)}일 전`;
  return fmtDate(iso);
}

/** VOD timestamp as used in descriptions / chapters: 00:23:10 */
export function fmtStamp(sec: number): string {
  const s = Math.max(0, Math.floor(sec));
  return `${String(Math.floor(s / 3600)).padStart(2, "0")}:${String(Math.floor((s % 3600) / 60)).padStart(2, "0")}:${String(s % 60).padStart(2, "0")}`;
}

export function clamp(v: number, lo: number, hi: number): number {
  return v < lo ? lo : v > hi ? hi : v;
}
