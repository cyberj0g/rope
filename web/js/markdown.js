import { esc } from "./helpers.js";

const KEYWORDS = new Set(("let const var fn function return if else match while for loop break continue impl pub struct enum trait type use mod " +
  "def class import from as with try except raise new delete void int float bool string true false null this self None True False " +
  "export default async await switch case do in of not and or is None def echo set export local then elif fi end done elif").split(" "));
function highlight(code, lang) {
  const hashComment = /^(?:sh|bash|zsh|py|python|rb|toml|ini|make)$/.test(lang);
  const pattern = /(\/\/[^\n]*|#[^!\n][^\n]*|\/\*[\s\S]*?\*\/|"(?:[^"\\\n]|\\.)*"|'(?:[^'\\\n]|\\.)*'|`(?:[^`\\]|\\.)*`|\b\d+(?:\.\d+)?\b|\b[A-Za-z_][A-Za-z0-9_]*(?=\s*\()|\b[A-Za-z_][A-Za-z0-9_]*\b)/g;
  // Every slice — matched tokens and the text between them — is escaped, so
  // fenced code can never introduce live HTML into the page.
  let out = "", last = 0, m;
  pattern.lastIndex = 0;
  while ((m = pattern.exec(code))) {
    out += esc(code.slice(last, m.index));
    const tok = m[0];
    let cls = "";
    if (tok[0] === '"' || tok[0] === "'" || tok[0] === "`") cls = "s";
    else if (tok[0] === "/" || (tok[0] === "#" && hashComment)) cls = "c";
    else if (/^\d/.test(tok)) cls = "n";
    else if (KEYWORDS.has(tok)) cls = "k";
    else if (tok[0] === "_" || /[a-z]/.test(tok[0])) cls = "f";
    out += cls ? `<span class="${cls}">${esc(tok)}</span>` : esc(tok);
    last = m.index + tok.length;
  }
  return out + esc(code.slice(last));
}
function inline(s) {
  // Code spans are carved out before any emphasis or link parsing so their
  // text stays literal; only the surrounding prose is further transformed.
  const emph = t => {
    t = esc(t);
    t = t.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
    t = t.replace(/(^|[\s(])\*([^*\n]+)\*(?=[\s).,!?:;]|$)/g, "$1<em>$2</em>");
    t = t.replace(/~~([^~]+)~~/g, "<del>$1</del>");
    t = t.replace(/\[([^\]]+)\]\((https?:[^)\s]+)\)/g,
      (_, a, u) => `<a href="${u}" target="_blank" rel="noopener">${a}</a>`);
    t = t.replace(/(^|[\s>])(https?:\/\/[^\s<]+)/g,
      (_, p, u) => `${p}<a href="${u}" target="_blank" rel="noopener">${u.replace(/^[a-z]+:\/\//i, "").slice(0, 48)}${u.length > 60 ? "…" : ""}</a>`);
    return t;
  };
  return s.split(/(`[^`\n]+`)/g).map(part =>
    /^`[^`\n]+`$/.test(part) ? `<code>${esc(part.slice(1, -1))}</code>` : emph(part)
  ).join("");
}
function md(text) {
  if (!text) return "";
  const fences = [];
  const body = text.replace(/```([\w+-]*)\n?([\s\S]*?)(?:```|$)/g, (m, lang, code) => {
    fences.push(`<pre data-lang="${esc(lang)}"><code>${highlight(code.replace(/\n$/, ""), lang)}</code></pre>`);
    return `\u0000F${fences.length - 1}\u0000`;
  });
  const lines = body.split("\n");
  let html = "", para = [], list = null;
  const flushP = () => {
    if (para.length) { html += `<p>${para.map(inline).join("<br>")}</p>`; para = []; }
  };
  const flushL = () => {
    if (list) {
      html += `<${list.tag}>${list.items.map(i =>
        `<li>${i.task ? `<span class="task${i.done ? " done" : ""}"></span>` : ""}${inline(i.text)}</li>`
      ).join("")}</${list.tag}>`;
      list = null;
    }
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const t = line.trim();
    if (/^\u0000F\d+\u0000$/.test(t)) { flushP(); flushL(); html += t; continue; }
    if (!t) { flushP(); flushL(); continue; }
    let m;
    if ((m = t.match(/^(#{1,4})\s+(.*)/))) { flushP(); flushL(); html += `<h${m[1].length}>${inline(m[2])}</h${m[1].length}>`; continue; }
    if (/^(-{3,}|\*{3,})$/.test(t)) { flushP(); flushL(); html += "<hr>"; continue; }
    if (/^>\s?/.test(t)) {
      flushP(); flushL();
      const q = [t.replace(/^>\s?/, "")];
      while (lines[i + 1] && /^>\s?/.test(lines[i + 1].trim())) { i++; q.push(lines[i].trim().replace(/^>\s?/, "")); }
      html += `<blockquote>${q.map(inline).join("<br>")}</blockquote>`; continue;
    }
    if (t.includes("|") && i + 1 < lines.length && /^\s*\|?[\s:|-]+\|[\s:|-]*$/.test(lines[i + 1])) {
      flushP(); flushL();
      const cells = r => r.trim().replace(/^\||\|$/g, "").split("|").map(c => inline(c.trim()));
      const head = cells(t); i += 2;
      const rows = [];
      while (i < lines.length && lines[i].includes("|") && lines[i].trim()) { rows.push(cells(lines[i])); i++; }
      i--;
      html += `<table><thead><tr>${head.map(h => `<th>${h}</th>`).join("")}</tr></thead><tbody>${rows.map(r => `<tr>${r.map(c => `<td>${c}</td>`).join("")}</tr>`).join("")}</tbody></table>`;
      continue;
    }
    if ((m = t.match(/^[-*]\s+\[( |x|X)\]\s+(.*)/))) {
      flushP();
      if (!list || list.tag !== "ul") { flushL(); list = { tag: "ul", items: [] }; }
      list.items.push({ text: m[2], task: true, done: m[1].toLowerCase() === "x" }); continue;
    }
    if ((m = t.match(/^[-*]\s+(.*)/))) {
      flushP();
      if (!list || list.tag !== "ul") { flushL(); list = { tag: "ul", items: [] }; }
      list.items.push({ text: m[1] }); continue;
    }
    if ((m = t.match(/^\d+[.)]\s+(.*)/))) {
      flushP();
      if (!list || list.tag !== "ol") { flushL(); list = { tag: "ol", items: [] }; }
      list.items.push({ text: m[1] }); continue;
    }
    flushL(); para.push(t);
  }
  flushP(); flushL();
  return html.replace(/\u0000F(\d+)\u0000/g, (_, i) => fences[+i]);
}
function plain(text) {
  return `<pre class="plain">${esc(text)}</pre>`;
}

export { highlight, inline, md, plain };
