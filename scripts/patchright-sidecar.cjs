"use strict";

const { execFileSync } = require("node:child_process");
const readline = require("node:readline");
const { chromium } = require("patchright-core");

const profilePath = process.argv[2];
const browserPath = process.argv[3];
const active = new Map();
const pending = new Set();
const cancelled = new Set();
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

async function extract(page) {
  await page.waitForTimeout(750);
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
    pending.delete(request.id);
    if (cancelled.delete(request.id)) return;
    active.set(request.id, page);
    await page.goto(request.url, { waitUntil: "load", timeout: 20_000 });
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
  await Promise.all([...active.values()].map(page => page.close().catch(() => {})));
  await context.close().catch(() => {});
  send({ id, result: true });
  process.exit(0);
}

async function main() {
  const context = await chromium.launchPersistentContext(profilePath, {
    executablePath: browserPath,
    headless: true,
    userAgent: chromeUserAgent(),
    viewport: null,
  });
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
