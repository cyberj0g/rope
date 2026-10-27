// Real DOM and event execution; deliberately no CSP so it cannot mask XSS.
import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { chromium } from "playwright";

let server, browser, page, origin;
before(async () => {
  server = createServer(async (req, res) => {
    if (req.url === "/") {
      res.setHeader("Content-Type", "text/html");
      res.end('<!doctype html><div id="out"></div>');
      return;
    }
    // Serve only source modules, never node_modules or arbitrary disk paths.
    if (!/^\/js\/(?:vendor\/)?[\w.-]+\.(?:js|mjs)$/.test(req.url)) {
      res.writeHead(404).end(); return;
    }
    try {
      const source = await readFile(new URL(`.${req.url}`, import.meta.url));
      res.setHeader("Content-Type", "text/javascript");
      res.end(source);
    } catch { res.writeHead(404).end(); }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  origin = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch({
    executablePath: process.env.ROPE_E2E_BROWSER || undefined,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
  });
  page = await browser.newPage();
  await page.goto(origin);
  await page.evaluate(async () => { window.renderer = await import("/js/markdown.js"); });
});
after(async () => {
  await browser?.close();
  if (server) await new Promise(resolve => server.close(resolve));
});

const payloads = [
  "[x](https://**https://a/onmouseover=window.__poc=1//**)",
  "[x](https://~~https://a/onmouseover=window.__poc=1//~~)",
  '<img src=x onerror="window.__poc=1">',
  '<svg onload="window.__poc=1"><a href="javascript:alert(1)">x</a></svg>',
  '[x](javascript:alert%281%29)',
  '[x](jav&#x61;script:alert%281%29)',
  '[x](data:text/html;base64,PHNjcmlwdD4=)',
  '[x](https://example.com/"onmouseover="window.__poc=1)',
  '[**https://example.com**](https://example.org)',
  '```html\n<img src=x onerror="window.__poc=1">\n```',
  '`<img src=x onerror="window.__poc=1">`',
];

test("malicious Markdown stays inert, including every streamed prefix", async () => {
  const result = await page.evaluate(payloads => {
    const out = document.getElementById("out");
    window.__poc = 0;
    let checked = 0;
    for (const render of [window.renderer.md, window.renderer.inline]) {
      for (const payload of payloads) {
        for (let n = 1; n <= payload.length; n++) {
          const input = payload.slice(0, n);
          out.innerHTML = render(input);
          for (const el of out.querySelectorAll("*")) {
            if (/^(SCRIPT|IFRAME|OBJECT|EMBED|IMG|SVG|MATH|STYLE|FORM)$/.test(el.tagName))
              throw new Error(`active element for ${input}: ${el.outerHTML}`);
            for (const attr of el.attributes) {
              if (/^on/i.test(attr.name)) throw new Error(`event handler for ${input}: ${el.outerHTML}`);
            }
            if (el.hasAttribute("href") && !/^https?:\/\//i.test(el.getAttribute("href")))
              throw new Error(`unsafe link for ${input}: ${el.outerHTML}`);
            for (const type of ["mouseover", "focus", "error", "load", "click"])
              if (type !== "click" || el.tagName !== "A") el.dispatchEvent(new Event(type));
          }
          checked++;
        }
      }
    }
    return { checked, executed: window.__poc };
  }, payloads);
  assert.ok(result.checked > 1000);
  assert.equal(result.executed, 0);
  // Also exercise a physical hover over the original attack, not only events.
  await page.evaluate(input => { document.getElementById("out").innerHTML = window.renderer.md(input); }, payloads[0]);
  await page.locator("#out a").hover();
  assert.equal(await page.evaluate(() => window.__poc), 0);
});

test("Markdown retains formatting, literal code, safe links and task markers", async () => {
  const result = await page.evaluate(() => {
    const source = '# Title\n\n**bold** and *emphasis* and ~~deleted~~\n\n> quote\n\n- [ ] pending\n- [x] done\n\n3. third\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n```js\nconst x = "<img onerror=x>";\n```\n\n`**literal stars**`\n\n[x](https://example.com "Title") and https://example.org\n\nline one\nline two';
    const out = document.getElementById("out");
    out.innerHTML = window.renderer.md(source);
    return {
      tags: ["h1", "strong", "em", "s", "blockquote", "ul", "ol", "table", "pre code", "br"].filter(t => !out.querySelector(t)),
      tasks: out.querySelectorAll(".task").length,
      done: out.querySelectorAll(".task.done").length,
      start: out.querySelector("ol").getAttribute("start"),
      code: out.querySelector("pre code").textContent,
      language: out.querySelector("pre").dataset.lang,
      highlighted: !!out.querySelector("pre .k"),
      literal: [...out.querySelectorAll("code")].some(c => c.textContent === "**literal stars**"),
      links: [...out.querySelectorAll("a")].map(a => [a.href, a.target, a.rel]),
      title: out.querySelector("a").title,
    };
  });
  assert.deepEqual(result.tags, []);
  assert.equal(result.tasks, 2);
  assert.equal(result.done, 1);
  assert.equal(result.start, "3");
  assert.equal(result.code, 'const x = "<img onerror=x>";');
  assert.equal(result.language, "js");
  assert.equal(result.highlighted, true);
  assert.equal(result.literal, true);
  assert.equal(result.title, "Title");
  assert.deepEqual(result.links, [
    ["https://example.com/", "_blank", "noopener noreferrer"],
    ["https://example.org/", "_blank", "noopener noreferrer"],
  ]);
});

test("Markdown images cannot initiate network requests", async () => {
  const requests = [];
  const listener = req => requests.push(req.url());
  page.on("request", listener);
  await page.evaluate(() => {
    document.getElementById("out").innerHTML = window.renderer.md(
      '![tracking](https://attacker.invalid/pixel) ![local](http://127.0.0.1:9999/private)');
  });
  await page.waitForTimeout(100);
  page.off("request", listener);
  assert.deepEqual(requests, []);
  assert.match(await page.locator("#out").textContent(), /tracking.*local/);
});
