const LS = {
  get(k, d) { try { const v = localStorage.getItem(k); return v === null ? d : JSON.parse(v); } catch { return d; } },
  set(k, v) { try { localStorage.setItem(k, JSON.stringify(v)); } catch {} },
};
const prefs = Object.assign({ showThinking: true, showTools: true, sound: true }, LS.get("rope.prefs", {}));
const savePrefs = () => LS.set("rope.prefs", prefs);

const S = {
  token: LS.get("rope.token", ""),
  // Transport identity and request counter live only for this page's lifetime:
  // each page load negotiates a fresh client identity with the server, so a
  // reload or a second tab can never replay (or collide with) another page's
  // mutation request IDs.
  clientId: null,
  serverId: null,
  requestId: 0,
  socket: null,
  models: [],
  projectRoot: "",
  project: null,
  catalog: [],
  catalogTotal: 0,
  catalogQuery: "",
  confirmDelete: null,
  sessions: new Map(),
  selected: null,
  pending: new Map(),
  chunks: new Map(),
  reconnect: null,
  lastPhase: "idle",
};

const draft = { attachments: [] };
const events = new EventTarget();

export { LS, prefs, savePrefs, S, draft, events };
