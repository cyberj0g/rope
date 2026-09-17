#!/usr/bin/env node
// Headless e2e for the Rope web UI: drives the real server (scripted model)
// in a mobile-viewport Chromium and captures screenshots for review.
//
//   node tests/web/e2e.mjs [--browser=/path/to/chrome] [--port=8791]
//
// Screenshots land in tests/web/shots/. The driver exits non-zero when an
// assertion fails.

import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import zlib from "node:zlib";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "..", "..");
const shotsDir = path.join(here, "shots");
fs.mkdirSync(shotsDir, { recursive: true });

const args = Object.fromEntries(process.argv.slice(2).map(a => {
  const i = a.indexOf("=");
  return i < 0 ? [a, true] : [a.slice(0, i), a.slice(i + 1)];
}));

/* ---------- runtime: node + patchright-core from the embedded archive ---------- */
function runtimeDir() {
  if (process.env.ROPE_E2E_RUNTIME) return process.env.ROPE_E2E_RUNTIME;
  const dir = path.join(os.tmpdir(), "rope-e2e-runtime");
  if (!fs.existsSync(path.join(dir, "node_modules", "patchright-core"))) {
    const tar = fs.readdirSync(path.join(repoRoot, "browser-runtime"))
      .find(f => f.endsWith(".tar.gz"));
    if (!tar) throw new Error("no browser-runtime archive found");
    fs.mkdirSync(dir, { recursive: true });
    spawnSync("tar", ["xzf", path.join(repoRoot, "browser-runtime", tar), "-C", dir], { stdio: "inherit" });
  }
  return dir;
}
const requireFromRuntime = createRequire(path.join(runtimeDir(), "node_modules", "patchright-core", "package.json"));
const { chromium } = requireFromRuntime("patchright-core");

function browserExecutable() {
  return args["--browser"]
    || process.env.ROPE_E2E_BROWSER
    || ["/usr/bin/google-chrome", "/usr/bin/chromium",
        path.join(os.homedir(), ".cache", "ms-playwright", "chromium-1234", "chrome-linux64", "chrome")]
      .find(p => fs.existsSync(p));
}

/* ---------- tiny PNG encoder (solid two-tone test image) ---------- */
function crc32(buf) {
  let c, table = crc32.table || (crc32.table = Int32Array.from({ length: 256 }, (_, n) => {
    c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    return c;
  }));
  let out = -1;
  for (const b of buf) out = (out >>> 8) ^ table[(out ^ b) & 0xff];
  return (out ^ -1) >>> 0;
}
function pngChunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}
function makePng(width, height) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; ihdr[9] = 6; // 8-bit RGBA
  const raw = Buffer.alloc(height * (1 + width * 4));
  for (let y = 0; y < height; y++) {
    const row = y * (1 + width * 4);
    raw[row] = 0;
    for (let x = 0; x < width; x++) {
      const p = row + 1 + x * 4;
      const corner = x < width / 2 && y < height / 2;
      raw[p] = corner ? 0x4f : 0x14; raw[p + 1] = corner ? 0x8c : 0x2b; raw[p + 2] = corner ? 0xff : 0x4a; raw[p + 3] = 0xff;
    }
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", zlib.deflateSync(raw)),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

/* ---------- e2e server ---------- */
function startServer(port) {
  const bin = path.join(repoRoot, "target", "debug", "examples", "e2e_server");
  if (!fs.existsSync(bin)) spawnSync("cargo", ["build", "--example", "e2e_server"], { cwd: repoRoot, stdio: "inherit" });
  const child = spawn(bin, [], {
    cwd: repoRoot,
    env: { ...process.env, ROPE_E2E_PORT: String(port || 0), ROPE_E2E_TOKEN: "e2e-token" },
  });
  let buffer = "";
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("e2e server did not start")), 30000);
    const onData = chunk => {
      buffer += chunk.toString();
      if (!buffer.includes("\nready")) return;
      clearTimeout(timer);
      const url = /e2e server on (http:\/\/\S+)/.exec(buffer)?.[1];
      if (!url) return reject(new Error("e2e server address missing:\n" + buffer));
      child.stdout.off("data", onData);
      resolve({ child, url: url.replace(/\/$/, "") });
    };
    child.stdout.on("data", onData);
    child.stderr.on("data", d => process.env.ROPE_E2E_DEBUG && process.stderr.write(d));
    child.on("exit", code => code && reject(new Error(`e2e server exited ${code}`)));
  });
}

