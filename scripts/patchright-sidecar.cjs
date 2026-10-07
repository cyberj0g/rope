"use strict";

const { execFileSync } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");
const readline = require("node:readline");
const { filterCompactRules } = require("@duckduckgo/autoconsent");
const { chromium } = require("patchright-core");
const compactRules = require("@duckduckgo/autoconsent/rules/compact-rules.json");

const autoconsentScript = fs.readFileSync(
  path.join(path.dirname(require.resolve("@duckduckgo/autoconsent")), "autoconsent.playwright.js"),
  "utf8",
);
const autoconsentConfig = {
  enabled: true,
  autoAction: "optOut",
  disabledCmps: [],
  enablePrehide: true,
  enableCosmeticRules: true,
  enableGeneratedRules: true,
  detectRetries: 20,
  isMainWorld: false,
  prehideTimeout: 2_000,
  enableHeuristicDetection: true,
  heuristicMode: "reject",
  logs: { errors: false },
};

const profilePath = process.argv[2];
const browserPath = process.argv[3];
const persistentPath = process.argv[4] || null;
const flags = process.argv.slice(5);
const headful = flags.includes("--headful");
const loginUrl = flags.includes("--login") ? flags[flags.indexOf("--login") + 1] : null;

// State worth keeping in the persistent profile: site storage and
// preferences. Cookies travel separately as a Playwright jar snapshot
// (see saveCookies), and everything else (caches, crash data) stays
// per-session.
const persistentFiles = ["Default/Preferences"];
const persistentDirs = ["Default/Local Storage"];

async function syncProfile(from, to, label) {
  if (!persistentPath) return;
  try {
    for (const name of persistentFiles) {
      const source = path.join(from, name);
      if (!fs.existsSync(source)) continue;
      const target = path.join(to, name);
      fs.mkdirSync(path.dirname(target), { recursive: true });
      fs.copyFileSync(source, target);
    }
    for (const name of persistentDirs) {
      const source = path.join(from, name);
      if (!fs.existsSync(source)) continue;
      fs.cpSync(source, path.join(to, name), { recursive: true });
    }
  } catch (error) {
    console.error(`${label} failed: ${errorText(error)}`);
  }
}

// Session cookies (e.g. a Reddit login session) never reach the on-disk
// profile: a graceful browser close deletes them. Serialize the full cookie
// jar through Playwright instead and re-inject it on the next launch.
const cookieStore = () => persistentPath && path.join(persistentPath, "rope-cookies.json");

async function saveCookies(context) {
  const store = cookieStore();
  if (!store) return;
  try {
    fs.writeFileSync(store, JSON.stringify(await context.cookies()));
  } catch (error) {
    console.error(`cookie export failed: ${errorText(error)}`);
  }
}

async function loadCookies(context) {
  const store = cookieStore();
  if (!store || !fs.existsSync(store)) return;
  try {
    await context.addCookies(JSON.parse(fs.readFileSync(store, "utf8")));
  } catch (error) {
    console.error(`cookie import failed: ${errorText(error)}`);
  }
}

const active = new Map();
const pending = new Set();
const cancelled = new Set();
const consentRuns = new WeakMap();
const browserHooks = [];
let closing = false;

function send(message) {
  process.stdout.write(`${JSON.stringify(message)}\n`);
}

function errorText(error) {
  return error instanceof Error ? error.message : String(error);
}

function chromeUserAgent() {
  const output = execFileSync(browserPath, ["--version"], {
    encoding: "utf8",
    timeout: 5_000,
    windowsHide: true,
  });
  const versions = output.match(/\d+(?:\.\d+){1,3}/g);
  if (!versions?.length) throw new Error(`cannot read browser version from: ${output.trim()}`);
  const major = versions.at(-1).split(".")[0];
  const platform = {
    darwin: "Macintosh; Intel Mac OS X 10_15_7",
    win32: "Windows NT 10.0; Win64; x64",
  }[process.platform] || "X11; Linux x86_64";
  return `Mozilla/5.0 (${platform}) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/${major}.0.0.0 Safari/537.36`;
}

function createConsentRun() {
  let detected;
  let settled;
  return {
    detected: new Promise(resolve => { detected = resolve; }),
    settled: new Promise(resolve => { settled = resolve; }),
    markDetected: () => detected(true),
    markSettled: () => {
      detected(false);
      settled();
    },
  };
}

function consentRun(page) {
  let run = consentRuns.get(page);
  if (!run) {
    run = createConsentRun();
    consentRuns.set(page, run);
  }
  return run;
}

async function sendAutoconsentMessage(frame, message) {
  await frame.evaluate(message => globalThis.autoconsentReceiveMessage?.(message), message).catch(() => {});
}

async function handleAutoconsentMessage({ frame, page }, message) {
  if (!page || !message || typeof message !== "object") return;

  const mainFrame = frame.parentFrame() === null;
  if (message.type === "init") {
    const url = message.url || frame.url();
    if (mainFrame && /^https?:/.test(url)) {
      consentRuns.set(page, createConsentRun());
    }
    const enabled = /^https?:/.test(url);
    const rules = enabled
      ? { compact: filterCompactRules(compactRules, { url, mainFrame }) }
      : { autoconsent: [] };
    await sendAutoconsentMessage(frame, {
      type: "initResp",
      config: { ...autoconsentConfig, enabled },
      rules,
    });
    return;
  }

  const run = consentRun(page);
  if (message.type === "cmpDetected" || message.type === "popupFound") {
    run.markDetected();
  } else if (
    message.type === "autoconsentDone"
    || message.type === "optOutResult"
    || (message.type === "report" && ["nothingDetected", "done", "optOutFailed", "optOutSucceeded"].includes(message.state?.lifecycle))
  ) {
    run.markSettled();
  } else if (message.type === "eval") {
    const result = await frame.evaluate(message.code);
    await sendAutoconsentMessage(frame, { type: "evalResp", id: message.id, result });
  }
}

