import { resolve } from "node:path";
import { defineConfig, externalizeDepsPlugin } from "electron-vite";
import react from "@vitejs/plugin-react";
import type { Plugin } from "vite";

// Strict CSP for the packaged renderer; dev additionally allows Vite's HMR preamble/socket.
function csp(): Plugin {
  const base = [
    "default-src 'self'",
    "img-src 'self' data: blob: https: cliper-media:",
    "media-src 'self' blob: cliper-media:",
    "font-src 'self' data:",
    "style-src 'self' 'unsafe-inline'",
    "object-src 'none'",
    "base-uri 'none'",
  ];
  return {
    name: "cliper-csp",
    transformIndexHtml: {
      order: "pre",
      handler(html, ctx) {
        const dev = !!ctx.server;
        const policy = [
          ...base,
          dev ? "script-src 'self' 'unsafe-inline'" : "script-src 'self'",
          dev ? "connect-src 'self' cliper-media: ws://localhost:5733 http://localhost:5733" : "connect-src 'self' cliper-media:",
          "worker-src 'self' blob:",
        ].join("; ");
        return html.replace("%CSP%", policy);
      },
    },
  };
}

export default defineConfig({
  main: {
    plugins: [externalizeDepsPlugin()],
    build: { rollupOptions: { input: { index: resolve(__dirname, "src/main/index.ts") } } },
  },
  preload: {
    plugins: [externalizeDepsPlugin()],
    build: {
      rollupOptions: {
        input: { index: resolve(__dirname, "src/preload/index.ts") },
        // sandboxed preloads must be CommonJS
        output: { format: "cjs", entryFileNames: "[name].cjs" },
      },
    },
  },
  renderer: {
    root: resolve(__dirname, "src/renderer"),
    resolve: { alias: { "@ipc": resolve(__dirname, "src/ipc"), "@": resolve(__dirname, "src/renderer") } },
    plugins: [react(), csp()],
    // electron-vite leaves output unminified by default; the renderer is the one bundle where it matters
    build: { minify: true, rollupOptions: { input: { index: resolve(__dirname, "src/renderer/index.html") } } },
    server: { port: 5733, strictPort: true },
  },
});
