import { $, esc, toast } from "./helpers.js";
import { LS, S, prefs, savePrefs, draft } from "./state.js";
import { request, command, commandTo, select } from "./protocol.js";
import { updateSendBtn, newSession } from "./status.js";
import { clearAttachments, uploadAttachment, closeCamera } from "./attachments.js";
import { openSheet, closeSheet, fillPlanBody } from "./panels.js";
import { openSearch, closeSearch } from "./search.js";
import { renderChat } from "./chat.js";

let history = LS.get("rope.history", []);
const saveHistory = () => LS.set("rope.history", history.slice(0, 100));

let palIndex = -1;

const input = $("input");
input.addEventListener("input", () => {
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 160) + "px";
  maybePalette();
  updateSendBtn();
});
input.addEventListener("keydown", e => {
  if (e.key === "Escape" && palIndex >= 0) {
    e.preventDefault();
    // Dismissing the palette must not bubble up to the document handler,
    // which would cancel the active turn as well.
    e.stopPropagation();
    hidePalette();
    return;
  }
  if (e.key === "ArrowDown" && palIndex >= 0) { e.preventDefault(); palMove(1); return; }
  if (e.key === "ArrowUp" && palIndex >= 0) { e.preventDefault(); palMove(-1); return; }
  if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    pressEnter();
    return;
  }
  if (e.key === "ArrowUp" && !input.value && history.length) {
    e.preventDefault(); histMove(1); return;
  }
  if (e.key === "ArrowDown" && histPos > 0) {
    e.preventDefault(); histMove(-1); return;
  }
});
let histPos = 0;
function histMove(dir) {
  if (dir > 0) histPos = Math.min(histPos + 1, history.length);
  else histPos = Math.max(histPos - 1, 0);
  input.value = histPos ? history[histPos - 1] : "";
  input.style.height = "auto";
  input.style.height = Math.min(input.scrollHeight, 160) + "px";
  input.selectionStart = input.selectionEnd = input.value.length;
}
// One submission dispatcher for Enter and the send button: a selected slash
// command runs, otherwise the message goes out (a steer while a turn is
// running). Both paths must never differ.
function pressEnter() {
  if (palIndex >= 0) {
    const item = $("palette").children[palIndex];
    const cmd = item?.querySelector(".c")?.textContent || "/";
    runSlash(cmd, input.value.slice(cmd.length));
    return;
  }
  send();
}
function cancelTurn(silent) {
  const st = S.selected && S.sessions.get(S.selected)?.state;
  if (!st?.turn_id) return;
  command({ type: "cancel", turn_id: st.turn_id }).catch(e => { if (!silent) toast(e.message, "err"); });
}
$("sendBtn").onclick = pressEnter;
$("cancelBtn").onclick = () => cancelTurn(false);
document.addEventListener("keydown", e => {
  if (e.key === "Escape") {
    // One priority order: close the topmost overlay first, otherwise cancel
    // the active turn — even while the composer has focus.
    if ($("viewer").classList.contains("on")) { $("viewer").classList.remove("on"); return; }
    if ($("camera").classList.contains("on")) { closeCamera(); return; }
    if ($("sheetWrap").classList.contains("on")) { closeSheet(); return; }
    if ($("searchbar").classList.contains("on")) { closeSearch(); return; }
    if (palIndex >= 0) { hidePalette(); return; }
    cancelTurn(true);
  }
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "f") { e.preventDefault(); openSearch(); }
});
async function send() {
  const text = input.value;
  if (!text.trim() && !draft.attachments.length) return;
  if (S.socket?.readyState !== WebSocket.OPEN) { toast("Not connected", "err"); return; }
  const content = text.trim();
  const keptPlates = draft.attachments;
  // Capture the destination once, up front: the uploads and the message must
  // all land in the same session, even if the user switches conversations
  // while the uploads are still in flight.
  let session = S.selected;
  try {
    if (!session) {
      const { session_id } = await request({ type: "create_session", name: null });
      select(session_id);
      session = session_id;
    }
    for (const a of keptPlates) {
      if (a.pathSession !== session) a.path = (await uploadAttachment(a, session)).path;
    }
    const atts = keptPlates.map(a => a.path);
    pushHistory(content);
    await commandTo(session, { type: "send_message", content, attachments: atts });
    if (S.selected === session) {
      input.value = ""; histPos = 0;
      input.style.height = "auto";
      if (draft.attachments === keptPlates) clearAttachments();
    }
  } catch (e) {
    toast(e.message, "err");
  }
}
function pushHistory(t) {
  t = t.trim();
  if (!t) return;
  if (history[0] !== t) { history.unshift(t); saveHistory(); }
}

