import { $, esc, prettyArgs, fmtDur } from "./helpers.js";
import { S, prefs, events } from "./state.js";
import { md, plain } from "./markdown.js";
import { renderImages, renderFile } from "./files.js";
import { openRaw } from "./raw.js";
import { openDiffSheet } from "./panels.js";
import { search, applySearch } from "./search.js";
import { revealBlock } from "./protocol.js";

function blockKey(b) {
  const t = b.tool;
  return (prefs.showThinking ? "T" : "") + (prefs.showTools ? "L" : "") +
    [b.kind, b.content, b.queued, b.model, b.summary ? 1 : 0,
    JSON.stringify(b.file || null),
    b.images.map(i => (i.path || "") + " " + i.width + "x" + i.height).join(","),
    t ? [t.name, t.status, t.arguments, t.output || "", t.diff || "", t.redacted ? 1 : 0] : "",
    b.redacted ? "R" : "",
    b.timer.running ? "r" : b.timer.elapsed_ms].join("\u0001");
}
// Whether the section's content is withheld from the wire and must be
// requested on first expand.
function isRedacted(b) {
  return (b.kind === "thinking" && !!b.redacted) ||
    (b.kind === "tool" && !!(b.tool && b.tool.redacted));
}

// First expand of a redacted section: show a loading plate and fetch the
// full block from the server, which then keeps streaming its live updates.
async function maybeReveal(session, blockId, section) {
  const snap = S.sessions.get(session);
  const b = snap?.blocks.find(x => x.id === blockId);
  if (!snap || !b || !isRedacted(b) || b._revealing) return;
  b._revealing = true;
  const body = section._body;
  body.innerHTML = `<div class="loading"><span class="spin"></span>loading…</div>`;
  try {
    const res = await revealBlock(session, blockId);
    const i = snap.blocks.findIndex(x => x.id === blockId);
    if (i >= 0) snap.blocks[i] = res.block;
    events.dispatchEvent(new Event("render"));
  } catch (e) {
    b._revealing = false; // let the next expand retry
    body.innerHTML = `<div class="loading err">${esc(e.message)}</div>`;
  }
}

function sectionEl(kind, label, timer, extraClass) {
  const d = document.createElement("details");
  d.className = `sec ${kind} ${extraClass || ""}`;
  const s = document.createElement("summary");
  s.innerHTML = `<svg class="chev" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="m9 6 6 6-6 6"/></svg><span class="tag">${esc(label)}</span><span class="meta"><span class="timer"></span></span>`;
  const body = document.createElement("div");
  body.className = "body";
  d.append(s, body);
  d._body = body;
  updateTimer(d, timer);
  return d;
}
function updateTimer(section, timer) {
  const el = section.querySelector(".meta .timer");
  if (el) el.textContent = timer.running ? fmtDur(timer.elapsed_ms) + " …" : fmtDur(timer.elapsed_ms);
}
// A collapsed section still shows its work in progress: the header carries
// the tool name/status or a running timer, which pulses while active.
function markRunning(section, running) {
  section.dataset.running = running ? "1" : "0";
}

function blockEl(session, b) {
  const wrap = blockContent(session, b);
  if (!wrap.childNodes.length) return wrap;
  const button = document.createElement("button");
  button.className = "raw-button";
  button.type = "button";
  button.textContent = "[raw]";
  button.setAttribute("aria-label", "View raw model request");
  button.onclick = e => { e.preventDefault(); e.stopPropagation(); openRaw(session, b.id); };
  const header = wrap.querySelector(".who, summary, .sys");
  if (header) header.append(button); else wrap.prepend(button);
  return wrap;
}

