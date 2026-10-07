// `cliper-media://` — the only way the renderer reads project media.
//
//   cliper-media://project/<projectId>/<rel>   whitelisted files below <projectsRoot>/<projectId>
//                                               (Range requests supported for <video> seeking)
//   cliper-media://pcm/<projectId>?t=<s>&d=<s>   VOD audio window decoded to f32le stereo 48 kHz
//
// Anything else (other paths, `..`, absolute paths, other projects roots) is refused.
import { spawn } from "node:child_process";
import { createReadStream, existsSync, promises as fsp, statSync } from "node:fs";
import { isAbsolute, join, relative, resolve, sep } from "node:path";
import { Readable } from "node:stream";

// one path segment: no separators, reserved characters or control chars (ids can contain
// non-ASCII, e.g. local recordings named in Korean); containment is re-checked after resolve
const ID_RE = /^(?!\.{1,2}$)[^\\\/:*?"<>|\u0000-\u001f]{1,160}$/u;
const REL_RE = /^(overview\.peaks|probs\.bin|clips\/[A-Za-z0-9_-]{1,64}\/(source\.mp4|orig\.wav|vocals\.wav|orig\.peaks|vocals\.peaks))$/;

const MIME: Record<string, string> = {
  mp4: "video/mp4",
  wav: "audio/wav",
  peaks: "application/octet-stream",
  bin: "application/octet-stream",
};

export interface MediaContext {
  projectsRoot: () => string | null;
  ffmpeg: () => string;
}

function deny(status: number, msg: string): Response {
  return new Response(msg, { status, headers: { "content-type": "text/plain; charset=utf-8" } });
}

/** Resolve a project-relative media path, or null if it is not allowed. */
export function isProjectId(id: string): boolean {
  return ID_RE.test(id) && !id.includes("..");
}

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

// ── ADTS frame index: VOD time ↔ AAC frame (the engine decodes audio.aac
// sequentially, so VOD time = decoded sample index / sample rate). ──

interface AdtsIndex {
  mtimeMs: number;
  size: number;
  offsets: Float64Array; // byte offset of each frame
  count: number;
  sampleRate: number;
}

const SR_TABLE = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];
const adtsCache = new Map<string, AdtsIndex>();
const adtsBuilding = new Map<string, Promise<AdtsIndex>>();

async function buildAdtsIndex(file: string): Promise<AdtsIndex> {
  const st = statSync(file);
  const cached = adtsCache.get(file);
  if (cached && cached.mtimeMs === st.mtimeMs && cached.size === st.size) return cached;
  const inflight = adtsBuilding.get(file);
  if (inflight) return inflight;
  const p = (async () => {
    try {
      const buf = await fsp.readFile(file);
      let offsets = new Float64Array(1 << 16);
      let count = 0;
      let sampleRate = 48000;
      let i = 0;
      const n = buf.length;
      while (i + 7 <= n) {
        if (buf[i] === 0xff && (buf[i + 1] & 0xf6) === 0xf0) {
          const len = ((buf[i + 3] & 0x03) << 11) | (buf[i + 4] << 3) | (buf[i + 5] >> 5);
          if (len >= 7) {
            if (count === 0) sampleRate = SR_TABLE[(buf[i + 2] >> 2) & 0x0f] ?? 48000;
            if (count >= offsets.length) {
              const grown = new Float64Array(offsets.length * 2);
              grown.set(offsets);
              offsets = grown;
            }
            offsets[count++] = i;
            i += len;
            continue;
          }
        }
        i++; // resync
      }
      const idx: AdtsIndex = { mtimeMs: st.mtimeMs, size: st.size, offsets: offsets.slice(0, count), count, sampleRate };
      adtsCache.set(file, idx);
      return idx;
    } finally {
      adtsBuilding.delete(file);
    }
  })();
  adtsBuilding.set(file, p);
  return p;
}

function runFfmpeg(ffmpeg: string, args: string[], input?: Buffer): Promise<Buffer> {
  return new Promise((res, rej) => {
    const ch = spawn(ffmpeg, args, { windowsHide: true, stdio: ["pipe", "pipe", "pipe"] });
    const out: Buffer[] = [];
    let err = "";
    ch.stdout.on("data", (b: Buffer) => out.push(b));
    ch.stderr.on("data", (b: Buffer) => (err += b.toString()));
    ch.on("error", rej);
    ch.on("close", (code) => {
      if (code === 0 || out.length) res(Buffer.concat(out));
      else rej(new Error(`ffmpeg ${code}: ${err.slice(-400)}`));
    });
    ch.stdin.on("error", () => {
      /* ffmpeg may close stdin early */
    });
    if (input) ch.stdin.end(input);
    else ch.stdin.end();
  });
}

const OUT_SR = 48000;

async function decodeWindow(ctx: MediaContext, root: string, projectId: string, t: number, d: number): Promise<Buffer> {
  const dir = join(root, projectId);
  const aac = join(dir, "audio.aac");
  const mka = join(dir, "audio.mka");
  const wantFrames = Math.round(d * OUT_SR);
  if (existsSync(aac)) {
    const idx = await buildAdtsIndex(aac);
    const spf = 1024;
    const first = Math.floor((t * idx.sampleRate) / spf);
    const pre = Math.min(2, first); // decoder warm-up frames
    const k0 = first - pre;
    const k1 = Math.min(idx.count, Math.ceil(((t + d) * idx.sampleRate) / spf) + 2);
    if (k0 >= idx.count || k1 <= k0) return Buffer.alloc(0);
    const startByte = idx.offsets[k0];
    const endByte = k1 < idx.count ? idx.offsets[k1] : idx.size;
    const fh = await fsp.open(aac, "r");
    let slice: Buffer;
    try {
      slice = Buffer.alloc(endByte - startByte);
      await fh.read(slice, 0, slice.length, startByte);
    } finally {
      await fh.close();
    }
    const pcm = await runFfmpeg(ctx.ffmpeg(), ["-hide_banner", "-loglevel", "error", "-f", "aac", "-i", "pipe:0", "-ac", "2", "-ar", String(OUT_SR), "-f", "f32le", "pipe:1"], slice);
    const dropSec = t - (k0 * spf) / idx.sampleRate;
    const drop = Math.max(0, Math.round(dropSec * OUT_SR)) * 8;
    return pcm.subarray(drop, Math.min(pcm.length, drop + wantFrames * 8));
  }
  if (existsSync(mka)) {
    return runFfmpeg(ctx.ffmpeg(), ["-hide_banner", "-loglevel", "error", "-ss", t.toFixed(6), "-i", mka, "-t", d.toFixed(6), "-vn", "-ac", "2", "-ar", String(OUT_SR), "-f", "f32le", "pipe:1"]);
  }
  throw new Error("no audio");
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
    if (url.host === "pcm") {
      const projectId = parts[0] ?? "";
      if (!isProjectId(projectId)) return deny(403, "forbidden");
      const t = Number(url.searchParams.get("t"));
      const d = Number(url.searchParams.get("d"));
      if (!(t >= 0) || !(d > 0 && d <= 60)) return deny(400, "bad window");
      try {
        const pcm = await decodeWindow(ctx, root, projectId, t, d);
        return new Response(new Uint8Array(pcm), { status: 200, headers: { "content-type": "application/octet-stream", "x-sample-rate": String(OUT_SR) } });
      } catch (e) {
        return deny(500, String((e as Error).message));
      }
    }
    return deny(403, "forbidden");
  };
}
