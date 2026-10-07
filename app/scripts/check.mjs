// Runs a renderer-side check file through a running devdriver: node scripts/check.mjs scripts/checks/dsp-check.js
import { readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
const token = process.env.DEVDRIVER_TOKEN || readFileSync(join(tmpdir(), "cliper-devdriver.token"), "utf8");
const file = process.argv[2];
const js = readFileSync(file, "utf8").replace(/^\/\/.*$/gm, "");
const r = await fetch("http://127.0.0.1:7788/eval", { method: "POST", headers: { "content-type": "application/json", "x-devdriver-token": token }, body: JSON.stringify({ js }) });
console.log(JSON.stringify(await r.json(), null, 2));
