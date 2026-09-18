import { $, esc, toast } from "./helpers.js";
import { S } from "./state.js";

async function imageUrl(session, path) {
  const key = session + "\u0001" + path;
  if (imageCache.has(key)) return imageCache.get(key);
  try {
    const res = await fetch(`/api/sessions/${encodeURIComponent(session)}/attachments/${path}`, { headers: { Authorization: `Bearer ${S.token}` } });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const url = URL.createObjectURL(await res.blob());
    imageCache.set(key, url);
    return url;
  } catch (e) {
    console.error("attachment fetch failed", path, e);
    return null;
  }
}
const imageCache = new Map();
function renderImages(container, session, images, prepend = true) {
  if (!images || !images.length) return;
  const grid = document.createElement("div");
  grid.className = "imgrid";
  if (prepend) container.prepend(grid); else container.appendChild(grid);
  for (const img of images) {
    const el = document.createElement("img");
    el.alt = "attachment";
    el.src = "data:image/gif;base64,R0lGODlhAQABAAAAACw=";
    grid.appendChild(el);
    imageUrl(session, img.path).then(url => {
      if (!url || !el.isConnected) return;
      el.src = url;
      el.onclick = () => { $("viewerImg").src = url; $("viewer").classList.add("on"); };
    });
  }
}
const fileCache = new Map();
async function fileUrl(endpoint) {
  const key = endpoint;
  if (fileCache.has(key)) return fileCache.get(key);
  try {
    const res = await fetch(endpoint, { headers: { Authorization: `Bearer ${S.token}` } });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const url = URL.createObjectURL(await res.blob());
    fileCache.set(key, url);
    return url;
  } catch (e) {
    console.error("file fetch failed", endpoint, e);
    return null;
  }
}
function fmtSize(n) {
  if (n < 1024) return n + " B";
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < units.length - 1);
  return n.toFixed(n < 10 ? 1 : 0) + " " + units[i];
}
async function downloadFile(endpoint, file) {
  try {
    const res = await fetch(endpoint, { headers: { Authorization: `Bearer ${S.token}` } });
    if (!res.ok) throw new Error("HTTP " + res.status);
    const url = URL.createObjectURL(await res.blob());
    const a = document.createElement("a");
    a.href = url;
    a.download = file.name;
    document.body.appendChild(a);
    a.click();
    a.remove();
    setTimeout(() => URL.revokeObjectURL(url), 30000);
  } catch (e) {
    console.error("file download failed", file.path, e);
    toast("download failed: " + file.name, "err");
  }
}
// A file a tool sent to the chat: images render inline like attachments,
// everything else is a tile that downloads the file on click.
function renderFile(container, session, block) {
  const file = block.file;
  if (!file) return;
  const endpoint = `/api/sessions/${encodeURIComponent(session)}/files/${encodeURIComponent(block.id)}`;
  if (file.mime_type.startsWith("image/")) {
    const grid = document.createElement("div");
    grid.className = "imgrid";
    container.appendChild(grid);
    const el = document.createElement("img");
    el.alt = file.name;
    el.src = "data:image/gif;base64,R0lGODlhAQABAAAAACw=";
    grid.appendChild(el);
    fileUrl(endpoint).then(url => {
      if (!url || !el.isConnected) return;
      el.src = url;
      el.onclick = () => { $("viewerImg").src = url; $("viewer").classList.add("on"); };
    });
    return;
  }
  const tile = document.createElement("div");
  tile.className = "filetile";
  tile.innerHTML = `<svg class="ficon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/></svg><div class="fmeta"><div class="fname"></div><div class="fsize"></div></div><svg class="fdown" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12"/><path d="m7 10 5 5 5-5"/><path d="M5 21h14"/></svg>`;
  tile.querySelector(".fname").textContent = file.name;
  tile.querySelector(".fsize").textContent = fmtSize(file.size);
  tile.onclick = () => downloadFile(endpoint, file);
  container.appendChild(tile);
}

$("viewerX").onclick = () => $("viewer").classList.remove("on");
$("viewer").onclick = e => { if (e.target === $("viewer")) $("viewer").classList.remove("on"); };

export { renderImages, renderFile, imageCache };
