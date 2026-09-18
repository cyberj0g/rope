const $ = id => document.getElementById(id);
const esc = s => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");
const fmt = n => n >= 10000 ? (n / 1000).toFixed(1) + "k" : String(n);
// Catalog timestamps are epoch milliseconds.
function relTime(unixMs) {
  const d = Math.max(0, Math.floor((Date.now() - unixMs) / 1000));
  if (d < 60) return "just now";
  if (d < 3600) return Math.floor(d / 60) + "m ago";
  if (d < 86400) return Math.floor(d / 3600) + "h ago";
  return Math.floor(d / 86400) + "d ago";
}
function toast(text, kind = "") {
  const el = document.createElement("div");
  el.className = "toast " + kind;
  el.textContent = text;
  $("toasts").appendChild(el);
  setTimeout(() => el.remove(), 4000);
}

function prettyArgs(a) {
  try { return JSON.stringify(JSON.parse(a), null, 2); } catch { return a || ""; }
}

function fmtDur(ms) {
  const s = ms / 1000;
  if (s < 60) return s < 10 ? s.toFixed(1) + "s" : (s | 0) + "s";
  if (s < 3600) return `${(s / 60) | 0}m ${s % 60 | 0}s`;
  return `${(s / 3600) | 0}h ${(s % 3600) / 60 | 0}m`;
}

export { $, esc, fmt, relTime, toast, prettyArgs, fmtDur };
