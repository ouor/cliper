import { Keyboard } from "lucide-react";
import { Modal } from "@/components/common";
import { useApp, openSettings } from "@/state/app";
import { KEY_ACTIONS, keysFor, prettyKey, SCOPE_LABEL, type KeyScope } from "@/lib/keys";

export function HelpOverlay() {
  const keymap = useApp((s) => s.settings.keymap);
  const close = () => useApp.setState({ overlay: null });
  const scopes: KeyScope[] = ["global", "play", "review", "talk"];
  return (
    <Modal
      title="단축키"
      icon={<Keyboard size={18} />}
      onClose={close}
      width={980}
      footer={
        <>
          <span className="faint" style={{ marginRight: "auto" }}>
            한글 입력 상태에서도 그대로 동작해요. 마우스 없이 한 방송을 끝낼 수 있어요.
          </span>
          <button className="btn subtle" onClick={() => openSettings("keys")}>
            단축키 바꾸기
          </button>
          <button className="btn primary" onClick={close}>
            닫기
          </button>
        </>
      }
    >
      <div className="help-grid">
        {scopes.map((sc) => (
          <section key={sc}>
            <h3>{SCOPE_LABEL[sc]}</h3>
            {KEY_ACTIONS.filter((a) => a.scope === sc).map((a) => (
              <div key={a.scope + a.id} className="help-row">
                <span className="grow">{a.label}</span>
                <span className="help-keys">
                  {keysFor(a.id, sc, keymap).map((k) => (
                    <kbd key={k}>{prettyKey(k)}</kbd>
                  ))}
                </span>
              </div>
            ))}
          </section>
        ))}
      </div>
      <p className="hint">※ E는 곡 검수에서 "끝 듣기", 멘트 다듬기에서 "지운 목소리"예요. 지도와 타임라인은 Ctrl+휠로 확대, 휠로 이동해요.</p>
    </Modal>
  );
}