async function waitForAutoconsent(page) {
  const run = consentRun(page);
  const detected = await Promise.race([
    run.detected,
    page.waitForTimeout(750).then(() => false),
  ]);
  if (detected) {
    await Promise.race([run.settled, page.waitForTimeout(6_000)]);
  }
}

async function injectAutoconsent(frame) {
  await frame.evaluate(autoconsentScript).catch(() => {});
}

async function extract(page) {
  await waitForAutoconsent(page);
  return page.evaluate(() => {
    const root = document.querySelector("main")
      || document.querySelector("article")
      || document.querySelector('[role="main"]')
      || document.body;
    const excluded = "script, style, noscript, svg, nav, aside, footer, [hidden], [aria-hidden=\"true\"]";
    const containers = "li, blockquote, pre, td, th, tr";
    const text = element => element.innerText.replace(/\s+/g, " ").trim();
    const visible = element => {
      if (!element || element.closest(excluded) || !text(element)) return false;
      const style = getComputedStyle(element);
      return style.display !== "none"
        && style.visibility !== "hidden"
        && style.visibility !== "collapse"
        && Number(style.opacity) !== 0;
    };
    const blocks = [...root.querySelectorAll("h1, h2, h3, h4, h5, h6, p, pre, li, blockquote, dt, dd, tr")]
      .filter(element => visible(element) && !element.parentElement.closest(containers))
      .map(element => ({
        tag: element.tagName.toLowerCase(),
        text: element.tagName === "TR"
          ? [...element.querySelectorAll(":scope > th, :scope > td")].map(text).join(" | ")
          : text(element),
      }));
    const links = [...root.querySelectorAll("a[href]")]
      .filter(visible)
      .map(element => ({ text: text(element), url: element.href }));
    return {
      title: document.title,
      url: location.href,
      html: document.documentElement.outerHTML,
      visible_text: root.innerText,
      blocks,
      links,
    };
  });
}

async function load(context, request) {
  let page;
  pending.add(request.id);
  try {
    page = await context.newPage();
    page.on("framenavigated", frame => void injectAutoconsent(frame));
    pending.delete(request.id);
    if (cancelled.delete(request.id)) return;
    active.set(request.id, page);
    await page.goto(request.url, { waitUntil: "load", timeout: 20_000 });
    await Promise.all(page.frames().map(injectAutoconsent));
    send({ id: request.id, result: await extract(page) });
  } catch (error) {
    send({ id: request.id, error: errorText(error) });
  } finally {
    pending.delete(request.id);
    cancelled.delete(request.id);
    active.delete(request.id);
    await page?.close().catch(() => {});
  }
}

async function close(context, id) {
  if (closing) return;
  closing = true;
  await saveCookies(context);
  await Promise.all([...active.values()].map(page => page.close().catch(() => {})));
  await context.close().catch(() => {});
  syncProfile(profilePath, persistentPath, "profile export");
  if (id !== undefined) send({ id, result: true });
  process.exit(0);
}

async function browserOptions() {
  return {
    executablePath: browserPath,
    headless: !headful,
    userAgent: chromeUserAgent(),
    viewport: null,
  };
}

async function runLogin() {
  // Sign-in mode operates directly on the persistent profile: whatever the
  // user does in the window (logins, cookies) is saved by closing it.
  const context = await chromium.launchPersistentContext(profilePath, await browserOptions());
  // The window can die at any moment and the graceful close drops session
  // cookies, so keep a fresh jar snapshot while the browser is alive.
  const snapshot = setInterval(() => void saveCookies(context), 1_000);
  const closeBrowser = () => void context.close().catch(() => {});
  process.on("SIGINT", closeBrowser);
  process.on("SIGTERM", closeBrowser);
  await loadCookies(context);
  try {
    const page = context.pages()[0] ?? (await context.newPage());
    await page
      .goto(loginUrl, { waitUntil: "domcontentloaded", timeout: 30_000 })
      .catch(error => console.error(`login page warning: ${errorText(error)}`));
    console.error("Browser is open. Sign in, then close the browser window (or press Ctrl-C) to save the profile and exit.");
    await new Promise(resolve => context.once("close", resolve));
  } finally {
    clearInterval(snapshot);
    await context.close().catch(() => {});
  }
}

async function main() {
  if (loginUrl) {
    await runLogin();
    return;
  }
  syncProfile(persistentPath, profilePath, "profile import");
  const context = await chromium.launchPersistentContext(profilePath, await browserOptions());
  await loadCookies(context);
  browserHooks.push(await context.exposeBinding("autoconsentSendMessage", handleAutoconsentMessage));
  send({ ready: true });

  const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
  lines.on("line", line => {
    let request;
    try {
      request = JSON.parse(line);
    } catch (error) {
      send({ error: `invalid request: ${errorText(error)}` });
      return;
    }
    if (request.method === "load") {
      void load(context, request);
    } else if (request.method === "cancel") {
      const page = active.get(request.id);
      if (page) {
        void page.close().catch(() => {});
      } else if (pending.has(request.id)) {
        cancelled.add(request.id);
      }
    } else if (request.method === "shutdown") {
      void close(context, request.id);
    } else {
      send({ id: request.id, error: `unknown method: ${request.method}` });
    }
  });
  lines.on("close", () => void close(context));
  process.on("SIGINT", () => void close(context));
  process.on("SIGTERM", () => void close(context));
}

main().catch(error => {
  process.stderr.write(`${errorText(error)}\n`);
  process.exit(1);
});
