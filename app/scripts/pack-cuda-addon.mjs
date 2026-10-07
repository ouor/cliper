// Builds the CUDA runtime add-on that the app installs on PCs with an NVIDIA GPU
// (engine job `addon.install_cuda`; release builds don't bundle CUDA).
//
//   node scripts/pack-cuda-addon.mjs            → dist/cliper-cuda-addon-v1-amd64.zip (+ .json manifest)
//   env: CLIPER_PACK_CUDA_DIR (default ../runtime/cuda), CLIPER_CUDA_ADDON_VERSION (default 1)
//
// Layout: the DLLs at the zip root (the engine extracts into %LOCALAPPDATA%\Cliper\addons\cuda-v<N>).
// Uses Windows' built-in tar.exe (bsdtar) to write the zip; no extra packages.
import { createHash } from "node:crypto";
import { createReadStream, existsSync, mkdirSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const here = dirname(dirname(fileURLToPath(import.meta.url)));
const src = resolve(here, process.env.CLIPER_PACK_CUDA_DIR || "../runtime/cuda");
const version = process.env.CLIPER_CUDA_ADDON_VERSION || "1";
const outDir = join(here, "dist");
const name = `cliper-cuda-addon-v${version}-amd64.zip`;
const out = join(outDir, name);

const files = readdirSync(src).filter((f) => /\.dll$/i.test(f)).sort();
for (const need of ["cudart64_13.dll", "cublas64_13.dll", "cublasLt64_13.dll", "cufft64_12.dll", "cudnn64_9.dll"])
  if (!files.includes(need)) throw new Error(`[cuda-addon] ${join(src, need)} not found`);

mkdirSync(outDir, { recursive: true });
rmSync(out, { force: true });
const tar = join(process.env.SystemRoot || "C:\\Windows", "System32", "tar.exe");
if (!existsSync(tar)) throw new Error("[cuda-addon] tar.exe not found (Windows 10 1803+)");
console.log(`[cuda-addon] ${files.length} DLLs from ${src} → ${out}`);
execFileSync(tar, ["-a", "-c", "-f", out, "-C", src, ...files], { stdio: "inherit" });

const sha256 = await new Promise((ok, fail) => {
  const h = createHash("sha256");
  createReadStream(out).on("data", (d) => h.update(d)).on("end", () => ok(h.digest("hex"))).on("error", fail);
});
const size = statSync(out).size;
const manifest = {
  name,
  version: Number(version),
  size,
  sha256,
  unpacked: files.reduce((n, f) => n + statSync(join(src, f)).size, 0),
  files,
};
writeFileSync(out.replace(/\.zip$/, ".json"), JSON.stringify(manifest, null, 2) + "\n");
console.log(`[cuda-addon] ${(size / 1024 / 1024).toFixed(1)} MB  sha256 ${sha256}`);
