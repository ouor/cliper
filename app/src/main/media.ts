// `cliper-media://` — the only way the renderer reads project media.
//
//   cliper-media://project/<projectId>/<rel>   whitelisted files below <projectsRoot>/<projectId>
//                                               (Range requests supported for <video> seeking)
//
// Whitelist: overview.peaks, probs.bin, cache/window_*.wav (engine `audio.window` results for the
// review preview), clips/<clip>/{source.mp4, orig.wav, vocals.wav, orig.peaks, vocals.peaks}.
// Anything else (other paths, `..`, absolute paths, other projects roots) is refused.
import { createReadStream, statSync } from "node:fs";
import { isAbsolute, relative, resolve, sep } from "node:path";
import { Readable } from "node:stream";

// one path segment: no separators, reserved characters or control chars (ids can contain
// non-ASCII, e.g. local recordings named in Korean); containment is re-checked after resolve
const ID_RE = /^(?!\.{1,2}$)[^\\\/:*?"<>|\u0000-\u001f]{1,160}$/u;
const REL_RE = /^(overview\.peaks|probs\.bin|cache\/window_[0-9_]{3,64}\.wav|clips\/[A-Za-z0-9_-]{1,64}\/(source\.mp4|orig\.wav|vocals\.wav|orig\.peaks|vocals\.peaks))$/;

const MIME: Record<string, string> = {
  mp4: "video/mp4",
  wav: "audio/wav",
  peaks: "application/octet-stream",
  bin: "application/octet-stream",
};

export interface MediaContext {
  projectsRoot: () => string | null;
}

function deny(status: number, msg: string): Response {
  return new Response(msg, { status, headers: { "content-type": "text/plain; charset=utf-8" } });
}

export function isProjectId(id: string): boolean {
  return ID_RE.test(id) && !id.includes("..");
}

/** Resolve a project-relative media path, or null if it is not allowed. */
export function resolveProjectFile(root: string, projectId: string, rel: string): string | null {
  if (!isProjectId(projectId)) return null;
  if (!REL_RE.test(rel)) return null;
  const base = resolve(root);
  const full = resolve(base, projectId, rel);
  const r = relative(base, full);
  if (!r || r.startsWith("..") || isAbsolute(r) || !full.startsWith(base + sep)) return null;
  return full;
}

async function serveFile(file: string, req: Request): Promise<Response> {
  let size: number;
  try {
    size = statSync(file).size;
  } catch {
    return deny(404, "not found");
  }
  const ext = file.split(".").pop() ?? "";
  const type = MIME[ext] ?? "application/octet-stream";
  const range = req.headers.get("range");
  const common = { "content-type": type, "accept-ranges": "bytes", "cache-control": "no-cache" };
  if (range) {
    const m = /bytes=(\d*)-(\d*)/.exec(range);
    if (m) {
      let start = m[1] ? parseInt(m[1], 10) : NaN;
      let end = m[2] ? parseInt(m[2], 10) : NaN;
      if (Number.isNaN(start)) {
        // suffix range
        start = Math.max(0, size - (Number.isNaN(end) ? 0 : end));
        end = size - 1;
      }
      if (Number.isNaN(end) || end >= size) end = size - 1;
      if (start >= size || start > end) {
        return new Response(null, { status: 416, headers: { ...common, "content-range": `bytes */${size}` } });
      }
      const stream = createReadStream(file, { start, end, highWaterMark: 1 << 20 });
      return new Response(Readable.toWeb(stream) as unknown as ReadableStream, {
        status: 206,
        headers: { ...common, "content-length": String(end - start + 1), "content-range": `bytes ${start}-${end}/${size}` },
      });
    }
  }
  const stream = createReadStream(file, { highWaterMark: 1 << 20 });
  return new Response(Readable.toWeb(stream) as unknown as ReadableStream, {
    status: 200,
    headers: { ...common, "content-length": String(size) },
  });
}

export function createMediaHandler(ctx: MediaContext) {
  return async (req: Request): Promise<Response> => {
    const root = ctx.projectsRoot();
    if (!root) return deny(503, "no projects root");
    let url: URL;
    try {
      url = new URL(req.url);
    } catch {
      return deny(400, "bad url");
    }
    const parts = decodeURIComponent(url.pathname).replace(/^\/+/, "").split("/");
    if (url.host === "project") {
      const [projectId, ...rest] = parts;
      const file = resolveProjectFile(root, projectId ?? "", rest.join("/"));
      if (!file) return deny(403, "forbidden");
      return serveFile(file, req);
    }
    return deny(403, "forbidden");
  };
}
