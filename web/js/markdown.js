import { esc } from "./helpers.js";
import MarkdownIt from "./vendor/markdown-it.mjs";
import DOMPurify from "./vendor/purify.mjs";

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
// Parse Markdown into tokens; never run substitutions over generated HTML.
// Sanitize last, after every renderer/plugin, including our trusted highlighter.
const parser = new MarkdownIt({ html: false, breaks: true, linkify: true });
parser.validateLink = url => /^https?:\/\//i.test(url);
parser.linkify.set({ fuzzyLink: false, fuzzyEmail: false });
parser.renderer.rules.link_open = (tokens, idx, options, env, renderer) => {
  tokens[idx].attrSet("target", "_blank");
  tokens[idx].attrSet("rel", "noopener noreferrer");
  return renderer.renderToken(tokens, idx, options);
};
// Chat attachments have a separate authenticated renderer. Markdown must not
// load remote images (tracking or requests to services on the user's network).
parser.renderer.rules.image = (tokens, idx, options, env, renderer) =>
  esc(renderer.renderInlineAsText(tokens[idx].children || [], options, env));
parser.renderer.rules.fence = (tokens, idx) => {
  const token = tokens[idx];
  const lang = token.info.trim().split(/\s+/)[0];
  return `<pre data-lang="${esc(lang)}"><code>${highlight(token.content.replace(/\n$/, ""), lang)}</code></pre>\n`;
};
// Keep the existing decorative task markers without permitting arbitrary HTML.
parser.core.ruler.after("inline", "task_markers", state => {
  for (let i = 2; i < state.tokens.length; i++) {
    const token = state.tokens[i];
    if (token.type !== "inline" || state.tokens[i - 1].type !== "paragraph_open"
        || state.tokens[i - 2].type !== "list_item_open") continue;
    const first = token.children?.[0];
    const match = first?.type === "text" && first.content.match(/^\[([ xX])\]\s+/);
    if (!match) continue;
    first.content = first.content.slice(match[0].length);
    const marker = new state.Token("task_marker", "span", 0);
    marker.meta = { done: match[1] !== " " };
    token.children.unshift(marker);
  }
});
parser.renderer.rules.task_marker = (tokens, idx) =>
  `<span class="task${tokens[idx].meta.done ? " done" : ""}"></span>`;

const SANITIZE = {
  ALLOWED_TAGS: ["p", "br", "strong", "em", "del", "s", "a", "code", "pre",
    "h1", "h2", "h3", "h4", "h5", "h6", "hr", "blockquote", "ul", "ol", "li",
    "table", "thead", "tbody", "tr", "th", "td", "span"],
  ALLOWED_ATTR: ["href", "title", "target", "rel", "class", "data-lang", "start"],
  ALLOW_DATA_ATTR: false,
  ALLOW_ARIA_ATTR: false,
};
function inline(text) {
  return DOMPurify.sanitize(parser.renderInline(text || ""), SANITIZE);
}
function md(text) {
  return DOMPurify.sanitize(parser.render(text || ""), SANITIZE);
}
function plain(text) {
  return `<pre class="plain">${esc(text)}</pre>`;
}

export { highlight, inline, md, plain };
