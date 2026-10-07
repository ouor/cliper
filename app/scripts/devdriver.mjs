// Dev/QA driver: launches the built app (out/) through Playwright's Electron support and
// exposes a tiny localhost-only HTTP API for scripted checks and screenshots.
//
//   node scripts/devdriver.mjs [--port 7788] [--width 1600 --height 960]
//   env: CLIPER_PROJECTS, CLIPER_USER_DATA, CLIPER_SKIP_FIRST_RUN=1
//
//   POST /eval   {"js": "<expression>"}          → {"value": …}  (renderer context)
//   POST /shot   {"path": "C:/…/x.png"}           → screenshot of the window
//   POST /key    {"key": "Control+J"}             → keyboard.press
//   POST /type   {"text": "…"}                    → keyboard.type
//   POST /click  {"selector": "text=시작하기"}     → page.click
//   POST /mouse  {"x":…,"y":…,"action":"click|down|up|move","button":"left"}
//   POST /resize {"width":…,"height":…}
//   POST /close                                   → close the window (as the X button)
//   GET  /logs                                    → recent console lines
//   POST /quit
import http from "node:http";
import { _electron as electron } from "playwright-core";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const appDir = resolve(here, "..");
const arg = (name, def) => {
  const i = process.argv.indexOf(`--${name}`);
  return i > 0 ? process.argv[i + 1] : def;
};
const port = Number(arg("port", "7788"));
const width = Number(arg("width", "1600"));
const height = Number(arg("height", "960"));

const app = await electron.launch({ args: [appDir], cwd: appDir, env: { ...process.env } });
const page = await app.firstWindow();
const logs = [];
page.on("console", (m) => {
  logs.push(`[${m.type()}] ${m.text()}`);
  if (logs.length > 500) logs.shift();
});
page.on("pageerror", (e) => logs.push(`[pageerror] ${e.message}`));
await app.evaluate(({ BrowserWindow }, [w, h]) => {
  const win = BrowserWindow.getAllWindows()[0];
  win.setSize(w, h);
  win.center();
}, [width, height]);

const body = (req) =>
  new Promise((res) => {
    let s = "";
    req.on("data", (c) => (s += c));
    req.on("end", () => res(s ? JSON.parse(s) : {}));
  });

http
  .createServer(async (req, res) => {
    const send = (code, obj) => {
      res.writeHead(code, { "content-type": "application/json; charset=utf-8" });
      res.end(JSON.stringify(obj));
    };
    try {
      const b = req.method === "POST" ? await body(req) : {};
      switch (req.url) {
        case "/eval":
          return send(200, { value: await page.evaluate(b.js) });
        case "/shot":
          await page.screenshot({ path: b.path });
          return send(200, { ok: true });
        case "/key":
          await page.keyboard.press(b.key);
          return send(200, { ok: true });
        case "/type":
          await page.keyboard.type(b.text);
          return send(200, { ok: true });
        case "/click":
          await page.click(b.selector, { timeout: 5000 });
          return send(200, { ok: true });
        case "/mouse": {
          const m = page.mouse;
          if (b.action === "move") await m.move(b.x, b.y, { steps: b.steps ?? 1 });
          else if (b.action === "down") await m.down({ button: b.button ?? "left" });
          else if (b.action === "up") await m.up({ button: b.button ?? "left" });
          else if (b.action === "wheel") await m.wheel(b.dx ?? 0, b.dy ?? 0);
          else await m.click(b.x, b.y, { button: b.button ?? "left", clickCount: b.count ?? 1 });
          return send(200, { ok: true });
        }
        case "/resize":
          await app.evaluate(({ BrowserWindow }, [w, h]) => BrowserWindow.getAllWindows()[0].setSize(w, h), [b.width, b.height]);
          return send(200, { ok: true });
        case "/close": // like clicking the window's X (preventable close event)
          await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows()[0]?.close());
          return send(200, { ok: true });
        case "/logs":
          return send(200, { logs: logs.slice(-200) });
        case "/quit":
          send(200, { ok: true });
          await app.close();
          process.exit(0);
      }
      send(404, { error: "unknown" });
    } catch (e) {
      send(500, { error: String(e?.message ?? e) });
    }
  })
  .listen(port, "127.0.0.1", () => console.log(`devdriver listening on 127.0.0.1:${port}`));