const COMMANDS = [
  { c: "/new", d: "Start a new conversation (/new my name names it)" },
  { c: "/compact", d: "Compact the conversation context" },
  { c: "/model", d: "Choose model & reasoning effort" },
  { c: "/thinking", d: "Toggle thinking blocks (/thinking off)" },
  { c: "/tools", d: "Toggle tool blocks (/tools off)" },
  { c: "/plan", d: "Show the plan" },
  { c: "/git", d: "Show project git status" },
  { c: "/search", d: "Search this conversation" },
  { c: "/sound", d: "Toggle completion sound (/sound off)" },
  { c: "/clear", d: "Clear the input" },
  { c: "/help", d: "Show commands" },
];
// Constrain the palette to the space above the composer and let the items
// scroll; recompute when the visual viewport changes (soft keyboard, etc.).
function fitPalette() {
  const vh = (window.visualViewport && window.visualViewport.height) || window.innerHeight;
  const top = $("composer").getBoundingClientRect().top;
  const avail = vh - top - 16;
  document.documentElement.style.setProperty("--pal-max", Math.max(120, avail) + "px");
}
if (window.visualViewport) window.visualViewport.addEventListener("resize", fitPalette);
window.addEventListener("resize", fitPalette);
function maybePalette() {
  const v = input.value;
  if (!v.startsWith("/") || input.selectionStart !== input.selectionEnd) { hidePalette(); return; }
  const items = v === "/"
    ? COMMANDS
    : COMMANDS.filter(c => v.includes(" ") ? c.c === v.slice(0, v.indexOf(" ")) : c.c.startsWith(v));
  if (!items.length) { hidePalette(); return; }
  const pal = $("palette");
  pal.innerHTML = "";
  palIndex = 0;
  items.forEach((c, i) => {
    const el = document.createElement("div");
    el.className = "pal-item" + (i === 0 ? " sel" : "");
    el.innerHTML = `<span class="c">${esc(c.c)}</span><span class="d">${esc(c.d)}</span>`;
    el.onclick = () => runSlash(c.c, v.slice(c.c.length));
    pal.appendChild(el);
  });
  pal.onitems = items;
  fitPalette();
  pal.classList.add("on");
}
function palMove(dir) {
  const pal = $("palette");
  const items = pal.onitems || [];
  palIndex = (palIndex + dir + items.length) % items.length;
  [...pal.children].forEach((el, i) => el.classList.toggle("sel", i === palIndex));
  pal.children[palIndex]?.scrollIntoView({ block: "nearest" });
}
function hidePalette() { $("palette").classList.remove("on"); palIndex = -1; }
async function runSlash(cmd, rest) {
  hidePalette();
  input.value = "";
  input.style.height = "auto";
  const arg = rest.trim();
  if (cmd === "/new") newSession(arg || null);
  else if (cmd === "/compact") {
    const st = S.selected && S.sessions.get(S.selected)?.state;
    if (!st) toast("Open a conversation first");
    else command({ type: "compact" }).catch(e => toast(e.message, "err"));
  }
  else if (cmd === "/model") $("chipModel").click();
  else if (cmd === "/thinking") prefs.showThinking = arg === "off" ? false : !prefs.showThinking;
  else if (cmd === "/tools") prefs.showTools = arg === "off" ? false : !prefs.showTools;
  else if (cmd === "/sound") prefs.sound = arg === "off" ? false : !prefs.sound;
  else if (cmd === "/plan") openSheet("Plan", b => fillPlanBody(b, S.selected && S.sessions.get(S.selected)?.plan));
  else if (cmd === "/git") $("chipGit").click();
  else if (cmd === "/search") openSearch();
  else if (cmd === "/clear") { clearAttachments(); }
  else if (cmd === "/help") openSheet("Commands", b => {
    for (const c of COMMANDS) { const row = document.createElement("div"); row.className = "plan-row pending"; row.innerHTML = `<span class="ic" style="width:auto;border:0;font-family:var(--mono);font-size:12px;color:var(--accent)">${esc(c.c)}</span><span>${esc(c.d)}</span>`; b.appendChild(row); }
  });
  if (cmd === "/thinking" || cmd === "/tools" || cmd === "/sound") {
    savePrefs(); renderChat(); toast(`${cmd.slice(1)}: ${prefs[cmd.slice(1)]} ${prefs[cmd.slice(1)] ? "on" : "off"}`);
  }
}

export { hidePalette };
