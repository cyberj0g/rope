import { LS, S, events } from "./state.js";
import { toast } from "./helpers.js";

function connect() {
  clearTimeout(S.reconnect);
  if (S.socket) { S.socket.onclose = null; S.socket.onerror = null; S.socket.close(); }
  S.chunks.clear();
  const wsUrl = `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/ws`;
  const socket = new WebSocket(wsUrl);
  S.socket = socket;
  events.dispatchEvent(new CustomEvent("phase", { detail: ["connecting", "connecting…"] }));
  socket.onopen = () => {
    const hello = { protocol: 2, token: S.token };
    if (S.clientId && S.serverId) { hello.client_id = S.clientId; hello.server_id = S.serverId; }
    socket.send(JSON.stringify(hello));
  };
  socket.onmessage = e => { try { receive(JSON.parse(e.data)); } catch (err) { console.error(err); } };
  socket.onerror = () => {};
  socket.onclose = () => {
    const uncertain = S.pending.size > 0;
    for (const done of S.pending.values()) done(null, { code: "disconnected", message: "Disconnected. Close and reopen to retry." });
    S.pending.clear();
    events.dispatchEvent(new CustomEvent("phase", { detail: ["idle", uncertain ? "disconnected — an action may have been accepted" : "disconnected; reconnecting…"] }));
    S.reconnect = setTimeout(connect, 1500);
  };
}
function request(value, done = () => {}) {
  if (S.socket?.readyState !== WebSocket.OPEN) { toast("Not connected", "err"); return Promise.reject(new Error("offline")); }
  const id = String(++S.requestId);
  return new Promise((res, rej) => {
    S.pending.set(id, (r, e) => e ? rej(Object.assign(new Error(e.message), { code: e.code })) : res(r));
    S.socket.send(JSON.stringify({ request_id: id, ...value }));
  });
}
const commandTo = (id, action, done) => id && request({ type: "command", session_id: id, action }, done);
const command = (action, done) => commandTo(S.selected, action, done);

// The sidebar filter runs on the server against every session; the client
// only tracks how many matching rows it has loaded so far.
export const CATALOG_PAGE = 20;
let catalogSeq = 0;
function catalogView(query, offset) {
  const seq = ++catalogSeq;
  return request({ type: "catalog_view", query: query || null, offset })
    .then(res => {
      if (seq !== catalogSeq) return null; // a newer filter replaced this one
      S.catalog = res.sessions;
      S.catalogTotal = res.total;
      events.dispatchEvent(new Event("catalog"));
      return res;
    });
}
const deleteSession = id => request({ type: "delete_session", session_id: id });
const revealBlock = (session, blockId) =>
  request({ type: "reveal_block", session_id: session, block_id: blockId });

function select(id) {
  if (S.selected === id) return;
  if (S.selected) request({ type: "unsubscribe", session_id: S.selected }, () => {});
  S.selected = id;
  S.confirmDelete = null;
  LS.set("rope.session", id);
  events.dispatchEvent(new Event("select"));
  if (id) request({ type: "subscribe", session_id: id }, () => {});
  events.dispatchEvent(new Event("render"));
}
function receive(message) {
  if (message.type === "chunk") {
    const parts = S.chunks.get(message.id) || [];
    if (message.index !== parts.length) { S.socket.onclose = null; S.socket.close(); return; }
    parts.push(message.data); S.chunks.set(message.id, parts); return;
  }
  if (message.type === "chunk_end") {
    const parts = S.chunks.get(message.id); S.chunks.delete(message.id);
    if (!parts || parts.length !== message.count) { S.socket.onclose = null; S.socket.close(); return; }
    receive(JSON.parse(parts.join(""))); return;
  }
  switch (message.type) {
    case "hello":
      S.clientId = message.client_id; S.serverId = message.server_id;
      S.models = message.models; S.projectRoot = message.project_root;
      events.dispatchEvent(new Event("render"));
      if (S.selected) request({ type: "subscribe", session_id: S.selected }, () => {});
      break;
    case "catalog":
      S.catalog = message.catalog.sessions;
      S.catalogTotal = message.catalog.total ?? message.catalog.sessions.length;
      events.dispatchEvent(new Event("catalog")); break;
    case "project":
      S.project = message.update;
      events.dispatchEvent(new Event("project")); break;
    case "snapshot": {
      const snap = message.snapshot;
      S.sessions.set(snap.session_id, snap);
      if (snap.session_id === S.selected) events.dispatchEvent(new Event("render"));
      break;
    }
    case "event": {
      const update = message.update;
      const snap = S.sessions.get(update.session_id);
      if (!snap || update.seq <= snap.seq) break;
      if (update.seq !== snap.seq + 1) {
        request({ type: "subscribe", session_id: update.session_id }, () => {});
        break;
      }
      for (const change of update.changes) {
        if (change.type === "insert") {
          const index = change.before ? snap.blocks.findIndex(b => b.id === change.before) : -1;
          snap.blocks.splice(index < 0 ? snap.blocks.length : index, 0, change.block);
        } else if (change.type === "replace") {
          const index = snap.blocks.findIndex(b => b.id === change.block.id);
          if (index >= 0) snap.blocks[index] = change.block; else snap.blocks.push(change.block);
        } else if (change.type === "append") {
          const block = snap.blocks.find(b => b.id === change.block_id);
          if (!block) continue;
          if (change.field === "content") block.content += change.text;
          else if (block.tool) block.tool[change.field] = (block.tool[change.field] || "") + change.text;
        } else if (change.type === "state") snap.state = change.state;
        else if (change.type === "plan") snap.plan = change.plan;
        else if (change.type === "project") snap.project = change.project;
      }
      snap.seq = update.seq;
      if (update.session_id === S.selected) events.dispatchEvent(new Event("render"));
      break;
    }
    case "resync_required":
      if (message.session_id === S.selected) request({ type: "subscribe", session_id: message.session_id }, () => {});
      break;
    case "reply": {
      const done = S.pending.get(message.request_id);
      S.pending.delete(message.request_id);
      if (done) done(message.result, message.error);
      break;
    }
    case "error":
      if (message.code === "unauthorized") {
        clearTimeout(S.reconnect);
        S.socket.onclose = null; S.socket.close();
        S.token = ""; LS.set("rope.token", "");
        events.dispatchEvent(new CustomEvent("auth", { detail: "Invalid token" }));
      } else if (message.code === "expired_client") {
        // The server no longer knows this page's identity (e.g. a server
        // restart). Reconnect with a fresh identity; never resend mutations.
        S.clientId = null; S.serverId = null;
        toast(message.message, "err");
      } else toast(`${message.code}: ${message.message}`, "err");
      break;
  }
}

export { connect, request, commandTo, command, select, catalogView, deleteSession, revealBlock };
