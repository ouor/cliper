// S8 로그인 쿠키 (고급). 비밀번호는 받지 않고 쿠키만 받아요. 값은 엔진에만 전달하고 앱 설정에는 저장 날짜만 남겨요.
import { useState } from "react";
import { AlertTriangle, Cookie, FileUp, Lock } from "lucide-react";
import { Modal } from "@/components/common";
import { toast, updateSettings, useApp } from "@/state/app";
import { call } from "@/lib/rpc";
import { friendly } from "@/lib/errors";
import { fmtDate } from "@/lib/format";

/** "NID_AUT=…; NID_SES=…", Netscape cookies.txt, or JSON export → name/value map. */
export function parseCookies(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  const t = text.trim();
  if (t.startsWith("[") || t.startsWith("{")) {
    try {
      const j = JSON.parse(t);
      const arr = Array.isArray(j) ? j : Object.entries(j).map(([name, value]) => ({ name, value }));
      for (const c of arr) if (c && typeof c.name === "string" && typeof c.value === "string") out[c.name] = c.value;
      return out;
    } catch {
      /* fall through */
    }
  }
  for (const line of t.split(/\r?\n/)) {
    const cols = line.split("\t");
    if (cols.length >= 7 && !line.startsWith("#")) {
      out[cols[5]] = cols[6];
      continue;
    }
    for (const part of line.split(";")) {
      const i = part.indexOf("=");
      if (i > 0) out[part.slice(0, i).trim()] = part.slice(i + 1).trim();
    }
  }
  return out;
}

const NEEDED: Record<string, string[]> = { chzzk: ["NID_AUT", "NID_SES"], soop: [] };

export function CookiesDialog() {
  const saved = useApp((s) => s.settings.cookies);
  const [platform, setPlatform] = useState<"chzzk" | "soop">(saved?.platform ?? "chzzk");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const close = () => useApp.setState({ overlay: null });
  const parsed = parseCookies(text);
  const missing = NEEDED[platform].filter((k) => !parsed[k]);
  const count = Object.keys(parsed).length;

  const save = async () => {
    setBusy(true);
    try {
      const cookies = platform === "chzzk" ? Object.fromEntries(Object.entries(parsed).filter(([k]) => NEEDED.chzzk.includes(k))) : parsed;
      await call("system.set_cookies", { cookies });
      await updateSettings({ cookies: { platform, savedAt: new Date().toISOString() } });
      toast("로그인 쿠키를 넣었어요. 이 PC의 처리 엔진에서만 써요.", { kind: "ok" });
      close();
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    } finally {
      setBusy(false);
    }
  };
  const clear = async () => {
    try {
      await call("system.set_cookies", { cookies: null });
      await updateSettings({ cookies: null });
      toast("로그인 쿠키를 지웠어요.", { kind: "ok" });
      close();
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    }
  };

  return (
    <Modal
      title="로그인 쿠키 (고급)"
      icon={<Cookie size={18} />}
      onClose={close}
      width={600}
      footer={
        <>
          {saved && (
            <button className="btn ghost" style={{ marginRight: "auto" }} onClick={() => void clear()}>
              쿠키 지우기
            </button>
          )}
          <button className="btn ghost" onClick={close}>
            취소
          </button>
          <button className="btn primary" onClick={() => void save()} disabled={busy || count === 0 || missing.length > 0}>
            저장
          </button>
        </>
      }
    >
      <p className="muted">구독자 전용이나 비공개 다시보기를 받을 때만 필요해요. 비밀번호는 받지 않아요.</p>
      <div className="row">
        <span className="label" style={{ width: 52 }}>
          플랫폼
        </span>
        <label className="check">
          <input type="radio" checked={platform === "chzzk"} onChange={() => setPlatform("chzzk")} /> 치지직
        </label>
        <label className="check">
          <input type="radio" checked={platform === "soop"} onChange={() => setPlatform("soop")} /> SOOP
        </label>
      </div>
      <div className="row">
        <span className="label" style={{ width: 52 }}>
          쿠키
        </span>
        <button
          className="btn sm subtle"
          onClick={async () => {
            const p = await window.cliper.pickFile({ title: "쿠키 파일", filters: [{ name: "쿠키", extensions: ["txt", "json"] }] });
            if (p) setText(await window.cliper.readTextFile(p));
          }}
        >
          <FileUp size={13} /> 파일에서 불러오기…
        </button>
        <span className="faint">또는 아래에 붙여넣기</span>
      </div>
      <textarea className="textarea mono" rows={4} placeholder={platform === "chzzk" ? "NID_AUT=…; NID_SES=…" : "브라우저에서 내보낸 쿠키"} value={text} onChange={(e) => setText(e.target.value)} onKeyDown={(e) => e.stopPropagation()} />
      {text && <div className={`hint${missing.length ? " err-text" : ""}`}>{missing.length ? `${missing.join(", ")} 값이 없어요.` : `쿠키 ${count}개를 읽었어요.`}</div>}
      <div className="banner info">
        <Lock size={14} />
        <div>
          쿠키는 이 PC의 처리 엔진에만 전달되고 외부로 보내지 않아요. [쿠키 지우기]로 언제든 지울 수 있어요.
          <div className="faint">앱을 다시 켜면 쿠키를 다시 넣어야 해요.</div>
        </div>
      </div>
      <div className="banner warn">
        <AlertTriangle size={14} />
        <div>
          구독자 전용 콘텐츠의 클립은 많은 스트리머가 금지해요. 스트리머의 가이드라인을 꼭 확인하세요.
          <div className="faint">플랫폼 약관에 따라 계정에 불이익이 있을 수 있어요.</div>
        </div>
      </div>
      <div className="hint">상태: {saved ? `설정됨 · ${fmtDate(saved.savedAt)} 저장 (${saved.platform === "chzzk" ? "치지직" : "SOOP"})` : "없음"}</div>
    </Modal>
  );
}
