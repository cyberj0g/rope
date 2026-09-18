import { $ } from "./helpers.js";
import { events } from "./state.js";

const search = { query: "", matches: [], pos: -1 };
function openSearch() {
  $("searchbar").classList.add("on");
  $("searchInput").focus();
  if (search.query) applySearch();
}
function closeSearch() {
  $("searchbar").classList.remove("on");
  $("searchInput").value = "";
  search.query = ""; search.pos = -1;
  $("searchCount").textContent = "0/0";
  clearSearchMarks();
  events.dispatchEvent(new Event("render"));
}
$("tbSearch").onclick = openSearch;
$("searchClose").onclick = closeSearch;
$("searchInput").oninput = e => { search.query = e.target.value.trim(); search.pos = -1; applySearch(); };
$("searchInput").onkeydown = e => {
  if (e.key === "Enter") { e.preventDefault(); moveSearch(e.shiftKey ? -1 : 1); }
  if (e.key === "Escape") { e.stopPropagation(); closeSearch(); }
};
$("searchNext").onclick = () => moveSearch(1);
$("searchPrev").onclick = () => moveSearch(-1);
function moveSearch(dir) {
  if (!search.matches.length) return;
  search.pos = (search.pos + dir + search.matches.length) % search.matches.length;
  search.matches.forEach(m => m.classList.remove("cur"));
  search.matches[search.pos].classList.add("cur");
  $("searchCount").textContent = `${search.pos + 1}/${search.matches.length}`;
  search.matches[search.pos].scrollIntoView({ block: "center", behavior: "smooth" });
}
// Unwrap all search marks and merge the text nodes they split, so the next
// search matches across the boundaries the previous one introduced.
function clearSearchMarks() {
  const inner = $("chatInner");
  inner.querySelectorAll("mark.srch").forEach(m => m.replaceWith(document.createTextNode(m.textContent)));
  inner.normalize();
}
function applySearch() {
  const inner = $("chatInner");
  clearSearchMarks();
  if (!search.query) { search.matches = []; search.pos = -1; $("searchCount").textContent = "0/0"; return; }
  const q = search.query.toLowerCase();
  const walker = document.createTreeWalker(inner, NodeFilter.SHOW_TEXT);
  const targets = [];
  while (walker.nextNode()) {
    const node = walker.currentNode;
    if (node.parentElement.closest("mark")) continue;
    if (node.nodeValue.toLowerCase().includes(q)) targets.push(node);
  }
  search.matches = [];
  search.pos = -1;
  for (const node of targets) {
    const frag = document.createDocumentFragment();
    let rest = node.nodeValue, idx;
    while ((idx = rest.toLowerCase().indexOf(q)) >= 0) {
      frag.appendChild(document.createTextNode(rest.slice(0, idx)));
      const mark = document.createElement("mark");
      mark.className = "srch";
      mark.textContent = rest.slice(idx, idx + q.length);
      frag.appendChild(mark);
      search.matches.push(mark);
      rest = rest.slice(idx + q.length);
    }
    frag.appendChild(document.createTextNode(rest));
    node.parentNode.replaceChild(frag, node);
  }
  if (search.matches.length) {
    search.pos = 0;
    $("searchCount").textContent = `${search.pos + 1}/${search.matches.length}`;
    search.matches[0].classList.add("cur");
    search.matches[0].scrollIntoView({ block: "center" });
  } else $("searchCount").textContent = "0/0";
}

export { search, openSearch, closeSearch, applySearch };
