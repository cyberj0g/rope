import { $, esc, fmt, relTime, toast, prettyArgs } from "./helpers.js";
import { S, prefs, draft } from "./state.js";
import { request, command, select, catalogView, deleteSession, CATALOG_PAGE } from "./protocol.js";

function beep() {
  if (!prefs.sound) return;
  try {
    beep.ctx = beep.ctx || new AudioContext();
    if (beep.ctx.state === "suspended") beep.ctx.resume();
    const t = beep.ctx.currentTime;
    for (const [f, at, dur] of [[880, 0, .09], [1318, .11, .16]]) {
      const o = beep.ctx.createOscillator(), g = beep.ctx.createGain();
      o.frequency.value = f; o.type = "sine";
      g.gain.setValueAtTime(.001, t + at);
      g.gain.exponentialRampToValueAtTime(.12, t + at + .015);
      g.gain.exponentialRampToValueAtTime(.001, t + at + dur);
      o.connect(g).connect(beep.ctx.destination);
      o.start(t + at); o.stop(t + at + dur + .05);
    }
  } catch {}
}

function setPhase(phase, text) {
  const el = $("tbPhase");
  el.className = phase;
  $("tbPhaseText").textContent = text || phase;
}
const PHASE_TEXT = { idle: "idle", connecting: "waiting for first response", waiting: "waiting for first response", generating: "generating", tool: "running tools", approval: "needs approval", compacting: "compacting", retrying: "retrying", error: "error" };

let sideSig = "";
function renderSide() {
  const list = $("sideList");
  const filter = S.catalogQuery;
  const sig = S.catalog.map(s => [s.name, s.title, s.activity, s.total_tokens, s.total_cost].join(",")).join(";")
    + "|" + filter + "|" + S.selected + "|" + S.confirmDelete + "|" + S.catalogTotal;
  if (sig === sideSig) return;
  sideSig = sig;
  // The list is already filtered and paginated by the server.
  const entries = S.catalog;
  list.innerHTML = "";
  if (!entries.length) {
    list.innerHTML = `<div class="side-empty">${filter ? "No matching conversations" : "No conversations yet.<br>Start a new chat."}</div>`;
  }
  for (const s of entries) {
    // Rows are divs (not buttons): they contain their own delete/confirm
    // buttons, which would be invalid nested interactive content.
    const btn = document.createElement("div");
    btn.className = "side-item" + (s.name === S.selected ? " on" : "");
    btn.dataset.session = s.name;
    btn.setAttribute("role", "button");
    btn.tabIndex = 0;
    btn.onkeydown = e => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); btn.onclick?.(e); } };
    const title = s.title || s.name;
    if (S.confirmDelete === s.name) {
      btn.className += " confirming";
      btn.innerHTML = `<span class="t"><span class="d ${s.activity}"></span>${esc(title)}</span>
        <span class="m confirm">Delete this conversation?</span>
        <span class="m yesno">
          <button class="ybad" data-del="1">Delete</button>
          <button class="yok" data-canc="1">Cancel</button>
        </span>`;
      btn.onclick = e => {
        const t = e.target.closest("[data-del],[data-canc]");
        if (t?.dataset.del) { confirmDelete(s.name); }
        else { S.confirmDelete = null; renderSide(); }
      };
    } else {
      const meta = [s.activity === "idle" ? relTime(s.created_at) : s.activity,
        s.total_tokens ? `${fmt(s.total_tokens)} tok` : "",
        s.total_cost != null ? `$${s.total_cost.toFixed(4)}` : ""].filter(Boolean).join(" · ");
      btn.innerHTML = `<span class="t"><span class="d ${s.activity}"></span>${esc(title)}</span><span class="m">${esc(meta)}</span>
        <button class="side-del" aria-label="Delete conversation" data-del="1">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/></svg>
        </button>`;
      btn.onclick = e => {
        if (e.target.closest("[data-del]")) { e.stopPropagation(); S.confirmDelete = s.name; renderSide(); return; }
        select(s.name); $("app").classList.remove("side-open");
      };
    }
    list.appendChild(btn);
  }
  // "Load more" when the server has more rows matching the current filter.
  const more = $("sideMore");
  if (more) more.hidden = !(S.catalogTotal > entries.length);
  $("sideFoot").textContent = S.projectRoot;
}

// Delete a conversation. If it was selected, fall back to the next row.
async function confirmDelete(name) {
  S.confirmDelete = null;
  const wasSelected = S.selected === name;
  try {
    await deleteSession(name);
    if (wasSelected) {
      const next = S.catalog.find(s => s.name !== name) || null;
      select(next ? next.name : null);
      if (next) S.sessions.delete(name);
    } else {
      S.sessions.delete(name);
    }
    toast("Conversation deleted");
    renderSide();
  } catch (e) {
    toast(e.message, "err");
  }
}