function blockContent(session, b) {
  const wrap = document.createElement("div");
  if (b.kind === "user" || b.kind === "steer") {
    wrap.className = `msg ${b.kind}`;
    const bubble = document.createElement("div");
    bubble.className = "bubble md";
    bubble.innerHTML =
      (b.kind === "steer" && b.queued ? `<span class="badge-queued">queued</span>` : "") +
      (md(b.content) || (b.content ? plain(b.content) : ""));
    renderImages(bubble, session, b.images);
    wrap.appendChild(bubble);
    return wrap;
  }
  if (b.kind === "assistant" || b.kind === "status") {
    wrap.className = "msg assistant";
    const who = document.createElement("div");
    who.className = b.kind === "status" ? "who status" : "who";
    who.innerHTML = `${b.kind === "status" ? "Status" : "Assistant"}${b.model ? `<span class="mdl">${esc(b.model)}</span>` : ""}${b.queued ? `<span class="badge-queued">queued</span>` : ""}`;
    const bubble = document.createElement("div");
    bubble.className = "bubble md";
    bubble.innerHTML = md(b.content);
    wrap.append(who, bubble);
    return wrap;
  }
  if (b.kind === "thinking") {
    wrap.className = "msg thinking";
    if (!prefs.showThinking) return wrap;
    const d = sectionEl("thinking", "Thinking", b.timer);
    markRunning(d, b.timer.running);
    if (b.redacted) {
      // Content is withheld while collapsed; the running timer in the
      // header still shows the model is thinking.
      d._body.innerHTML = "";
      d.addEventListener("toggle", () => { if (d.open) maybeReveal(session, b.id, d); });
    } else {
      d._body.innerHTML = plain(b.content);
    }
    wrap.appendChild(d);
    wrap._timer = d;
    return wrap;
  }
  if (b.kind === "tool") {
    wrap.className = "msg tool";
    renderFile(wrap, session, b);
    if (!prefs.showTools) return wrap;
    const t = b.tool;
    const d = sectionEl("tool", t.name, b.timer);
    d.dataset.status = t.status;
    markRunning(d, ["streaming", "pending", "running", "waiting-approval"].includes(t.status));
    const status = document.createElement("span");
    status.className = "status";
    status.textContent = { streaming: "streaming", pending: "pending", "waiting-approval": "approval", running: "running", done: "done", failed: "failed" }[t.status] || t.status;
    d.querySelector(".meta").prepend(status);
    if (t.redacted) {
      // Arguments and output are withheld while collapsed; the header keeps
      // the tool name and its live status/timer so in-progress work is visible.
      d._body.innerHTML = "";
      d.addEventListener("toggle", () => { if (d.open) maybeReveal(session, b.id, d); });
    } else {
      const args = document.createElement("pre");
      args.className = "toolarg";
      args.textContent = prettyArgs(t.arguments);
      d._body.appendChild(args);
      if (t.output) {
        const lbl = document.createElement("div");
        lbl.className = "lbl";
        lbl.textContent = "Output";
        const out = document.createElement("pre");
        out.className = "toolout plain";
        out.textContent = t.output;
        d._body.append(lbl, out);
      }
      if (t.diff) {
        const lbl = document.createElement("div");
        lbl.className = "lbl";
        lbl.textContent = "Diff";
        const out = document.createElement("pre");
        out.className = "toolout plain";
        out.style.cssText += ";max-height:180px;cursor:pointer;";
        out.textContent = t.diff;
        out.onclick = () => openDiffSheet(`Diff · ${t.name}`, t.diff);
        d._body.append(lbl, out);
      }
    }
    // view_image stays inside the tool details
    renderImages(d._body, session, b.images, false);
    wrap.prepend(d);
    wrap._timer = d;
    return wrap;
  }
  if (b.kind === "system") {
    wrap.className = "msg system";
    const sys = document.createElement("div");
    sys.className = "sys";
    sys.innerHTML = `<span>${esc(b.content || "—")}</span>`;
    if (b.summary) {
      const det = document.createElement("details");
      det.innerHTML = `<summary>Summary</summary>`;
      const pre = document.createElement("pre");
      pre.className = "plain";
      pre.style.cssText = "max-height:200px;overflow:auto;background:var(--bg);border-radius:8px;padding:8px;margin-top:6px;";
      pre.textContent = b.summary;
      det.appendChild(pre);
      sys.appendChild(det);
    }
    wrap.appendChild(sys);
    return wrap;
  }
  if (b.kind === "error") {
    wrap.className = "msg error";
    const who = document.createElement("div");
    who.className = "who";
    who.style.color = "var(--red)";
    who.textContent = "Error";
    const bubble = document.createElement("div");
    bubble.className = "bubble md";
    bubble.innerHTML = md(b.content);
    wrap.append(who, bubble);
    return wrap;
  }
  return wrap;
}

