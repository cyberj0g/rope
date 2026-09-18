import { request } from "./protocol.js";

export async function openRaw(session, blockId) {
  const previous = document.activeElement;
  const dialog = document.createElement("dialog");
  dialog.className = "raw-dialog";
  dialog.setAttribute("aria-labelledby", "rawTitle");
  dialog.innerHTML = `<header><h2 id="rawTitle">Raw model request</h2><button type="button" aria-label="Close raw request">✕</button></header>
    <div class="raw-toolbar"><input type="search" aria-label="Search request" placeholder="Search keys and values…" autocomplete="off"><button type="button" data-expand>Expand all</button><button type="button" data-collapse>Collapse all</button><button type="button" data-copy>Copy JSON</button></div>
    <p class="raw-note">Recorded provider JSON · binary values are shortened · file contents are not fetched</p>
    <div class="raw-tree" aria-live="polite">Loading request…</div>`;
  document.body.append(dialog);
  const close = () => dialog.close();
  dialog.querySelector("header button").onclick = close;
  dialog.addEventListener("click", e => { if (e.target === dialog) close(); });
  dialog.addEventListener("close", () => { dialog.remove(); previous?.focus(); });
  dialog.addEventListener("keydown", e => {
    e.stopPropagation();
    if (e.key === "Escape") { e.preventDefault(); close(); }
  });
  dialog.showModal();
  const tree = dialog.querySelector(".raw-tree");
  try {
    const { body } = await request({ type: "raw_request", session_id: session, block_id: blockId });
    if (!dialog.open) return;
    const nodes = [];
    function node(key, value, depth, ancestors) {
      const branch = value !== null && typeof value === "object";
      const row = document.createElement(branch ? "details" : "div");
      row.className = branch ? "raw-branch" : "raw-leaf";
      const label = branch ? document.createElement("summary") : row;
      const prefix = key === null ? "" : JSON.stringify(key) + ": ";
      const text = branch ? prefix + (Array.isArray(value) ? `[${value.length} items]` : `{${Object.keys(value).length} keys}`) : prefix + JSON.stringify(value);
      label.textContent = text;
      nodes.push({ row, label, text: text.toLowerCase(), ancestors, branch });
      if (branch) {
        row.open = depth < 2;
        row.append(label);
        const children = document.createElement("div");
        children.className = "raw-children";
        for (const [k, v] of Object.entries(value)) children.append(node(k, v, depth + 1, [...ancestors, row]));
        row.append(children);
      }
      return row;
    }
    tree.replaceChildren(node(null, body, 0, []));
    dialog.querySelector("[data-copy]").onclick = async e => {
      try { await navigator.clipboard.writeText(JSON.stringify(body, null, 2)); e.target.textContent = "Copied"; }
      catch { e.target.textContent = "Copy unavailable"; }
    };
    const note = dialog.querySelector(".raw-note");
    const originalNote = note.textContent;
    let savedOpen = null;
    const search = dialog.querySelector("input");
    search.oninput = () => {
      const query = search.value.toLowerCase();
      if (query && !savedOpen) savedOpen = nodes.filter(n => n.branch && n.row.open).map(n => n.row);
      for (const n of nodes) {
        const match = !!query && n.text.includes(query);
        n.label.classList.toggle("raw-match", match);
        n.row.hidden = !!query && !match;
      }
      if (query) {
        for (const n of nodes) if (n.text.includes(query)) for (const parent of n.ancestors) { parent.hidden = false; parent.open = true; }
      } else if (savedOpen) {
        for (const n of nodes) if (n.branch) n.row.open = savedOpen.includes(n.row);
        savedOpen = null;
      }
      const count = nodes.filter(n => query && n.text.includes(query)).length;
      note.textContent = query ? `${count} matching keys or values` : originalNote;
      tree.scrollTop = 0;
    };
    dialog.querySelector("[data-expand]").onclick = () => nodes.forEach(n => { if (n.branch) n.row.open = true; });
    dialog.querySelector("[data-collapse]").onclick = () => nodes.forEach(n => { if (n.branch) n.row.open = false; });
  } catch (error) {
    if (dialog.open) tree.textContent = error.message;
  }
}
