// CUDA runtime add-on for the release (engine job `addon.install_cuda`; release builds don't bundle CUDA).
//
// The engine pins the add-on's size and SHA-256, so the release must carry exactly the canonical
// zip (../var/release/cliper-cuda-addon-v1-amd64.zip). Rebuilding the zip never reproduces the
// pinned hash, so this script does not build one: it verifies the canonical file against the pins
// and copies it next to the app artifacts.
//
//   node scripts/pack-cuda-addon.mjs   → dist/cliper-cuda-addon-v1-amd64.zip (verified copy)
//   env: CLIPER_CUDA_ADDON_ZIP (default ../var/release/cliper-cuda-addon-v1-amd64.zip)
import { createHash } from "node:crypto";
import { copyFileSync, createReadStream, existsSync, mkdirSync, statSync } from "node:fs";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// keep in sync with the engine's pinned values (core: CUDA add-on)
const PINNED = {
  name: "cliper-cuda-addon-v1-amd64.zip",
  size: 1_012_083_860,
  sha256: "e73b28d19705de569c50808998274cf9046c0d16e64e25a010b7e8eb2a422ba6",
};

const here = dirname(dirname(fileURLToPath(import.meta.url)));
const src = resolve(here, process.env.CLIPER_CUDA_ADDON_ZIP || `../var/release/${PINNED.name}`);
if (!existsSync(src)) throw new Error(`[cuda-addon] canonical zip not found: ${src}`);
if (basename(src) !== PINNED.name) throw new Error(`[cuda-addon] expected ${PINNED.name}, got ${basename(src)}`);

const size = statSync(src).size;
if (size !== PINNED.size) throw new Error(`[cuda-addon] size ${size} ≠ pinned ${PINNED.size}`);
const sha256 = await new Promise((ok, fail) => {
  const h = createHash("sha256");
  createReadStream(src).on("data", (d) => h.update(d)).on("end", () => ok(h.digest("hex"))).on("error", fail);
});
if (sha256 !== PINNED.sha256) throw new Error(`[cuda-addon] sha256 ${sha256} ≠ pinned ${PINNED.sha256}`);

const out = join(here, "dist", PINNED.name);
mkdirSync(dirname(out), { recursive: true });
copyFileSync(src, out);
console.log(`[cuda-addon] verified ${size} bytes sha256 ${sha256}\n[cuda-addon] → ${out} (upload this file to the release as-is)`);
