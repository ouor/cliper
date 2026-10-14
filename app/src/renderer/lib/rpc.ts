// Typed engine client on top of the preload bridge.
import type { Method, NotificationMethod, Notifications, ParamsOf, ResultOf } from "@ipc/protocol";

export class EngineError extends Error {
  constructor(
    message: string,
    /** engine ApiError code, e.g. BUSY, NETWORK, ENGINE_DOWN */
    public engineCode: string,
    public rpcCode: number,
    public method: string,
    /** details next to data.code: VIDEO_MISSING ranges / songs, NO_SPACE need / free, … */
    public data?: Record<string, unknown>,
  ) {
    super(message);
  }
}

export async function call<M extends Method>(method: M, params: ParamsOf<M>): Promise<ResultOf<M>> {
  const r = await window.cliper.call(method, params);
  if (r.ok) return r.result as ResultOf<M>;
  throw new EngineError(r.error.message, r.error.engineCode, r.error.code, method, r.error.data);
}

type Handler = (method: NotificationMethod, params: unknown) => void;
const handlers = new Set<Handler>();
let wired = false;

function wire() {
  if (wired) return;
  wired = true;
  window.cliper.onNotify((m, p) => {
    for (const h of handlers) {
      try {
        h(m as NotificationMethod, p);
      } catch (e) {
        console.error("notification handler failed", m, e);
      }
    }
  });
}

export function onAnyNotification(h: Handler): () => void {
  wire();
  handlers.add(h);
  return () => handlers.delete(h);
}

export function onNotification<N extends NotificationMethod>(name: N, cb: (p: Notifications[N]) => void): () => void {
  return onAnyNotification((m, p) => {
    if (m === name) cb(p as Notifications[N]);
  });
}
