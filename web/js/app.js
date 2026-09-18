import { $, esc, relTime } from "./helpers.js";
import { LS, S, prefs, draft, events } from "./state.js";
import { connect, request, select } from "./protocol.js";
import { highlight, inline, md } from "./markdown.js";
import { blockEl, renderChat } from "./chat.js";
import { imageCache } from "./files.js";
import { setPhase, renderSide, renderStatus, renderChips, renderApproval } from "./status.js";
import { renderPlanSheet } from "./panels.js";
import { renderPlates, uploadAttachment } from "./attachments.js";
import { hidePalette } from "./composer.js";

let renderPending = false;
function renderAll() {
  if (renderPending) return;
  renderPending = true;
  requestAnimationFrame(() => {
    renderPending = false;
    try {
      renderSide(); renderChat(); renderStatus(); renderChips(); renderApproval(); renderPlanSheet();
    } catch (err) {
      console.error("render error", err);
      const el = document.createElement("div");
      el.className = "toast err";
      el.textContent = "render error: " + err.message;
      $("toasts").appendChild(el);
      setTimeout(() => el.remove(), 8000);
    }
  });
}

events.addEventListener("render", renderAll);
events.addEventListener("catalog", renderSide);
events.addEventListener("project", renderChips);
events.addEventListener("phase", e => setPhase(...e.detail));
events.addEventListener("auth", e => showAuth(e.detail));
events.addEventListener("select", hidePalette);

function showAuth(err) {
  $("app").classList.remove("on");
  $("auth").style.display = "flex";
  if (err) $("authError").textContent = err;
  $("authToken").value = S.token;
}
function boot() {
  if (!S.token) { showAuth(); return; }
  $("auth").style.display = "none";
  $("app").classList.add("on");
  connect();
  const saved = LS.get("rope.session", null);
  if (saved) S.selected = saved; // snapshot arrives after subscribe on hello
}
$("authGo").onclick = () => {
  S.token = $("authToken").value.trim();
  if (!S.token) { $("authError").textContent = "Enter the server token"; return; }
  LS.set("rope.token", S.token);
  $("authError").textContent = "";
  S.clientId = null; S.serverId = null;
  $("auth").style.display = "none";
  $("app").classList.add("on");
  connect();
};
$("authToken").addEventListener("keydown", e => { if (e.key === "Enter") $("authGo").click(); });
// Debug/e2e handle: view of the connection state plus the pure helpers the
// e2e suite unit-tests. It lives in the page's main JS world, so the driver
// reaches it through CDP Runtime.evaluate (the default evaluate world cannot
// see this script's globals).
const debugHandle = {
  get state() { return S; },
  get prefs() { return prefs; },
  get attachments() { return draft.attachments; },
  set attachments(v) { draft.attachments = v; },
  highlight, inline, md, esc, blockEl,
  request, select,
  renderAll, renderPlates,
  uploadAttachment: (a, session) => uploadAttachment(a, session),
  imgCache: () => [...imageCache.entries()].map(([k, u]) => [k.split("\u0001"), u]),
  // The session the chat DOM has been reconciled for; lags state by one frame.
  chatSession: () => document.getElementById("chatInner")._session,
  // Unit-test relTime against a frozen main-world clock.
  relTime: ts => relTime(ts),
  relTimeAt: (now, ts) => {
    const real = Date.now;
    Date.now = () => now;
    try { return relTime(ts); } finally { Date.now = real; }
  },
};
window.__rope = debugHandle;
boot();
