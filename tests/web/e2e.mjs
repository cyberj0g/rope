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
async function attachImage(pg, file) {
  const [fileChooser] = await Promise.all([
    pg.waitForEvent("filechooser", { timeout: 10000 }),
    pg.click("#attachBtn"),
  ]);
  await fileChooser.setFiles(file);
  await waitFor(pg, () => document.querySelectorAll("#plates .plate").length >= 1, "attachment plate for " + file);
}
// Upload the named plate in the current session (if not already uploaded)
// and resolve its stored path + session.
async function uploadedAttachment(page, name) {
  return mainEval(`(async () => {
    const R = window.__rope;
    const a = R.attachments.find(x => x.file && x.file.name === __a);
    if (!a) return null;
    if (!a.path) await R.uploadAttachment(a, R.state.selected);
    return a.path ? { path: a.path, session: a.pathSession } : null;
  })()`, name);
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
context.on("response", r => {
  if (r.status() >= 400) console.error("  [http]", r.status(), r.request().method(), r.url().slice(0, 120));
});
page.on("pageerror", e => { pageErrors.push(String(e)); console.error("  [pageerror]", String(e).slice(0, 400)); });
page.on("console", m => {
  if (m.type() === "error") {
    const t = m.text();
    pageErrors.push(t);
    console.error("  [console-error]", t.slice(0, 400));
  }
});

// The driver's default evaluate world cannot see the page script's globals,
// so main-world access (window.__rope and friends) goes through CDP.
const cdp = await context.newCDPSession(page);
async function mainEval(expr, arg = undefined) {
  const r = await cdp.send("Runtime.evaluate", {
    expression: `((__a) => { return (${expr}); })(${JSON.stringify(arg)})`,
    returnByValue: true,
    awaitPromise: true,
  });
  if (r.exceptionDetails) {
    throw new Error((r.exceptionDetails.exception && r.exceptionDetails.exception.description) || r.exceptionDetails.text || "main-world eval failed");
  }
  return r.result.value;
}
async function waitForMain(expr, what, timeout = 30000, arg) {
  const deadline = Date.now() + timeout;
  for (;;) {
    if (await mainEval(expr, arg)) return true;
    if (Date.now() > deadline) { fail(`timeout waiting for ${what}`); return false; }
    await new Promise(t => setTimeout(t, 200));
  }
}

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
  // #15: a section the user expanded must stay expanded when new blocks arrive.
  const thinkingStillOpen = await page.evaluate(() => {
    const el = document.querySelector("details.sec.thinking");
    return el ? el.open : null;
  });
  if (thinkingStillOpen !== true) fail("expanded thinking section collapsed when the next turn arrived (#15)");
  ok("expanded sections survive new blocks (#15)");
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

  console.log("14. ISSUES regressions: code safety and renderer fidelity (#1, #18, #20)");
  const xss = await mainEval(`(() => {
    const out = window.__rope.highlight("<IMG SRC=x ONERROR=\\"document.documentElement.dataset.ropeXss='1'\\">", "html");
    return {
      rawTag: /<img/i.test(out),
      escaped: out.includes("&lt;IMG"),
      ran: document.documentElement.dataset.ropeXss === "1",
    };
  })()`);
  if (xss.rawTag || !xss.escaped || xss.ran) fail("fenced-code XSS probe: " + JSON.stringify(xss));
  await sendPrompt(page, "Code fence probe\n```html\n<IMG SRC=x ONERROR=\"document.documentElement.dataset.ropeXss='1'\">\n<!-- visible comment -->\n<!DOCTYPE html>\n```");
  const xssDom = await page.evaluate(() => {
    const codes = [...document.querySelectorAll(".msg.user pre code")];
    const last = codes[codes.length - 1];
    return {
      ran: document.documentElement.dataset.ropeXss === "1",
      injected: !!document.querySelector(".msg.user pre code img"),
      text: last ? last.textContent : "",
    };
  });
  if (xssDom.ran || xssDom.injected) fail("in-DOM fenced-code probe executed or injected markup: " + JSON.stringify(xssDom));
  if (!xssDom.text.includes("<IMG SRC=x ONERROR=") || !xssDom.text.includes("<!-- visible comment -->") || !xssDom.text.includes("<!DOCTYPE html>"))
    fail("fenced code text not preserved literally: " + JSON.stringify(xssDom.text.slice(0, 120)));
  ok("fenced code stays inert and literal");
  const inlineOut = await mainEval(`window.__rope.inline("Use \`**literal stars**\` and \`a ~b~ c\` plus [x](https://e.com) outside code")`);
  if (!inlineOut.includes("<code>**literal stars**</code>")) fail("inline code lost literal asterisks: " + inlineOut);
  if (!inlineOut.includes("<code>a ~b~ c</code>")) fail("inline code lost literal tildes: " + inlineOut);
  if (!inlineOut.includes('<a href="https://e.com"')) fail("links outside code no longer rendered: " + inlineOut);
  ok("inline code spans stay literal, formatting outside still works");
  const ages = await mainEval(`(() => {
    const now = 1789643144881; // millisecond epoch reference
    const R = window.__rope;
    return {
      justNow: R.relTimeAt(now, now - 25 * 1000),
      m7: R.relTimeAt(now, 1789642715493),
      m2: R.relTimeAt(now, now - 120 * 1000),
      h2: R.relTimeAt(now, now - 7200 * 1000),
      d2: R.relTimeAt(now, now - 172800 * 1000),
    };
  })()`);
  const wantAges = { justNow: "just now", m7: "7m ago", m2: "2m ago", h2: "2h ago", d2: "2d ago" };
  for (const [k, want] of Object.entries(wantAges)) {
    if (ages[k] !== want) fail(`relTime(${k}): got "${ages[k]}", want "${want}"`);
  }
  ok("relTime reads millisecond catalog timestamps with correct units");

  console.log("15. ISSUES regressions: tool images, tool diffs, plate states (#16, #17, #21)");
  // Tool images are fetched per session, so give the probe a real uploaded
  // attachment in the current session — a made-up path would 404.
  await attachImage(page, pngFile);
  const probeImage = await uploadedAttachment(page, "test-image.png");
  if (!probeImage) fail("could not resolve the probe attachment upload");
  const toolBlock = await mainEval(`(() => {
    const s = window.__rope.state;
    const srcSession = __a.session;
    const img = { mime_type: "image/png", path: __a.path, width: 40, height: 30 };
    const el = window.__rope.blockEl(srcSession, {
      id: "999", kind: "tool", content: "", model: "", queued: false,
      images: [img],
      tool: { call_id: "c1", name: "view_image", arguments: "{}", output: "viewed " + img.path, diff: "+ line one\\n- line two", status: "done" },
      timer: { running: false, elapsed_ms: 120 },
    });
    const text = el.textContent;
    return {
      imgs: el.querySelectorAll(".imgrid img").length,
      diff: text.includes("+ line one") && text.includes("- line two"),
    };
  })()`, probeImage);
  await mainEval(`(() => {
    const R = window.__rope;
    R.attachments = [];
    R.renderPlates();
  })()`);
  if (toolBlock.imgs < 1) fail("tool block does not render its published images: " + JSON.stringify(toolBlock));
  if (!toolBlock.diff) fail("tool block does not show its stored diff: " + JSON.stringify(toolBlock));
  ok("tool blocks render published images and diffs");
  const plates = await mainEval(`(() => {
    const R = window.__rope;
    const f = new File([new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10])], "probe.png", { type: "image/png" });
    const a = { id: "probe-plate", file: f, path: null, pathSession: null, status: "idle", preview: null };
    R.attachments = R.attachments.concat([a]);
    const states = {};
    R.renderPlates();
    states.idle = document.querySelectorAll("#plates .spin").length;
    a.status = "uploading"; R.renderPlates();
    states.uploading = document.querySelectorAll("#plates .spin").length;
    a.status = "done"; a.path = "img/probe.png"; a.pathSession = "sess-probe"; R.renderPlates();
    states.done = document.querySelectorAll("#plates .spin").length;
    a.status = "error"; R.renderPlates();
    states.error = { spin: document.querySelectorAll("#plates .spin").length, err: document.querySelectorAll("#plates .err").length };
    // No revokeObjectURL here: the img may still be mid-load on the truncated
    // PNG, and revoking races into a console net::ERR_FILE_NOT_FOUND. The
    // page is torn down at suite end anyway.
    R.attachments = R.attachments.filter(x => x.id !== "probe-plate");
    R.renderPlates();
    return states;
  })()`);
  if (plates.idle !== 0 || plates.uploading !== 1 || plates.done !== 0)
    fail("plate spinner does not track the transfer: " + JSON.stringify(plates));
  if (plates.error.spin !== 0 || plates.error.err !== 1) fail("plate error state wrong: " + JSON.stringify(plates.error));
  ok("plate spinner only during transfer; error shown without spin");

  console.log("16. ISSUES regressions: search edit and close (#14, #19)");
  // The "rope" occurrences live in the original showcase conversation — the
  // oldest session, which is the last sidebar row (catalog is newest-first).
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar to showcase session");
  const searchSession = await page.evaluate(() => {
    const items = [...document.querySelectorAll(".side-item")];
    const last = items[items.length - 1];
    if (last) last.click();
    return last ? last.dataset.session : null;
  });
  await waitForMain(`window.__rope.state.selected === __a && !!window.__rope.state.sessions.get(__a)`, "showcase session selected", 15000, searchSession);
  await page.click("#tbSearch");
  await page.fill("#searchInput", "rope");
  await new Promise(r => setTimeout(r, 250));
  const c1 = await page.evaluate(() => document.querySelectorAll("mark.srch").length);
  if (c1 < 2) fail(`search 'rope' should find several matches, got ${c1}`);
  await page.keyboard.press("End");
  await page.keyboard.press("Backspace");
  await new Promise(r => setTimeout(r, 250));
  const c2 = await page.evaluate(() => document.querySelectorAll("mark.srch").length);
  await page.keyboard.type("e");
  await new Promise(r => setTimeout(r, 250));
  const c3 = await page.evaluate(() => document.querySelectorAll("mark.srch").length);
  if (c3 !== c1) fail(`editing the query changed the match count: rope=${c1} rop=${c2} rope=${c3}`);
  ok(`search survives editing the query (${c1} matches both times)`);
  await page.click("#searchClose");
  const closed = await page.evaluate(() => ({
    marks: document.querySelectorAll("mark.srch").length,
    field: document.getElementById("searchInput").value,
    count: document.getElementById("searchCount").textContent,
  }));
  if (closed.marks !== 0 || closed.field !== "" || closed.count !== "0/0")
    fail("closing search left stale state: " + JSON.stringify(closed));
  await page.click("#tbSearch");
  const reopened = await page.evaluate(() => ({
    field: document.getElementById("searchInput").value,
    count: document.getElementById("searchCount").textContent,
  }));
  if (reopened.field !== "" || reopened.count !== "0/0") fail("reopened search not clean: " + JSON.stringify(reopened));
  ok("closing search clears field, marks, and count; reopen is clean");
  await page.click("#searchClose"); // hide the bar for the rest of the run
  await waitFor(page, () => !document.getElementById("searchbar").classList.contains("on"), "search bar hidden");

  console.log("17. ISSUES regressions: plan sheet no duplication + live updates (#12, #13)");
  await page.click("#chipPlan");
  await waitFor(page, () => document.getElementById("sheetWrap").classList.contains("on"), "plan sheet reopen");
  const planRows1 = await page.evaluate(() => document.querySelectorAll("#sheetBody .plan-row").length);
  if (planRows1 !== 3) fail(`plan sheet should have 3 rows, got ${planRows1}`);
  await page.click("#sheetX");
  await waitFor(page, () => !document.getElementById("sheetWrap").classList.contains("on"), "sheet closed after plan");
  await page.click("#chipGit");
  await waitFor(page, () => document.getElementById("sheetWrap").classList.contains("on"), "git sheet between plans");
  await page.click("#sheetX");
  await waitFor(page, () => !document.getElementById("sheetWrap").classList.contains("on"), "sheet closed after git");
  await page.click("#chipPlan");
  await waitFor(page, () => document.getElementById("sheetWrap").classList.contains("on"), "plan sheet third opening");
  const planAgain = await page.evaluate(() => ({
    rows: document.querySelectorAll("#sheetBody .plan-row").length,
    hasGit: document.getElementById("sheetBody").textContent.includes("notes.md"),
  }));
  if (planAgain.rows !== 3 || planAgain.hasGit) fail("plan sheet accumulated content: " + JSON.stringify(planAgain));
  ok("plan sheet content is exact on repeated and interleaved openings");
  await mainEval(`(() => {
    const s = window.__rope.state;
    s.sessions.get(s.selected).plan = {
      explanation: "live update",
      plan: [
        { step: "step one", status: "completed" },
        { step: "step two", status: "in_progress" },
        { step: "step three", status: "pending" },
        { step: "step four", status: "pending" },
      ],
    };
    window.__rope.renderAll();
  })()`);
  await new Promise(r => setTimeout(r, 200));
  const planLive = await page.evaluate(() => ({
    rows: document.querySelectorAll("#sheetBody .plan-row").length,
    text: document.getElementById("sheetBody").textContent,
  }));
  if (planLive.rows !== 4 || !planLive.text.includes("live update"))
    fail("open plan sheet did not refresh live: " + JSON.stringify({ rows: planLive.rows }));
  ok("open plan sheet follows live plan updates without duplication");
  await mainEval(`(() => {
    const s = window.__rope.state;
    s.sessions.get(s.selected).plan = {
      explanation: "Validation pass",
      plan: [
        { step: "Build the mobile-first web UI", status: "completed" },
        { step: "Wire image upload and camera capture", status: "in_progress" },
        { step: "Run headless e2e validation", status: "pending" },
      ],
    };
    window.__rope.renderAll();
  })()`);
  await page.click("#sheetX");
  await waitFor(page, () => !document.getElementById("sheetWrap").classList.contains("on"), "sheet closed after live plan");

  console.log("18. ISSUES regression: send button runs slash commands (#7)");
  const sessionsBeforeSlash = await page.evaluate(() => document.querySelectorAll(".side-item").length);
  await page.click("#input");
  await page.fill("#input", "/new via button");
  await new Promise(r => setTimeout(r, 150));
  await page.click("#sendBtn");
  await waitFor(page, n => document.querySelectorAll(".side-item").length > n, "session created by send button", 15000, sessionsBeforeSlash);
  const slashLeak = await page.evaluate(() =>
    [...document.querySelectorAll(".msg.user")].some(el => el.textContent.includes("/new via button")));
  if (slashLeak) fail("slash command text was sent to the model via the button");
  if (await page.evaluate(() => document.getElementById("input").value) !== "") fail("input not cleared after button slash");
  ok("send button dispatches slash commands like Enter");

  console.log("19. ISSUES regressions: steer by button, cancel by Escape from composer (#6, #8)");
  // The chat DOM lags state changes by a frame; make sure it shows the fresh
  // (empty) session before counting user messages against a baseline.
  await waitForMain(`window.__rope.chatSession() === window.__rope.state.selected`, "chat shows the new session", 15000);
  const usersBeforeSteer = await page.evaluate(() => document.querySelectorAll(".msg.user").length);
  await page.fill("#input", "Button steer base turn");
  await page.keyboard.press("Enter");
  await waitFor(page, n => document.querySelectorAll(".msg.user").length > n, "button steer base accepted", 30000, usersBeforeSteer);
  // The turn window is short; sample the main-world state tightly instead of
  // relying on the 200ms poll cadence.
  let running = false;
  const runStart = Date.now();
  const runDeadline = runStart + 8000;
  let lastSample = "";
  while (Date.now() < runDeadline && !running) {
    const sample = await mainEval(`(() => {
      const s = window.__rope.state; const snap = s.sessions.get(s.selected);
      const st = snap?.state;
      return st ? (st.phase + "/" + (st.turn_id || "-")) : "nosnap";
    })()`);
    if (sample !== lastSample) { console.log("  [t+" + (Date.now() - runStart) + "ms]", sample); lastSample = sample; }
    running = sample.includes("/") && !sample.endsWith("/-");
    if (!running) await new Promise(t => setTimeout(t, 30));
  }
  if (!running) {
    const dbg = await mainEval(`(() => {
      const s = window.__rope.state; const snap = s.sessions.get(s.selected);
      return { selected: s.selected, phase: snap?.state?.phase, turn: snap?.state?.turn_id, kinds: snap?.blocks.map(b => b.kind) };
    })()`);
    fail("button steer base turn not running: " + JSON.stringify(dbg));
  }
  const cancelVisible = await mainEval(`!document.getElementById("cancelBtn").hidden`);
  if (!cancelVisible) fail("cancel button not visible while the turn is running");
  ok("cancel button appears alongside the send button while running");
  await page.fill("#input", "Steer from the button");
  await page.click("#sendBtn");
  await waitForMain(`document.querySelector(".msg.steer") !== null && !!window.__rope.state.sessions.get(window.__rope.state.selected)?.state?.turn_id`, "steer queued while running", 10000);
  ok("button sends a steer instead of cancelling");
  await page.click("#input");
  await page.keyboard.press("Escape");
  await waitFor(page, idleProbe, "turn cancelled by Escape while composer focused", 30000);
  const escaped = await mainEval(`document.body.textContent.toLowerCase().includes("cancelled by user")`);
  if (!escaped) fail("Escape from focused composer did not cancel the turn");
  ok("Escape cancels the turn even with the composer focused");
  await page.waitForTimeout(2500); // queued steer may be resubmitted as a fresh turn
  await waitFor(page, idleProbe, "settled after cancel", 45000);

  console.log("20. ISSUES regression: upload keeps the original send destination (#4)");
  const widePng = path.join(shotsDir, "test-image-wide.png");
  const narrowPng = path.join(shotsDir, "test-image-narrow.png");
  fs.writeFileSync(widePng, makePng(200, 140));
  fs.writeFileSync(narrowPng, makePng(90, 60));
  await page.route("**/api/sessions/**/attachments", route => {
    if (route.request().method() === "POST") setTimeout(() => route.continue(), 2500);
    else route.continue();
  });
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar for upload send");
  await page.click("#newChat");
  await waitFor(page, () => !document.getElementById("app").classList.contains("side-open"), "upload send session");
  const uploadSession = await mainEval("window.__rope.state.selected");
  await attachImage(page, widePng);
  await page.fill("#input", "Upload switch test");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(400);
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar mid-upload");
  const switchedTo = await page.evaluate(a2 => {
    const items = [...document.querySelectorAll(".side-item")];
    const target = items.find(el => el.dataset.session && el.dataset.session !== a2);
    if (target) { target.click(); return target.dataset.session; }
    return null;
  }, uploadSession);
  if (!switchedTo) fail("no second session available for the mid-upload switch");
  await waitForMain(`window.__rope.state.selected === __a && !!window.__rope.state.sessions.get(__a)`, "switched during upload", 15000, switchedTo);
  await page.waitForTimeout(3500); // let the delayed upload and the captured-destination send finish
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar back to upload session");
  await page.evaluate(name => {
    document.querySelector(`.side-item[data-session="${name}"]`)?.click();
  }, uploadSession);
  await waitForMain(`window.__rope.state.selected === __a && (window.__rope.state.sessions.get(__a)?.blocks.some(b => b.kind === "user" && b.content === "Upload switch test") ?? false)`,
    "delayed send delivered to the original session", 30000, uploadSession);
  const uploadResult = await mainEval(`(() => {
    const snap = window.__rope.state.sessions.get(__a);
    const msg = snap.blocks.find(b => b.kind === "user" && b.content === "Upload switch test");
    return { imgs: msg ? msg.images.length : -1, phase: snap.state.phase };
  })()`, uploadSession);
  if (uploadResult.imgs !== 1) fail("upload-switch message did not carry its image: " + JSON.stringify(uploadResult));
  const leak = await mainEval(`(() => {
    const snap = window.__rope.state.sessions.get(__a);
    return snap ? snap.blocks.some(b => b.content === "Upload switch test") : false;
  })()`, switchedTo);
  if (leak) fail("upload-switch message leaked into the session switched to mid-upload");
  const draft = await page.evaluate(() => document.getElementById("input").value);
  if (draft !== "Upload switch test") fail("draft in the switched-to session was not preserved: " + JSON.stringify(draft));
  ok("delayed upload sent to the original session; other draft intact");
  await mainEval(`(() => {
    const R = window.__rope;
    R.attachments = [];
    R.renderPlates();
  })()`);
  await page.unroute("**/api/sessions/**/attachments");
  await page.fill("#input", "");

  console.log("21. ISSUES regression: sessions keep their own images (#3)");
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar for image session A");
  await page.click("#newChat");
  await waitFor(page, () => !document.getElementById("app").classList.contains("side-open"), "image session A");
  const imgA = await mainEval("window.__rope.state.selected");
  await attachImage(page, widePng);
  await page.fill("#input", "Image check identical prompt");
  await page.keyboard.press("Enter");
  await waitFor(page, idleProbe, "image session A turn", 30000);
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar for image session B");
  await page.click("#newChat");
  await waitFor(page, () => !document.getElementById("app").classList.contains("side-open"), "image session B");
  const imgB = await mainEval("window.__rope.state.selected");
  await attachImage(page, narrowPng);
  await page.fill("#input", "Image check identical prompt");
  await page.keyboard.press("Enter");
  await waitFor(page, idleProbe, "image session B turn", 30000);
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar back to image session A");
  await page.evaluate(name => {
    document.querySelector(`.side-item[data-session="${name}"]`)?.click();
  }, imgA);
  await waitForMain(`window.__rope.state.selected === __a && (window.__rope.state.sessions.get(__a)?.blocks.some(b => b.kind === "user") ?? false)`,
    "image session A selected", 15000, imgA);
  await page.waitForTimeout(400); // let the transcript rebuild settle
  await waitForMain(`window.__rope.chatSession() === window.__rope.state.selected`, "chat shows session A", 15000);
  await waitFor(page, () => {
    const i = document.querySelector(".msg.user .imgrid img");
    // The placeholder is a 1x1 gif; require the real blob image.
    return i && i.src.startsWith("blob:") && i.complete && i.naturalWidth > 1;
  }, "session A image loaded", 15000);
  const aDump = await mainEval(`(() => {
    const s = window.__rope.state;
    const snap = s.sessions.get(s.selected);
    return {
      selected: s.selected,
      imgMeta: snap.blocks.filter(b => b.kind === "user").flatMap(b => b.images.map(i => i.width + "@" + i.path)),
      dom: [...document.querySelectorAll(".msg.user .imgrid img")].map(i => ({ src: i.src.slice(0, 30), w: i.naturalWidth })),
      cache: window.__rope.imgCache(),
    };
  })()`);
  console.log("  [imgA]", JSON.stringify(aDump));
  const widthA = await page.evaluate(() => document.querySelector(".msg.user .imgrid img").naturalWidth);
  if (widthA !== 200) fail(`session A shows the wrong image (width ${widthA}, want 200)`);
  else ok("session A shows its own 200px image after switching back");
  await page.click("#tbMenu");
  await waitFor(page, () => document.getElementById("app").classList.contains("side-open"), "sidebar to image session B");
  await page.evaluate(name => {
    document.querySelector(`.side-item[data-session="${name}"]`)?.click();
  }, imgB);
  await waitForMain(`window.__rope.state.selected === __a && (window.__rope.state.sessions.get(__a)?.blocks.some(b => b.kind === "user") ?? false)`,
    "image session B selected", 15000, imgB);
  await waitForMain(`window.__rope.chatSession() === window.__rope.state.selected`, "chat shows session B", 15000);
  await waitFor(page, () => {
    const i = document.querySelector(".msg.user .imgrid img");
    return i && i.src.startsWith("blob:") && i.complete && i.naturalWidth > 1;
  }, "session B image loaded", 15000);
  const bDump = await mainEval(`(() => {
    const s = window.__rope.state;
    const snap = s.sessions.get(s.selected);
    return {
      selected: s.selected,
      imgMeta: snap.blocks.filter(b => b.kind === "user").flatMap(b => b.images.map(i => i.width + "@" + i.path)),
      dom: [...document.querySelectorAll(".msg.user .imgrid img")].map(i => ({ src: i.src.slice(0, 30), w: i.naturalWidth })),
      cache: window.__rope.imgCache(),
    };
  })()`);
  console.log("  [imgB]", JSON.stringify(bDump));
  const widthB = await page.evaluate(() => document.querySelector(".msg.user .imgrid img").naturalWidth);
  if (widthB !== 90) fail(`session B shows the wrong image (width ${widthB}, want 90)`);
  ok("session B shows its own 90px image");

  console.log("22. ISSUES regression: reload keeps mutations working (#2)");
  await page.reload({ waitUntil: "domcontentloaded" });
  await waitFor(page, () =>
    document.getElementById("app").classList.contains("on") &&
    document.getElementById("auth").style.display === "none" &&
    document.body.dataset.snapshot === "1" &&
    document.body.dataset.phase === "idle", "reconnected with snapshot after reload", 20000);
  const usersBeforeReloadSend = await page.evaluate(() => document.querySelectorAll(".msg.user").length);
  await page.fill("#input", "A fresh action after reload");
  await page.keyboard.press("Enter");
  await waitFor(page, n => document.querySelectorAll(".msg.user").length > n, "post-reload message accepted", 30000, usersBeforeReloadSend);
  await waitFor(page, idleProbe, "post-reload turn finished");
  ok("reloaded page accepts new mutations (fresh per-load identity)");

  console.log("23. ISSUES regression: a second tab sends too (#2)");
  const tab2 = await context.newPage();
  tab2.on("pageerror", e => pageErrors.push("tab2: " + String(e)));
  await tab2.goto(server.url, { waitUntil: "domcontentloaded" });
  await waitFor(tab2, () =>
    document.getElementById("app").classList.contains("on") &&
    document.getElementById("auth").style.display === "none" &&
    document.body.dataset.snapshot === "1", "second tab auto-connected", 20000);
  const usersBeforeTab2 = await tab2.evaluate(() => document.querySelectorAll(".msg.user").length);
  await tab2.fill("#input", "Hello from the second tab");
  await tab2.keyboard.press("Enter");
  await waitFor(tab2, n => document.querySelectorAll(".msg.user").length > n, "second-tab message accepted", 30000, usersBeforeTab2);
  await waitFor(tab2, idleProbe, "second-tab turn finished");
  ok("second tab in the same browser profile accepts mutations");
  await tab2.close();

  console.log("24. ISSUES regression: git file diff opens on click (#17)");
  await page.click("#chipGit");
  await waitFor(page, () => document.getElementById("sheetWrap").classList.contains("on"), "git sheet for diff");
  const clicked = await page.evaluate(() => {
    const row = [...document.querySelectorAll(".git-row")].find(el => el.textContent.includes("notes.md"));
    if (row) { row.click(); return true; }
    return false;
  });
  if (!clicked) fail("git row for notes.md not found");
  await waitFor(page, () => {
    const body = document.getElementById("sheetBody");
    return body.querySelector("pre") && body.textContent.includes("ship the web UI");
  }, "file diff rendered", 15000);
  const diffTitle = await page.evaluate(() => document.getElementById("sheetTitle").textContent);
  if (!diffTitle.includes("notes.md")) fail("diff sheet title does not name the file: " + diffTitle);
  await shot(page, "19-git-diff.png");
  await page.click("#sheetX");
  await waitFor(page, () => !document.getElementById("sheetWrap").classList.contains("on"), "sheet closed after diff");
  ok("git row opens the per-file diff");

  console.log("25. ISSUES regressions: narrow viewport header/strip/palette (#10, #11)");
  const small = await browser.newContext({ viewport: { width: 320, height: 568 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true });
  const spage = await small.newPage();
  spage.on("pageerror", e => pageErrors.push("narrow: " + String(e)));
  await spage.goto(server.url, { waitUntil: "domcontentloaded" });
  if (await spage.evaluate(() => document.getElementById("auth").style.display !== "none")) {
    await spage.fill("#authToken", "e2e-token");
    await spage.click("#authGo");
  }
  await waitFor(spage, () => document.getElementById("app").classList.contains("on"), "narrow page connected", 20000);
  await spage.click("#tbMenu");
  await waitFor(spage, () => document.getElementById("app").classList.contains("side-open"), "narrow sidebar");
  await spage.click("#newChat");
  await waitFor(spage, () => !document.getElementById("app").classList.contains("side-open"), "narrow new chat");
  await sendPrompt(spage, "Narrow viewport check");
  const layout = await spage.evaluate(() => {
    const de = document.documentElement;
    const chips = [...document.querySelectorAll("#topbar .chip")].map(c => c.getBoundingClientRect().right);
    const title = document.getElementById("tbName").getBoundingClientRect();
    return {
      overflow: de.scrollWidth - de.clientWidth,
      clientW: de.clientWidth,
      chipMaxRight: chips.length ? Math.max(...chips) : 0,
      titleWidth: title.width,
      stripRight: document.getElementById("strip").getBoundingClientRect().right,
    };
  });
  if (layout.overflow > 1) fail(`page overflows horizontally at 320px by ${layout.overflow}px`);
  if (layout.chipMaxRight > layout.clientW + 1) fail(`topbar chips reach x=${layout.chipMaxRight.toFixed(0)} at 320px`);
  if (layout.titleWidth < 80) fail(`session title squeezed to ${layout.titleWidth.toFixed(0)}px at 320px`);
  if (layout.stripRight > layout.clientW + 1) fail(`status strip reaches x=${layout.stripRight.toFixed(0)} at 320px`);
  await spage.click("#input");
  await spage.fill("#input", "/");
  await waitFor(spage, () => document.getElementById("palette").classList.contains("on"), "narrow palette");
  const pal = await spage.evaluate(() => {
    const r = document.getElementById("palette").getBoundingClientRect();
    return { top: r.top, bottom: r.bottom, scrollable: document.getElementById("palette").scrollHeight > document.getElementById("palette").clientHeight };
  });
  if (pal.top < 0) fail(`slash palette starts above the viewport at 320x568 (top=${pal.top.toFixed(0)})`);
  if (pal.bottom > 569) fail(`slash palette extends below the viewport (bottom=${pal.bottom.toFixed(0)})`);
  if (!pal.scrollable) fail("slash palette is not scrollable at 320x568");
  await spage.screenshot({ path: path.join(shotsDir, "20-narrow.png") });
  screenshots.push(path.join(shotsDir, "20-narrow.png"));
  ok("narrow viewport: header/strip fit, palette stays in view and scrolls");
  await small.close();

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
