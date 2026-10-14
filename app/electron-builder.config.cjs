// electron-builder config for the Windows build. What the packager expects: app/README.md "패키징".
//   npm run pack:win  → dist/win-unpacked (no installer; quick check)
//   npm run dist:win  → dist/cliper-v<ver>-amd64.exe (portable; releases ship only this since v0.3.0)
//
// Native payload (none of it is in git) is copied into resources/:
//   resources/engine/   cliper.exe + its DLLs      ← CLIPER_PACK_ENGINE_DIR (default ../core/target/release)
//   resources/engine/cuda/  only with CLIPER_PACK_CUDA=1 ← CLIPER_PACK_CUDA_DIR (default ../runtime/cuda).
//                       Release builds are CPU-only; the CUDA runtime ships as a separate add-on
//                       (scripts/pack-cuda-addon.mjs) that the app installs when an NVIDIA GPU needs it.
//   resources/ffmpeg/   ffmpeg.exe + ffprobe.exe   ← CLIPER_PACK_FFMPEG_DIR (default: the folder of ffmpeg on PATH)
//   resources/models/   *.onnx + panns_labels.json ← CLIPER_PACK_MODELS_DIR (default ../models)
const { existsSync, readdirSync } = require("node:fs");
const { delimiter, join, resolve } = require("node:path");

const here = __dirname;
const envDir = (name, fallback) => resolve(here, process.env[name] || fallback || "");

function onPath(exe) {
  for (const d of (process.env.PATH || "").split(delimiter)) if (d && existsSync(join(d, exe))) return d;
  return null;
}

function need(dir, files, what) {
  for (const f of files) if (!existsSync(join(dir, f))) throw new Error(`[pack] ${what}: ${join(dir, f)} not found`);
  return dir;
}

const engineDir = need(envDir("CLIPER_PACK_ENGINE_DIR", "../core/target/release"), ["cliper.exe"], "engine (cargo build --release in core/)");
const ffmpegDir = need(process.env.CLIPER_PACK_FFMPEG_DIR ? envDir("CLIPER_PACK_FFMPEG_DIR") : onPath("ffmpeg.exe") || "", ["ffmpeg.exe", "ffprobe.exe"], "ffmpeg (set CLIPER_PACK_FFMPEG_DIR)");
const modelsDir = need(envDir("CLIPER_PACK_MODELS_DIR", "../models"), ["panns_sed.onnx", "panns_labels.json", "UVR-MDX-NET-Voc_FT.onnx"], "models");
const withCuda = process.env.CLIPER_PACK_CUDA === "1";
const cudaDir = withCuda ? need(envDir("CLIPER_PACK_CUDA_DIR", "../runtime/cuda"), ["cudart64_13.dll"], "CUDA runtime") : null;
// a shared (non-static) ffmpeg build needs its DLLs too
const ffmpegFiles = ["ffmpeg.exe", "ffprobe.exe", ...readdirSync(ffmpegDir).filter((f) => /\.dll$/i.test(f))];

console.log(`[pack] engine ${engineDir}\n[pack] ffmpeg ${ffmpegDir}\n[pack] models ${modelsDir}\n[pack] cuda   ${cudaDir ?? "(none: CPU-only build, CUDA via the add-on)"}`);

/** @type {import('electron-builder').Configuration} */
module.exports = {
  appId: "app.cliper.desktop",
  productName: "Cliper",
  directories: { output: "dist", buildResources: "build" },
  // the renderer and main are bundled by electron-vite; nothing from node_modules is needed at runtime
  files: ["out/**/*", "resources/icon.png", "package.json", "!**/*.map"],
  asar: true,
  // use the Electron that npm installed (no download)
  electronDist: join(here, "node_modules", "electron", "dist"),
  extraResources: [
    { from: engineDir, to: "engine", filter: ["cliper.exe", "*.dll"] },
    ...(cudaDir ? [{ from: cudaDir, to: "engine/cuda", filter: ["*.dll"] }] : []),
    { from: ffmpegDir, to: "ffmpeg", filter: ffmpegFiles },
    { from: modelsDir, to: "models", filter: ["*.onnx", "panns_labels.json"] },
  ],
  electronFuses: {
    runAsNode: false,
    enableNodeOptionsEnvironmentVariable: false,
    enableNodeCliInspectArguments: process.env.CLIPER_PACK_INSPECT === "1", // QA builds only (Playwright needs it)
    grantFileProtocolExtraPrivileges: false,
    onlyLoadAppFromAsar: true,
    enableCookieEncryption: true,
    // needs the integrity hash written into the exe resources, i.e. rcedit (signAndEditExecutable)
    enableEmbeddedAsarIntegrityValidation: false,
    loadBrowserProcessSpecificV8Snapshot: false,
  },
  // electronDist copies Electron's default_app.asar too; the app must never fall back to it
  afterPack: async (ctx) => {
    const { rmSync } = require("node:fs");
    rmSync(join(ctx.appOutDir, "resources", "default_app.asar"), { force: true });
  },
  win: {
    target: [
      { target: "portable", arch: ["x64"] },
    ],
    // editing the exe (icon / version info / signing) needs the winCodeSign bundle from GitHub;
    // turn on in a release pipeline that has it
    signAndEditExecutable: false,
  },
  portable: {
    artifactName: "cliper-v${version}-amd64.${ext}",
  },
  nsis: {
    artifactName: "cliper-v${version}-amd64-installer.${ext}",
    installerIcon: "build/icon.ico",
    uninstallerIcon: "build/icon.ico",
    oneClick: false,
    perMachine: false,
    allowToChangeInstallationDirectory: true,
  },
};