/* ---------- driver helpers ---------- */
const failures = [];
const screenshots = [];
function fail(msg) {
  failures.push(msg);
  console.error(`  FAIL: ${msg}`);
}
function ok(msg) { console.log(`  ok: ${msg}`); }
async function shot(page, name) {
  const file = path.join(shotsDir, name);
  await page.screenshot({ path: file });
  screenshots.push(file);
  console.log(`  shot: ${name}`);
}
async function waitFor(page, probe, what, timeout = 30000, arg) {
  const deadline = Date.now() + timeout;
  for (;;) {
    if (await page.evaluate(probe, arg)) return;
    if (Date.now() > deadline) { fail(`timeout waiting for ${what}`); return false; }
    await new Promise(r => setTimeout(r, 200));
  }
}
const runningProbe = () => document.body.dataset.turn !== "";
const idleProbe = () =>
  document.body.dataset.snapshot === "1" &&
  document.body.dataset.turn === "" &&
  document.body.dataset.phase === "idle";
async function sendPrompt(page, text) {
  const beforeUsers = await page.evaluate(() => document.querySelectorAll(".msg.user").length);
  await page.click("#input");
  await page.fill("#input", text);
  await page.keyboard.press("Enter");
  await waitFor(page, n => document.querySelectorAll(".msg.user").length > n, `message "${text}" accepted`, 30000, beforeUsers);
  await waitFor(page, idleProbe, `turn "${text}" to finish`);
}
async function closeCam(page) {
  await page.click("#camCancel").catch(() => {});
  await page.evaluate(() => document.getElementById("camera").classList.remove("on"));
}

/* ---------- scenario ---------- */
const port = args["--port"] || 0;
const server = await startServer(port).catch(e => { console.error(e.message); process.exit(1); });
console.log(`server: ${server.url}`);

const browser = await chromium.launch({
  executablePath: browserExecutable(),
  headless: true,
  args: ["--no-sandbox", "--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream"],
});
const context = await browser.newContext({
  viewport: { width: 390, height: 844 },
  deviceScaleFactor: 2,
  isMobile: true,
  hasTouch: true,
  userAgent: "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Mobile Safari/537.36",
});
const page = await context.newPage();
const pageErrors = [];
page.on("pageerror", e => { pageErrors.push(String(e)); console.error("  [pageerror]", String(e).slice(0, 400)); });
page.on("console", m => {
  if (m.type() === "error") {
    const t = m.text();
    pageErrors.push(t);
    console.error("  [console-error]", t.slice(0, 400));
  }
});

const pngFile = path.join(shotsDir, "test-image.png");
fs.writeFileSync(pngFile, makePng(160, 110));

