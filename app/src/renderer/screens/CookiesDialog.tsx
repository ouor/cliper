// S8 로그인 쿠키 (고급). 비밀번호는 받지 않고 쿠키만 받아요. 값은 엔진에만 전달하고 앱 설정에는 저장 날짜만 남겨요.
import { useEffect, useState } from "react";
import { AlertTriangle, Cookie, FileUp, Lock } from "lucide-react";
import { Modal } from "@/components/common";
import { toast, useApp } from "@/state/app";
import type { CookieStatus } from "@ipc/generated/CookieStatus";
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

function statusLine(st: CookieStatus | null): string {
  if (!st) return "확인 중…";
  if (!st.set) return "없음";
  const parts = [`설정됨 · ${st.saved_at ? fmtDate(st.saved_at) + " 저장" : ""}`.replace(/ · $/, "")];
  parts.push(st.encrypted ? "이 Windows 계정으로 암호화해 보관" : "암호화되지 않음");
  if (st.valid === true) parts.push(`로그인 확인됨${st.account ? ` (${st.account})` : ""}${st.checked_at ? ` · ${fmtDate(st.checked_at)}` : ""}`);
  else if (st.valid === false) parts.push("로그인이 만료된 것 같아요. 브라우저에서 새로 받아 넣어 주세요");
  else if (st.checked_at) parts.push("이 플랫폼은 로그인 여부를 확인할 수 없어요");
  return parts.join(" · ");
}

export function CookiesDialog() {
  const [status, setStatusRaw] = useState<CookieStatus | null>(null);
  const setStatus = (st: CookieStatus) => {
    setStatusRaw(st);
    const sys = useApp.getState().system;
    if (sys) useApp.setState({ system: { ...sys, cookies: st } });
  };
  const [platform, setPlatform] = useState<"chzzk" | "soop">("chzzk");
  const [text, setText] = useState("");
  const [busy, setBusy] = useState(false);
  const close = () => useApp.setState({ overlay: null });
  const parsed = parseCookies(text);
  const missing = NEEDED[platform].filter((k) => !parsed[k]);
  const count = Object.keys(parsed).length;

  useEffect(() => {
    void call("system.cookies_status", {})
      .then((st) => {
        setStatus(st);
        if (st.platforms.includes("soop") && !st.platforms.includes("chzzk")) setPlatform("soop");
      })
      .catch(() => {});
  }, []);

  const save = async () => {
    setBusy(true);
    try {
      const cookies = platform === "chzzk" ? Object.fromEntries(Object.entries(parsed).filter(([k]) => NEEDED.chzzk.includes(k))) : parsed;
      setStatus(await call("system.set_cookies", { cookies }));
      setText("");
      toast("로그인 쿠키를 넣었어요. 이 PC에만 암호화해서 보관해요.", { kind: "ok" });
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    } finally {
      setBusy(false);
    }
  };
  const check = async () => {
    setBusy(true);
    try {
      const st = await call("system.check_cookies", {});
      setStatus(st);
      toast(st.valid === true ? `로그인되어 있어요${st.account ? ` (${st.account})` : ""}.` : st.valid === false ? "로그인 쿠키가 만료된 것 같아요." : "이 플랫폼은 로그인 여부를 확인할 수 없어요.", { kind: st.valid === false ? "warn" : "info" });
    } catch (e) {
      toast(friendly(e).title, { kind: "error" });
    } finally {
      setBusy(false);
    }
  };
  const clear = async () => {
    try {
      setStatus(await call("system.set_cookies", { cookies: null }));
      toast("로그인 쿠키를 지웠어요.", { kind: "ok" });
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
          {status?.set && (
            <button className="btn ghost" style={{ marginRight: "auto" }} onClick={() => void clear()}>
              쿠키 지우기
            </button>
          )}
          <button className="btn ghost" onClick={close}>
            닫기
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
          쿠키는 이 PC의 처리 엔진에만 전달되고, 이 Windows 계정으로 암호화해서 보관해요. 외부로 보내지 않아요. 앱을 다시 켜도 그대로 남고, [쿠키 지우기]로 언제든 지울 수 있어요.
        </div>
      </div>
      <div className="banner warn">
        <AlertTriangle size={14} />
        <div>
          구독자 전용 콘텐츠의 클립은 많은 스트리머가 금지해요. 스트리머의 가이드라인을 꼭 확인하세요.
          <div className="faint">플랫폼 약관에 따라 계정에 불이익이 있을 수 있어요.</div>
        </div>
      </div>
      <div className="row cookie-status">
        <span className="hint grow">상태: {statusLine(status)}</span>
        {status?.set && (
          <button className="btn sm subtle" onClick={() => void check()} disabled={busy}>
            지금 확인
          </button>
        )}
      </div>
    </Modal>
  );
}
