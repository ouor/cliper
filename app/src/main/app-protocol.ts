// `app://cliper/…` — serves the built renderer (instead of file://) with the CSP as a
// response header. Only files inside the renderer output folder are served.
import { promises as fsp } from "node:fs";
import { extname, join, resolve } from "node:path";
import { isInside } from "./media";

export const APP_ORIGIN = "app://cliper";

const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".woff2": "font/woff2",
  ".woff": "font/woff",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".json": "application/json",
};

export const CSP = [
  "default-src 'self'",
  "script-src 'self'",
  "img-src 'self' data: blob: https: cliper-media:",
  "media-src 'self' blob: cliper-media:",
  "font-src 'self' data:",
  "style-src 'self' 'unsafe-inline'",
  "connect-src 'self' cliper-media:",
  "worker-src 'self' blob:",
  "object-src 'none'",
  "base-uri 'none'",
  "frame-ancestors 'none'",
].join("; ");

export function createAppHandler(rendererDir: string) {
  const base = resolve(rendererDir);
  return async (req: Request): Promise<Response> => {
    let url: URL;
    try {
      url = new URL(req.url);
    } catch {
      return new Response("bad url", { status: 400 });
    }
    if (url.host !== "cliper") return new Response("not found", { status: 404 });
    let rel: string;
    try {
      rel = decodeURIComponent(url.pathname).replace(/^\/+/, "") || "index.html";
    } catch {
      return new Response("bad url", { status: 400 });
    }
    const file = join(base, rel);
    if (!isInside(base, file)) return new Response("forbidden", { status: 403 });
    try {
      const body = await fsp.readFile(file);
      const headers: Record<string, string> = { "content-type": TYPES[extname(file).toLowerCase()] ?? "application/octet-stream" };
      if (file.endsWith(".html")) headers["content-security-policy"] = CSP;
      return new Response(new Uint8Array(body), { status: 200, headers });
    } catch {
      return new Response("not found", { status: 404 });
    }
  };
}