try {
  console.log("1. auth screen");
  await page.goto(server.url, { waitUntil: "domcontentloaded" });
  await waitFor(page, () => document.getElementById("auth").style.display !== "none" && document.getElementById("app").classList.contains("on") === false, "auth screen");
  await shot(page, "01-auth.png");
  await page.fill("#authToken", "wrong-token");
  await page.click("#authGo");
  await waitFor(page, () => document.getElementById("authError").textContent.length > 0, "auth error");
  ok("wrong token rejected, back at auth");
  await page.fill("#authToken", "e2e-token");
  await page.click("#authGo");
  await waitFor(page, () => document.getElementById("app").classList.contains("on") && document.getElementById("tbPhase").className === "idle", "connected");
  await shot(page, "02-empty.png");
  ok("connected with e2e-token");

  console.log("2. first turn: markdown showcase");
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar opens");
  await page.click("#newChat");
  await waitFor(page, () => !document.getElementById("app").classList.contains("side-open"), "sidebar closes after new chat");
  await sendPrompt(page, "Show me everything you can render");
  const md = await page.evaluate(() => ({
    h1: !!document.querySelector(".msg .md h1"),
    h2: !!document.querySelector(".msg .md h2"),
    table: !!document.querySelector(".msg .md table"),
    pre: !!document.querySelector(".msg .md pre code"),
    quote: !!document.querySelector(".msg .md blockquote"),
    link: !!document.querySelector(".msg .md a"),
    strong: !!document.querySelector(".msg .md strong"),
    thinking: !!document.querySelector("details.sec.thinking"),
  }));
  for (const [k, v] of Object.entries(md)) if (!v) fail(`markdown showcase missing: ${k}`);
  ok("showcase rendered: " + JSON.stringify(md));
  await shot(page, "03-showcase.png");

  console.log("3. thinking + tool sections");
  await page.evaluate(() => { const el = document.querySelector("details.sec.thinking"); if (el) el.open = true; });
  await new Promise(r => setTimeout(r, 150));
  await shot(page, "04-thinking.png");
  await sendPrompt(page, "Read notes.md and summarize it");
  await page.evaluate(() => { const el = document.querySelector("details.sec.tool"); if (el) el.open = true; });
  await new Promise(r => setTimeout(r, 150));
  const toolOut = await page.evaluate(() => document.querySelector("details.sec.tool")?.textContent || "");
  if (!toolOut.includes("notes.md")) fail("read tool arguments not visible");
  if (!toolOut.includes("rope: a rope for coding")) fail("read tool output not visible");
  ok("read tool section shows args + output");
  await shot(page, "05-tool.png");

  console.log("4. approval flow");
  await page.fill("#input", "Run the echo command");
  await page.keyboard.press("Enter");
  await waitFor(page, () => document.getElementById("approval").classList.contains("on"), "approval card");
  const appr = await page.evaluate(() => ({
    title: document.getElementById("apprTitle").textContent,
    args: document.getElementById("apprArgs").textContent,
  }));
  if (!appr.title.includes("shell")) fail("approval card does not name the shell tool: " + appr.title);
  if (!appr.args.includes("hello from rope")) fail("approval card does not show arguments");
  ok("approval card: " + appr.title);
  await shot(page, "06-approval.png");
  await page.click('#approval [data-decision="allow_once"]');
  await waitFor(page, idleProbe, "shell turn after approval");
  const shellOut = await page.evaluate(() => {
    const tools = [...document.querySelectorAll("details.sec.tool")];
    tools.forEach(t => (t.open = true));
    return document.body.textContent;
  });
  if (!shellOut.includes("hello from rope")) fail("shell output not visible after approval");
  ok("shell approved and output rendered");
  await shot(page, "07-shell.png");

  console.log("5. plan sheet");
  await sendPrompt(page, "Show me the plan");
  const chipPlanVisible = await page.evaluate(() => !document.getElementById("chipPlan").hidden);
  if (!chipPlanVisible) fail("plan chip not visible after plan turn");
  await page.click("#chipPlan");
  await waitFor(page, () => document.getElementById("sheetWrap").classList.contains("on"), "plan sheet");
  const planText = await page.evaluate(() => document.getElementById("sheetBody").textContent);
  if (!planText.includes("headless e2e validation")) fail("plan sheet missing steps");
  ok("plan sheet shows steps");
  await shot(page, "08-plan.png");
  await page.click("#sheetX");
  await waitFor(page, () => !document.getElementById("sheetWrap").classList.contains("on"), "sheet closed");

  console.log("6. git sheet");
  await page.click("#chipGit");
  await waitFor(page, () => document.getElementById("sheetWrap").classList.contains("on"), "git sheet");
  const gitText = await page.evaluate(() => document.getElementById("sheetBody").textContent);
  if (!gitText.includes("notes.md")) fail("git sheet missing modified notes.md");
  if (!gitText.includes("web.md")) fail("git sheet missing untracked web.md");
  ok("git sheet shows working tree");
  await shot(page, "09-git.png");
  await page.click("#sheetX");
  await waitFor(page, () => !document.getElementById("sheetWrap").classList.contains("on"), "sheet closed");

  console.log("7. image attachment");
  const [fileChooser] = await Promise.all([
    page.waitForEvent("filechooser", { timeout: 10000 }),
    page.click("#attachBtn"),
  ]);
  await fileChooser.setFiles(pngFile);
  await waitFor(page, () => document.querySelectorAll("#plates .plate").length === 1, "attachment plate");
  await sendPrompt(page, "What is in this image?");
  const imgCount = await page.evaluate(() => document.querySelectorAll(".msg.user .imgrid img").length);
  if (imgCount < 1) fail("user message does not show the attached image");
  const imgReply = await page.evaluate(() => document.body.textContent);
  if (!imgReply.includes("I can see the image you attached")) fail("image turn not answered");
  ok("image uploaded, rendered in the bubble, and answered");
  await shot(page, "10-image.png");

  console.log("8. camera capture (fake device)");
  let fileChooserPromise = page.waitForEvent("filechooser", { timeout: 12000 }).catch(() => null);
  await page.click("#cameraBtn");
  const camOverlay = await page.evaluate(() => document.getElementById("camera").classList.contains("on"));
  if (!camOverlay) {
    // the overlay may open a moment later once getUserMedia resolves
    await waitFor(page, () => document.getElementById("camera").classList.contains("on"), "camera overlay", 8000);
  }
  const overlayUp = await page.evaluate(() => document.getElementById("camera").classList.contains("on"));
  if (overlayUp) {
    await waitFor(page, () => document.getElementById("camVideo").videoWidth > 0, "fake camera frame", 10000);
    await shot(page, "11a-camera-overlay.png");
    await page.click("#camSnap");
    await waitFor(page, () => document.querySelectorAll("#plates .plate").length === 1, "camera plate", 10000);
    ok("camera captured a frame into an attachment");
  } else {
    // fallback path: a file chooser for <input capture>
    const fc = await fileChooserPromise;
    if (fc) { await fc.setFiles(pngFile); await waitFor(page, () => document.querySelectorAll("#plates .plate").length === 1, "camera fallback plate", 10000); ok("camera fell back to capture input"); }
    else fail("no camera overlay and no capture fallback");
  }
  if (await page.evaluate(() => document.getElementById("camera").classList.contains("on"))) closeCam(page);
  // send the captured image too
  await sendPrompt(page, "Here is the capture");
  await shot(page, "11b-camera-sent.png");

  console.log("9. steering a running turn");
  const beforeSteer = await page.evaluate(() => document.querySelectorAll(".msg.user").length);
  await page.fill("#input", "Show me everything you can render again please");
  await page.keyboard.press("Enter");
  await waitFor(page, n => document.querySelectorAll(".msg.user").length > n, "steer base turn accepted", 30000, beforeSteer);
  await page.fill("#input", "Also add a steer marker");
  await page.keyboard.press("Enter");
  await waitFor(page, idleProbe, "steered turn", 45000);
  await page.waitForTimeout(2500); // a queued steer may be resubmitted as a fresh turn
  await waitFor(page, idleProbe, "turn settled", 45000);
  const scroll = await page.evaluate(() => {
    const c = document.getElementById("chat");
    return { top: c.scrollTop, height: c.scrollHeight, client: c.clientHeight, atBottom: c.scrollHeight - c.scrollTop - c.clientHeight < 120 };
  });
  console.log("  [scroll]", JSON.stringify(scroll));
  const dbg = await page.evaluate(() => (document.getElementById("__dbg")?.textContent || "").split("\n").slice(-40).join("\n     "));
  if (dbg) console.log("  [dbg]\n     " + dbg);
  const steer = await page.evaluate(() => !!document.querySelector(".msg.steer"));
  const badgeCleared = await page.evaluate(() => !document.querySelector(".badge-queued"));
  if (!steer) {
    const dump = await page.evaluate(() => [...document.querySelectorAll("#chatInner > *")].slice(-12).map(el =>
      el.className + " :: " + (el.textContent || "").trim().replace(/\s+/g, " ").slice(0, 60)));
    console.log("  [steer-debug]");
    dump.forEach(l => console.log("    " + l));
  }
  if (!steer) fail("steer message not rendered");
  if (!badgeCleared) fail("steer queued badge still present after delivery");
  ok("steer queued, rendered, and its queued badge cleared after delivery");
  await shot(page, "12-steer.png");

  console.log("10. search");
  await page.click("#tbSearch");
  await page.fill("#searchInput", "showcase");
  await new Promise(r => setTimeout(r, 200));
  const found = await page.evaluate(() => document.querySelectorAll("mark.srch").length);
  if (found < 1) fail("search found no matches for 'showcase'");
  ok(`search found ${found} matches`);
  await shot(page, "13-search.png");
  await page.click("#searchClose");

  console.log("11. sidebar, sessions, slash palette");
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar open");
  await page.waitForTimeout(350); // let the drawer transition settle
  await shot(page, "14-sidebar.png");
  const sideText = await page.evaluate(() => document.getElementById("sideList").textContent);
  if (!sideText.length) fail("side list empty");
  await page.mouse.click(382, 420); // tap the visible sliver of the backdrop
  await waitFor(page, () => !document.getElementById("app").classList.contains("side-open"), "sidebar closes");
  await page.click("#input");
  await page.fill("#input", "/");
  await waitFor(page, () => document.getElementById("palette").classList.contains("on"), "slash palette");
  await shot(page, "15-palette.png");
  await page.keyboard.press("Escape");
  await page.fill("#input", "/new from palette");
  await page.waitForTimeout(100);
  await page.keyboard.press("Enter");
  await waitFor(page, () => document.querySelectorAll(".side-item").length >= 2, "second session in list", 10000);
  ok("new session via /new palette command");
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar open 2");
  await page.waitForTimeout(350);
  await shot(page, "16-sessions.png");

  console.log("12. token persistence (reload)");
  await page.reload({ waitUntil: "domcontentloaded" });
  await waitFor(page, () => document.getElementById("app").classList.contains("on") && document.getElementById("auth").style.display === "none", "auto-connect after reload", 15000);
  const stillThere = await page.evaluate(() => document.querySelectorAll(".side-item").length >= 2);
  if (!stillThere) fail("sessions not visible after reload");
  ok("token persisted; auto-connected after reload");
  await shot(page, "17-reload.png");

  console.log("13. desktop layout");
  const desktop = await browser.newContext({ viewport: { width: 1360, height: 900 }, deviceScaleFactor: 2 });
  const dpage = await desktop.newPage();
  dpage.on("pageerror", e => pageErrors.push("desktop: " + String(e)));
  await dpage.goto(server.url, { waitUntil: "domcontentloaded" });
  if (await dpage.evaluate(() => document.getElementById("auth").style.display !== "none")) {
    await dpage.fill("#authToken", "e2e-token");
    await dpage.click("#authGo");
  }
  await waitFor(dpage, () => document.getElementById("app").classList.contains("on") && document.getElementById("tbPhase").className === "idle", "desktop connect", 15000);
  await dpage.waitForTimeout(400);
  await dpage.screenshot({ path: path.join(shotsDir, "18-desktop.png") });
  screenshots.push(path.join(shotsDir, "18-desktop.png"));
  console.log("  shot: 18-desktop.png");
  const desktopSidebar = await dpage.evaluate(() => getComputedStyle(document.getElementById("sidebar")).position);
  if (desktopSidebar !== "static") fail("desktop sidebar should be static, got " + desktopSidebar);
  ok("desktop layout: fixed sidebar");
  await desktop.close();

  if (pageErrors.length) {
    fail("page errors: " + pageErrors.slice(0, 3).join(" | "));
  }
} catch (e) {
  fail("driver error: " + e.message);
  try { await shot(page, "99-crash.png"); } catch {}
} finally {
  await browser.close();
  server.child.kill("SIGTERM");
  await new Promise(r => { server.child.once("exit", r); setTimeout(r, 3000).unref?.(); });
}

console.log(`\n${failures.length === 0 ? "PASS" : "FAIL"} — ${failures.length} failure(s), ${screenshots.length} screenshots in ${shotsDir}`);
process.exit(failures.length ? 1 : 0);
