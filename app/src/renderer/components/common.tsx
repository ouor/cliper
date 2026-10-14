import { useEffect, useId, useRef, type ReactNode } from "react";
import { AlertTriangle, CheckCircle2, HelpCircle, Info, VideoOff, X, XCircle } from "lucide-react";
import { dismissToast, useApp } from "@/state/app";
import { usePlayer } from "@/audio/player";

export function Kbd({ k }: { k: string }) {
  return <kbd>{k}</kbd>;
}

/**
 * Covers a `.video-box` while the playhead is outside the bound clip, so a frame from another
 * time is never shown (#1). Renders nothing otherwise.
 */
export function VideoOutside() {
  const outside = usePlayer((s) => s.videoOutside);
  if (!outside) return null;
  return (
    <div className="video-outside" role="status">
      <VideoOff size={18} />
      <span>이 위치의 영상은 받지 않았어요</span>
    </div>
  );
}

export function Progress({ value, indeterminate, tone }: { value?: number | null; indeterminate?: boolean; tone?: "accent" | "warn" | "danger" }) {
  const ind = indeterminate || value === null || value === undefined;
  const color = tone === "warn" ? "var(--doubt)" : tone === "danger" ? "var(--danger)" : undefined;
  return (
    <div className={`progress${ind ? " indeterminate" : ""}`}>
      <i style={{ width: ind ? undefined : `${Math.round((value ?? 0) * 1000) / 10}%`, background: color }} />
    </div>
  );
}

export function Modal({ title, icon, children, footer, onClose, width, className }: { title: ReactNode; icon?: ReactNode; children: ReactNode; footer?: ReactNode; onClose?: () => void; width?: number; className?: string }) {
  const ref = useRef<HTMLDivElement>(null);
  const titleId = useId();
  useEffect(() => {
    const prev = document.activeElement as HTMLElement | null;
    const first = ref.current?.querySelector<HTMLElement>("[autofocus], input, textarea, select, button.primary, button");
    first?.focus();
    return () => prev?.focus?.();
  }, []);
  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onClose?.();
      }}
      onKeyDown={(e) => {
        if (e.key === "Escape" && onClose) {
          e.stopPropagation();
          onClose();
        }
        if (e.key === "Tab" && ref.current) {
          // keep focus inside the dialog
          const items = [...ref.current.querySelectorAll<HTMLElement>("button, input, textarea, select, [tabindex]:not([tabindex='-1'])")].filter((x) => !(x as HTMLButtonElement).disabled);
          if (items.length) {
            const first = items[0];
            const last = items[items.length - 1];
            if (e.shiftKey && document.activeElement === first) {
              e.preventDefault();
              last.focus();
            } else if (!e.shiftKey && document.activeElement === last) {
              e.preventDefault();
              first.focus();
            }
          }
        }
        e.stopPropagation();
      }}
    >
      <div className={`modal ${className ?? ""}`} ref={ref} style={width ? { width: `min(${width}px, calc(100vw - 48px))` } : undefined} role="dialog" aria-modal="true" aria-labelledby={titleId}>
        <header>
          {icon}
          <span className="grow" id={titleId}>
            {title}
          </span>
          {onClose && (
            <button className="btn ghost icon sm" onClick={onClose} aria-label="닫기">
              <X size={16} />
            </button>
          )}
        </header>
        <div className="body">{children}</div>
        {footer && <footer>{footer}</footer>}
      </div>
    </div>
  );
}

const DIALOG_ICON = {
  warn: <AlertTriangle size={20} color="var(--doubt)" />,
  info: <Info size={20} color="var(--info)" />,
  error: <XCircle size={20} color="var(--danger)" />,
  question: <HelpCircle size={20} color="var(--accent)" />,
};

export function DialogHost() {
  const d = useApp((s) => s.dialog);
  if (!d) return null;
  const cancel = d.buttons.find((b) => b.id === "cancel") ?? d.buttons[0];
  return (
    <Modal
      title={d.title}
      icon={d.icon ? DIALOG_ICON[d.icon] : undefined}
      onClose={() => d.resolve(cancel.id)}
      footer={d.buttons.map((b) => (
        <button key={b.id} className={`btn ${b.kind ?? "subtle"}`} onClick={() => d.resolve(b.id)} autoFocus={b.kind === "primary"}>
          {b.label}
          {b.hotkey && <kbd className="kbd-hint">{b.hotkey}</kbd>}
        </button>
      ))}
    >
      {Array.isArray(d.body) ? d.body.map((l, i) => <p key={i} className={i ? "muted" : undefined}>{l}</p>) : d.body && <p>{d.body}</p>}
      {d.detail && (
        <details>
          <summary>자세히</summary>
          <pre className="detail">{d.detail}</pre>
        </details>
      )}
    </Modal>
  );
}

const TOAST_ICON = {
  ok: <CheckCircle2 size={16} />,
  info: <Info size={16} />,
  warn: <AlertTriangle size={16} />,
  error: <XCircle size={16} />,
};

export function Toasts() {
  const toasts = useApp((s) => s.toasts);
  return (
    <div className="toasts" aria-live="polite">
      {toasts.map((t) => (
        <div key={t.id} className={`toast ${t.kind}`} role={t.kind === "error" ? "alert" : "status"}>
          {TOAST_ICON[t.kind]}
          <span className="grow">{t.text}</span>
          {t.action && (
            <button
              className="btn sm subtle"
              onClick={() => {
                t.action!.run();
                dismissToast(t.id);
              }}
            >
              {t.action.label}
            </button>
          )}
          <button className="btn ghost icon xs" onClick={() => dismissToast(t.id)} aria-label="닫기">
            <X size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}

export function Empty({ icon, title, children }: { icon?: ReactNode; title: string; children?: ReactNode }) {
  return (
    <div className="empty">
      {icon && <div className="empty-icon">{icon}</div>}
      <div className="empty-title">{title}</div>
      {children && <div className="empty-body">{children}</div>}
    </div>
  );
}

/** Keyboard-safe check: true when typing into a field. */
export function isTyping(e: KeyboardEvent): boolean {
  const t = e.target as HTMLElement | null;
  if (!t) return false;
  const tag = t.tagName;
  if (tag === "INPUT") {
    const type = (t as HTMLInputElement).type;
    return !["checkbox", "radio", "range", "button", "submit"].includes(type);
  }
  return tag === "TEXTAREA" || tag === "SELECT" || t.isContentEditable;
}