let filterTimer = null;
function onSideFilter() {
  const q = $("sideFilter").value.trim();
  if (filterTimer) clearTimeout(filterTimer);
  // Debounce so we do not fire a server request per keystroke.
  filterTimer = setTimeout(() => {
    S.catalogQuery = q;
    catalogView(q, CATALOG_PAGE);
  }, 180);
}
function renderStatus() {
  const snap = S.selected && S.sessions.get(S.selected);
  const st = snap?.state;
  if (!st) {
    if (S.selected) setPhase("connecting", "loading…");
    else setPhase("idle", S.socket?.readyState === WebSocket.OPEN ? "open a conversation" : "connecting…");
    document.body.dataset.snapshot = snap ? "1" : "0";
    document.body.dataset.phase = "none";
    document.body.dataset.turn = "";
    updateSendBtn();
    return;
  }
  document.body.dataset.snapshot = "1";
  document.body.dataset.phase = st.phase;
  document.body.dataset.turn = st.turn_id || "";
  const prevPhase = snap._lastPhase || st.phase;
  if (prevPhase !== st.phase) {
    if ((st.phase === "idle" && !st.compacting) || st.phase === "error") beep();
    snap._lastPhase = st.phase;
  }
  setPhase(st.phase, st.notice || st.error || PHASE_TEXT[st.phase] || st.phase);
  $("tbName").textContent = st.title || S.selected || "Rope";
  // status strip: the active model plus its current reasoning effort
  $("stripModel").textContent = st.reasoning_effort
    ? `${st.model} · ${st.reasoning_effort}`
    : st.model;
  $("stripTokens").textContent = `${fmt(st.total_tokens)} tok`;
  $("stripCost").textContent = st.total_cost != null ? `$${st.total_cost.toFixed(4)}` : "";
  const pct = st.max_context_tokens ? Math.min(100, st.context_tokens / st.max_context_tokens * 100) : 0;
  const fill = $("ctxFill");
  fill.style.width = pct + "%";
  fill.className = pct > 90 ? "hot" : pct > 75 ? "warn" : "";
  $("ctxText").textContent = `${fmt(st.context_tokens)}/${fmt(st.max_context_tokens)}`;
  const speed = $("stripSpeed");
  if ((st.phase === "generating" || st.phase === "tool" || st.phase === "waiting") && st.generation_ms > 0) {
    speed.textContent = `${(st.output_tokens / (st.generation_ms / 1000)).toFixed(0)} tok/s`;
  } else if (st.phase === "idle" && st.generation_ms > 0 && st.output_tokens > 0) {
    speed.textContent = `${(st.output_tokens / (st.generation_ms / 1000)).toFixed(0)} tok/s avg`;
  } else speed.textContent = "";
  // composer
  updateSendBtn();
  // model chip
  $("chipModelText").textContent = st.model || "model";
  $("chipAgentText").textContent = S.agents.find(a => a.id === (st.agent || "assistant"))?.name || st.agent || "Assistant";
  $("tbParent").hidden = !snap.parent;
  $("tbParent").title = snap.parent ? `Back to ${snap.parent.session}` : "";
  $("tbParent").onclick = () => snap.parent && select(snap.parent.session);
  renderChips();
  renderApproval();
}
// During a turn the composer offers two distinct actions: send the draft as a
// steer (enabled by text or attachments) and cancel the turn.
function updateSendBtn() {
  const st = S.selected && S.sessions.get(S.selected)?.state;
  const running = !!st?.turn_id;
  const hasDraft = $("input").value.trim().length > 0 || draft.attachments.length > 0;
  const btn = $("sendBtn");
  btn.disabled = !hasDraft;
  btn.setAttribute("aria-label", running ? "Send steering message" : "Send");
  $("cancelBtn").hidden = !running;
  $("hintRight").innerHTML = running ? '<kbd>Esc</kbd> cancel' : "";
  $("hintLeft").textContent = running
    ? (st?.delegation ? `steering subagent · ${st.queued_steers || 0} queued` : `steering · ${st?.queued_steers || 0} queued`)
    : "";
}
function renderChips() {
  const snap = S.selected && S.sessions.get(S.selected);
  const plan = snap?.plan;
  $("chipPlan").hidden = !plan || !plan.plan.length;
  if (plan) {
    const done = plan.plan.filter(s => s.status === "completed").length;
    $("chipPlanText").textContent = `Plan ${done}/${plan.plan.length}`;
  }
  const proj = snap?.project || S.project;
  $("chipGit").hidden = !proj || !proj.git_available || !proj.git_files.length;
  if (proj && proj.git_available && proj.git_files.length) {
    $("chipGitText").textContent = `Git ${proj.git_files.length}`;
  }
}
function renderApproval() {
  const st = S.selected && S.sessions.get(S.selected)?.state;
  const a = st?.approval;
  $("approval").classList.toggle("on", !!a);
  if (a) {
    $("apprTitle").textContent = `${a.call.name} wants to run`;
    $("apprArgs").textContent = prettyArgs(JSON.stringify(a.call.arguments));
  }
}
document.querySelectorAll("#approval [data-decision]").forEach(b => b.onclick = () => {
  const st = S.selected && S.sessions.get(S.selected)?.state;
  if (!st?.approval) return;
  command({ type: "approve", turn_id: st.turn_id, approval_id: st.approval.id, decision: b.dataset.decision })
    .catch(e => toast(e.message, "err"));
});

$("tbMenu").onclick = () => $("app").classList.toggle("side-open");
$("sideBackdrop").onclick = () => $("app").classList.remove("side-open");
$("sideFilter").oninput = onSideFilter;
// "Load more" widens the server-side window by one more page.
const sideMore = $("sideMore");
if (sideMore) sideMore.onclick = () => catalogView(S.catalogQuery, S.catalog.length + CATALOG_PAGE);
$("newChat").onclick = () => newSession(null);
async function newSession(name) {
  try {
    const { session_id } = await request({ type: "create_session", name });
    select(session_id);
    $("app").classList.remove("side-open");
    $("input").focus();
  } catch (e) { toast(e.message, "err"); }
}

export { setPhase, renderSide, renderStatus, renderChips, renderApproval, updateSendBtn, newSession };
