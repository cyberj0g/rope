import { $, esc, fmt, toast } from "./helpers.js";
import { S } from "./state.js";
import { request, command } from "./protocol.js";

let sheetOpen = null;

function openSheet(title, build) {
  $("sheetTitle").textContent = title;
  const body = $("sheetBody");
  // Every opening starts with an empty body so sheets can never accumulate
  // content from previous openings.
  body.innerHTML = "";
  body.scrollTop = 0;
  build(body);
  $("sheetWrap").classList.add("on");
  sheetOpen = title;
}
function closeSheet() { $("sheetWrap").classList.remove("on"); sheetOpen = null; }
$("sheetX").onclick = closeSheet;
$("sheetBackdrop").onclick = closeSheet;
// A scrollable, literal-text diff view shared by the Git sheet and tool blocks.
function openDiffSheet(title, content) {
  openSheet(title, body => {
    const pre = document.createElement("pre");
    pre.className = "toolout plain";
    pre.style.cssText = "max-height:50dvh;overflow:auto;background:var(--bg);border-radius:8px;padding:10px;white-space:pre;";
    pre.textContent = content || "(empty diff)";
    body.appendChild(pre);
  });
}
function fillPlanBody(body, plan) {
  if (!plan) {
    const note = document.createElement("div");
    note.className = "sheet-note";
    note.textContent = "No active plan.";
    body.appendChild(note);
    return;
  }
  if (plan.explanation) {
    const p = document.createElement("p");
    p.className = "plan-expl";
    p.textContent = plan.explanation;
    body.appendChild(p);
  }
  for (const step of plan.plan) {
    const row = document.createElement("div");
    row.className = `plan-row ${step.status}`;
    row.innerHTML = `<span class="ic">${step.status === "completed" ? '<svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="3.4" stroke-linecap="round"><path d="m4 12.5 5.5 5.5L20 6.5"/></svg>' : ""}</span><span>${esc(step.step)}</span>`;
    body.appendChild(row);
  }
}
// While the Plan sheet is open, keep its rows in sync with the live plan
// (replacing the content rather than appending to it).
function renderPlanSheet() {
  if (sheetOpen !== "Plan") return;
  const body = $("sheetBody");
  body.innerHTML = "";
  fillPlanBody(body, S.selected && S.sessions.get(S.selected)?.plan);
}

$("chipModel").onclick = () => {
  const st = S.selected && S.sessions.get(S.selected)?.state;
  openSheet("Model & reasoning", body => {
    body.innerHTML = `<div class="sheet-search"><input id="modelFilter" placeholder="Search models" autocomplete="off"></div><div id="modelList"></div>
      <div class="sheet-note">Switching models requires the turn to be idle.</div>`;
    const list = $("modelList");
    const draw = f => {
      list.innerHTML = "";
      for (const m of S.models.filter(m => !f || m.name.toLowerCase().includes(f) || m.provider.toLowerCase().includes(f) || m.id.toLowerCase().includes(f))) {
        const row = document.createElement("div");
        row.className = "srow";
        row.innerHTML = `<div class="t"><div class="n">${esc(m.name)}</div><div class="s">${esc(m.provider)} · ${fmt(m.max_context_tokens)} ctx${m.price_per_token != null ? " · priced" : ""}</div></div>
          ${m.vision ? '<span class="badge vision">vision</span>' : ""}${st && m.name === st.model ? '<span class="badge">current</span>' : ""}`;
        row.onclick = () => {
          if (!st) return closeSheet();
          command({ type: "set_model", model: m.name, revision: st.settings_revision })
            .then(closeSheet)
            .catch(e => toast(e.message, "err"));
        };
        list.appendChild(row);
      }
    };
    $("modelFilter").oninput = e => draw(e.target.value.trim().toLowerCase());
    draw("");
    // reasoning
    const efforts = st ? (S.models.find(m => m.name === st.model)?.reasoning_efforts || []).map(e => e.toLowerCase()) : [];
    if (efforts.length) {
      const note = document.createElement("div");
      note.className = "sheet-note";
      note.style.marginTop = "16px";
      note.textContent = "Reasoning effort";
      list.after(note);
      const rwrap = document.createElement("div");
      for (const opt of [["", "Off"], ...efforts.map(e => [e, e])]) {
        const row = document.createElement("div");
        row.className = "srow";
        const cur = (st?.reasoning_effort || "").toLowerCase() === opt[0];
        row.innerHTML = `<div class="t"><div class="n" style="text-transform:capitalize">${opt[1]}</div></div>${cur ? '<span class="badge">current</span>' : ""}`;
        row.onclick = () => {
          command({ type: "set_reasoning", effort: opt[0] || null, revision: st.settings_revision })
            .then(closeSheet)
            .catch(e => toast(e.message, "err"));
        };
        rwrap.appendChild(row);
      }
      note.after(rwrap);
    }
  });
};

$("chipGit").onclick = () => {
  const proj = (S.selected && S.sessions.get(S.selected)?.project) || S.project;
  openSheet("Project", body => {
    if (!proj || !proj.git_available) { body.innerHTML = `<div class="sheet-note">This project is not a git repository.</div>`; return; }
    const note = document.createElement("div");
    note.className = "sheet-note";
    note.textContent = proj.cwd + " — tap a file for its diff";
    body.appendChild(note);
    if (!proj.git_files.length) {
      const clean = document.createElement("div");
      clean.className = "sheet-note";
      clean.textContent = "Working tree clean.";
      body.appendChild(clean);
      return;
    }
    for (const f of proj.git_files) {
      const row = document.createElement("div");
      row.className = "git-row";
      const st = f.status.trim();
      const cls = st[0] === "M" ? "M" : st[0] === "A" ? "A" : st[0] === "D" ? "D" : st[0] === "?" ? "U" : "M";
      row.innerHTML = `<span class="g ${cls}">${esc(st)}</span><span class="p">${esc(f.path)}</span>`;
      row.onclick = () => {
        const p = f.path;
        openSheet("Diff · " + p, diffBody => {
          const loading = document.createElement("div");
          loading.className = "sheet-note";
          loading.textContent = "Loading diff…";
          diffBody.appendChild(loading);
          request({ type: "git_diff", path: p })
            .then(res => {
              loading.remove();
              const pre = document.createElement("pre");
              pre.className = "toolout plain";
              pre.style.cssText = "max-height:50dvh;overflow:auto;background:var(--bg);border-radius:8px;padding:10px;white-space:pre;";
              pre.textContent = res.content || "(no changes)";
              diffBody.appendChild(pre);
            })
            .catch(e => { loading.textContent = e.message; });
        });
      };
      body.appendChild(row);
    }
  });
};
$("chipPlan").onclick = () => {
  openSheet("Plan", body => fillPlanBody(body, S.selected && S.sessions.get(S.selected)?.plan));
};

export { openSheet, closeSheet, openDiffSheet, fillPlanBody, renderPlanSheet };
