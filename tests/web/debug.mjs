// Quick in-page probe: what does the UI state look like after a turn?
import { spawn } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..", "..");
const dir = path.join(os.tmpdir(), "rope-e2e-runtime");
const requireFromRuntime = createRequire(path.join(dir, "node_modules", "patchright-core", "package.json"));
const { chromium } = requireFromRuntime("patchright-core");

const server = spawn(path.join(repoRoot, "target", "debug", "examples", "e2e_server"), [], {
  env: { ...process.env, ROPE_E2E_PORT: "0", ROPE_E2E_TOKEN: "e2e-token" },
});
let buf = "";
const ready = new Promise((resolve, reject) => {
  const t = setTimeout(() => reject(new Error("no ready")), 15000);
  server.stdout.on("data", c => {
    buf += c;
    if (buf.includes("\nready")) { clearTimeout(t); resolve(/e2e server on (http:\/\/\S+)/.exec(buf)[1]); }
  });
});
const url = await ready;
console.log("server", url);

const browser = await chromium.launch({
  executablePath: process.env.ROPE_E2E_BROWSER || "/usr/bin/google-chrome",
  headless: true, args: ["--no-sandbox"],
});
const page = await (await browser.newContext({ viewport: { width: 390, height: 844 } })).newPage();
page.on("console", m => console.log("  [console]", m.type(), m.text()));
page.on("pageerror", e => console.log("  [pageerror]", String(e)));

await page.goto(url, { waitUntil: "domcontentloaded" });
await page.fill("#authToken", "e2e-token");
await page.click("#authGo");
await page.waitForTimeout(500);
await page.click("#tbMenu");
await page.click("#newChat");
await page.waitForTimeout(300);
await page.fill("#input", "Show me everything you can render");
await page.keyboard.press("Enter");
await page.waitForTimeout(2500);

const report = await page.evaluate(async () => {
  const S = window.__rope.state;
  const raf = new Promise(r => requestAnimationFrame(() => r("fired")));
  const snap = S.selected ? S.sessions.get(S.selected) : null;
  return {
    selected: S.selected,
    rafWorks: await raf,
    visibility: document.visibilityState,
    snapSeq: snap?.seq,
    snapBlocks: snap?.blocks.length,
    blockKinds: snap?.blocks.map(b => b.kind),
    state: snap ? { phase: snap.state.phase, turn: snap.state.turn_id, tokens: snap.state.total_tokens } : null,
    domChildren: document.getElementById("chatInner").children.length,
    domIds: [...document.getElementById("chatInner").children].map(c => c.className),
  };
});
console.log(JSON.stringify(report, null, 2));
await page.screenshot({ path: path.join(here, "shots", "debug.png") });
await browser.close();
server.kill("SIGTERM");
