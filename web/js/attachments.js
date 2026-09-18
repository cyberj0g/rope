import { $, esc, toast } from "./helpers.js";
import { S, draft } from "./state.js";
import { updateSendBtn } from "./status.js";

let camStream = null;

$("attachBtn").onclick = () => $("filePick").click();
$("filePick").onchange = e => { addFiles([...e.target.files]); e.target.value = ""; };
$("cameraBtn").onclick = () => openCamera();
$("camPick").onchange = e => { if (e.target.files[0]) addFile(e.target.files[0]); e.target.value = ""; };
// Plate IDs only need to be unique within this page's composer, and
// crypto.randomUUID is unavailable in non-secure contexts (plain-HTTP LAN
// access), so keep a plain counter instead.
let attSeq = 0;
function addFile(file) {
  const image = /^image\/(png|jpeg|gif|webp)$/.test(file.type);
  const limit = image ? 16 : 100;
  if (file.size > limit * 1024 * 1024) { toast(`File exceeds ${limit} MiB`, "err"); return; }
  if (draft.attachments.length >= 8) { toast("At most 8 attachments per message", "err"); return; }
  draft.attachments.push({ id: `att-${++attSeq}`, file, path: null, pathSession: null, status: "idle", preview: null });
  renderPlates();
}
function addFiles(files) { files.forEach(addFile); }
$("input").addEventListener("paste", e => {
  if (e.clipboardData?.files.length) {
    e.preventDefault();
    addFiles([...e.clipboardData.files]);
  }
});
$("composer").addEventListener("dragover", e => { e.preventDefault(); });
$("composer").addEventListener("drop", e => {
  e.preventDefault();
  addFiles([...e.dataTransfer.files]);
});
function clearAttachments() {
  for (const a of draft.attachments) { const u = a.preview; if (u) URL.revokeObjectURL(u); }
  draft.attachments = [];
  renderPlates();
}
// Plate states: idle (file selected, not uploaded), uploading (transfer in
// flight), done (uploaded path cached for one session), error (upload failed).
// The spinner reflects the transfer only — a selected or already-uploaded
// file never shows one.
function renderPlates() {
  const row = $("plates");
  row.innerHTML = "";
  for (const a of draft.attachments) {
    const plate = document.createElement("div");
    plate.className = "plate";
    const isImage = /^image\/(png|jpeg|gif|webp)$/.test(a.file.type);
    if (isImage && !a.preview) a.preview = URL.createObjectURL(a.file);
    plate.title = a.file.name;
    plate.innerHTML = `${isImage ? `<img src="${a.preview}" alt="">` : `<span class="filename">${esc(a.file.name)}</span>`}${a.status === "uploading" ? '<div class="spin"></div>' : ""}${a.status === "error" ? `<div class="err">failed — retry on send</div>` : ""}<button class="rm" aria-label="Remove"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18"/></svg></button>`;
    plate.querySelector(".rm").onclick = () => {
      draft.attachments = draft.attachments.filter(x => x.id !== a.id);
      renderPlates();
    };
    row.appendChild(plate);
  }
  updateSendBtn();
}
async function uploadAttachment(a, session) {
  a.status = "uploading";
  renderPlates();
  try {
    const res = await fetch(`/api/sessions/${encodeURIComponent(session)}/attachments?filename=${encodeURIComponent(a.file.name)}`, {
      method: "POST", headers: { Authorization: `Bearer ${S.token}` }, body: a.file,
    });
    if (!res.ok) throw new Error(await res.text());
    const out = await res.json();
    // A failed upload cleared the cached path; remember which session this
    // path belongs to so switching sessions forces a fresh upload.
    a.path = out.path;
    a.pathSession = session;
    a.status = "done";
    renderPlates();
    return out;
  } catch (e) {
    a.status = "error";
    renderPlates();
    throw e;
  }
}

async function openCamera() {
  if (navigator.mediaDevices?.getUserMedia) {
    try {
      camStream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: "environment" }, audio: false });
      $("camVideo").srcObject = camStream;
      $("camera").classList.add("on");
      return;
    } catch {}
  }
  $("camPick").click();
}
function closeCamera() {
  camStream?.getTracks().forEach(t => t.stop());
  camStream = null;
  $("camera").classList.remove("on");
}
$("camCancel").onclick = closeCamera;
$("camSnap").onclick = () => {
  const v = $("camVideo");
  const canvas = document.createElement("canvas");
  canvas.width = v.videoWidth; canvas.height = v.videoHeight;
  canvas.getContext("2d").drawImage(v, 0, 0);
  canvas.toBlob(blob => {
    closeCamera();
    if (blob) addFile(new File([blob], "camera.jpg", { type: "image/jpeg" }));
  }, "image/jpeg", .92);
};

export { clearAttachments, renderPlates, uploadAttachment, closeCamera };
