# Client/server protocol v1

Rope runs one core per process and one project directory per core. The TUI uses
`Core` directly through its local client adapter. `server.rs` adapts the same
commands and subscriptions to HTTP/WebSockets. Each loaded session owns a runtime
task and mutable tools; the core retains loaded sessions until shutdown.

## Connection

Connect to `/ws` and send this JSON text frame within ten seconds:

```json
{"protocol":1,"token":"YOUR_SERVER_TOKEN"}
```

The `hello` response includes `protocol`, `server_id`, `client_id`, `models`, and
`project_root`. Provider credentials and opaque provider replay items are never
part of the public view. The server then sends a complete `catalog` and `project`
snapshot and pushes subsequent changes automatically. Each topic has its own
sequence number; catalog and project messages replace their previous snapshots.

All clients share control. Use a token from `ROPE_SERVER_TOKEN` or the private
token file, generated on first network startup. Browser Origins must exactly
match the default loopback origins or a repeated `--allow-origin` argument.
Non-browser clients may omit Origin but still need authentication. For remote
access terminate HTTPS/WSS at a reverse proxy. Tokens belong in the first frame,
never in the URL. There is no remote process-shutdown command.

## Requests and replies

Every request has a positive, increasing integer `request_id`, encoded as a
string. Successful replies contain `result`; failures contain `error.code` and
`error.message`:

```json
{"request_id":"1","type":"create_session","name":null}
{"type":"reply","request_id":"1","result":{"session_id":"session-1789600000"}}

{"request_id":"2","type":"subscribe","session_id":"session-1789600000"}
{"request_id":"3","type":"command","session_id":"session-1789600000","action":{"type":"send_message","content":"Read README.md","attachments":[]}}
{"type":"reply","request_id":"3","result":{"turn_id":"opaque-turn-id","settings_revision":0}}
```

The server sends a `snapshot` before the subscribe reply and subsequent `event`
messages. Subscribe can open an existing unloaded session. It cannot create one.
Use `unsubscribe` with `session_id` to detach. A connection may subscribe to
several sessions at once.

`command.action` supports:

| Type | Fields | Behavior |
| --- | --- | --- |
| `send_message` | `content`, optional `attachments` | Starts a turn or queues a steer in server acceptance order |
| `cancel` | `turn_id` | Cancels only the identified active operation |
| `approve` | `turn_id`, `approval_id`, `decision` | First valid answer wins; decisions are `allow_once`, `allow_session`, `deny` |
| `set_model` | `model`, `revision` | Uses a model name from hello; requires idle state and current settings revision |
| `set_reasoning` | `effort`, `revision` | Explicit supported effort or null; requires idle state and current revision |
| `compact` | none | Starts cancellable manual compaction while idle |

Messages during manual compaction return `busy`; clients should retain drafts
until acceptance. Stale actions return `stale_turn`, `stale_approval`, or
`stale_settings` only to the requester. Errors from running model/tool work are
published to everyone viewing the session.

Use `{"request_id":"4","type":"git_diff","path":"src/main.rs"}` to query a
project-relative file, or null for the entire working diff. The result contains
`path` and `content`; it does not change any other client's selected diff.

## Session synchronization

`{"type":"snapshot","snapshot":{...}}` carries `session_id`, `seq`, `blocks`,
`state`, `plan`, and `project`. State includes the active operation ID and phase,
pending approval, settings and revision, usage/context, and queued steer count.
Blocks have stable IDs within that loaded session, visible content, attachment
references, tool state, and timers (`elapsed_ms`, `running`). The internal TUI
also uses `assistant_id`, `reasoning_id`, and `drafts` to restore its renderer.

`{"type":"event","update":{"session_id":"...","seq":42,"changes":[...]}}`
applies these changes in order:

| Change | Effect |
| --- | --- |
| `insert` | Insert `block` before ID `before`, or append if null |
| `replace` | Replace the block with the matching `block.id` |
| `append` | Append `text` to `block_id`'s `content`, or its tool's `arguments`/`output` field |
| `state` | Replace session state |
| `plan` | Replace the plan |
| `project` | Replace project state |

Install snapshots atomically, ignore events at or below their sequence, and
require each next session event to have sequence `seq + 1`. Empty change lists
still advance the sequence. On a gap or `resync_required`, subscribe again for a
new snapshot. Capture and subscription are atomic inside the core, so there is
no gap between a snapshot and the following events.

Large outgoing messages use UTF-8-safe chunks:

```json
{"type":"chunk","id":"opaque-message-id","index":0,"data":"first part of serialized JSON"}
{"type":"chunk_end","id":"opaque-message-id","count":1}
```

Accumulate chunks by ID, verify consecutive indexes and the final count, then
parse the concatenated data as one complete message. Different IDs can interleave.
Do not apply partial snapshots. The included browser example implements this
flow and batches display updates with `requestAnimationFrame`.

## Reconnects and uncertain requests

Send the previous `client_id` and `server_id` in hello to resume the request
history. Within that identity, repeating a mutation's ID and payload returns its
cached reply without repeating the action. A changed payload returns
`request_id_reused`; an evicted older reply returns `expired_request`. Requests
are serialized per identity, including across simultaneous connections.

The cache retains 128 mutation replies per client and identities for 24 hours
since their last request. Restarted servers and expired identities return
`expired_client`. Establish a fresh identity and inspect snapshots to reconcile
uncertain actions; never automatically resend mutations after losing their
identity. Acceptance means acceptance by the live core, not crash durability.
Disconnecting any number of clients does not cancel sessions or resolve approvals.

## Attachments

Upload PNG/JPEG/GIF/WebP bytes using `POST /api/sessions/{session_id}/attachments`
with `Authorization: Bearer TOKEN`. The JSON response contains `path`,
`mime_type`, `width`, and `height`. Pass the returned `path` unchanged in
`send_message.attachments`. Images in transcript blocks use the same references.

Download with `GET /api/sessions/{session_id}/attachments/{path}`, again with
Bearer authentication. The returned `path` currently includes `attachments/`;
treat it as an opaque identifier, not a local filename. The API cannot read
arbitrary filesystem paths. Tool-produced images are stored before publication.

## Limits and persistence

- 32 WebSocket connections and 16 session subscriptions per connection.
- 256 KiB incoming commands, at most eight images per message.
- 16 MiB and 64 megapixels per uploaded image.
- 256 buffered session events and 32 outgoing frames per connection; lag forces
  snapshot recovery and stalled writes close the connection.
- Outgoing JSON uses chunks of at most 32 KiB before JSON string escaping.

Existing session JSONL remains readable. New optional metadata binds sessions
to the canonical project root and stores session settings. Unbound legacy
sessions appear in the catalog and bind on explicit open; foreign-project
sessions are rejected. An OS-backed lock prevents two processes writing one
loaded session. Metadata is replaced atomically.

SIGINT/SIGTERM shuts down the process, interrupts active work, saves its recovery
state, and stops session tools. A process crash can lose uncommitted live output.
Sessions run concurrently but share project files; this version adds no worktree
isolation. Full web UI parity, remote TUI attachment, multiple projects per core,
user roles, and durable event replay are outside v1.