// Follow the bottom until the user scrolls away deliberately.
let stickBottom = true;
events.addEventListener("select", () => { stickBottom = true; });
// Blocks are reconciled by session + block ID: existing elements are kept and
// repositioned (so expanded sections and reading position survive new blocks),
// rebuilt only when their content key changes (carrying open state over), and
// the whole cache is reset when the selected session changes.
function renderChat() {
  const chat = $("chat");
  const inner = $("chatInner");
  const snap = S.selected && S.sessions.get(S.selected);
  if (!snap) {
    if (inner.children.length) inner.innerHTML = "";
    inner._els = null;
    inner._session = null;
    if (inner.children.length === 0) {
      inner.innerHTML = `<div class="empty-hint"><div class="big">◦</div>Pick a conversation or start a new one.<br>Your messages stream in live, with tools, thinking, and plans.</div>`;
    }
    return;
  }
  const blocks = snap.blocks;
  if (inner._session !== S.selected || !inner._els) {
    inner.innerHTML = "";
    inner._els = new Map();
  }
  inner._session = S.selected;
  const live = new Set();
  for (const b of blocks) {
    live.add(b.id);
    const key = blockKey(b);
    let rec = inner._els.get(b.id);
    if (!rec) {
      rec = { key, el: blockEl(S.selected, b) };
      inner._els.set(b.id, rec);
    } else if (key !== rec.key) {
      rec.key = key;
      const fresh = blockEl(S.selected, b);
      if (fresh._timer && rec.el._timer) fresh._timer.open = rec.el._timer.open;
      rec.el.replaceWith(fresh);
      rec.el = fresh;
    }
    if (rec.el.parentNode !== inner) inner.appendChild(rec.el);
  }
  for (const [id, rec] of [...inner._els]) {
    if (!live.has(id)) { rec.el.remove(); inner._els.delete(id); }
  }
  // live cursor while generating
  const gen = snap.state.phase === "generating";
  for (const [id, rec] of inner._els) {
    const b = blocks.find(x => x.id === id);
    if (!b || (b.kind !== "assistant" && b.kind !== "thinking") || b.tool) continue;
    const marker = rec.el.querySelector(".cursor");
    if (gen && b === blocks[blocks.length - 1] && b.content) {
      if (!marker) {
        const c = document.createElement("span");
        c.className = "cursor";
        rec.el.querySelector(".bubble, .body")?.appendChild(c);
      }
    } else marker?.remove();
  }
  if (search.query) applySearch();
  if (stickBottom) chat.scrollTop = chat.scrollHeight;
  updateJump();
}
// Layout changes (approval card, attachment plates) resize the chat area and
// fire scroll events without user intent, so only honor scrolls that follow a
// real wheel/touch/key input.
let userScrollAt = 0;
for (const ev of ["wheel", "touchstart", "pointerdown", "keydown"]) {
  $("chat").addEventListener(ev, () => { userScrollAt = Date.now(); }, { passive: true });
}
$("chat").addEventListener("scroll", () => {
  const away = $("chat").scrollHeight - $("chat").scrollTop - $("chat").clientHeight;
  const byUser = Date.now() - userScrollAt < 150;
  if (byUser && away > 160) stickBottom = false;
  else if (away < 40) stickBottom = true;
});
function updateJump() {
  const chat = $("chat");
  const away = chat.scrollHeight - chat.scrollTop - chat.clientHeight > 120;
  $("jump").classList.toggle("on", away && !!$("chatInner")._els?.size);
}
$("jump").onclick = () => { $("chat").scrollTop = $("chat").scrollHeight; updateJump(); };
$("chat").addEventListener("scroll", updateJump);

/* ticking timers: the snapshot carries the last reported elapsed_ms, so running
   timers advance locally between events */
const localTimers = new Map();
setInterval(() => {
  const snap = S.selected && S.sessions.get(S.selected);
  const inner = $("chatInner");
  if (!snap || !inner._els) return;
  const now = performance.now();
  for (const b of snap.blocks) {
    if (!b.timer.running) { localTimers.delete(b.id); continue; }
    let t = localTimers.get(b.id);
    if (!t) { localTimers.set(b.id, { base: b.timer.elapsed_ms, at: now }); t = localTimers.get(b.id); }
    const ms = t.base + (now - t.at);
    const rec = inner._els.get(b.id);
    if (rec?.el._timer) {
      const el = rec.el._timer.querySelector(".meta .timer");
      if (el) el.textContent = fmtDur(ms) + " …";
    }
  }
}, 500);

export { blockEl, renderChat };
