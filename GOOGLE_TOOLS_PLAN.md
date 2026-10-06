# Google Calendar and Gmail implementation plan

Plan only; no integration is implemented by this document. Based on `e07bdd3`,
2026-09-21. Use built-in Rust tools with direct Google REST calls, sharing one
Google connection. Reuse Rope's tool registry, policies, runtime, and approval
transport. No MCP server or browser automation is needed for these tools.

## Existing behavior checked

- `src/config.rs` merges global and project TOML and defines `allow`, `ask`, and
  `deny` tool policies. It has no Google configuration or credential store.
- `src/onboarding.rs` configures model providers at first launch. There is no
  later integration setup flow.
- `src/tool/mod.rs` registers built-ins and supports replacing a tool catalog by
  origin. `src/core/mod.rs` currently discovers tools separately for each session.
- `src/runtime/mod.rs` requests approval before running a tool, carrying the
  complete arguments. `src/runtime/actor.rs` resolves approvals once and saves
  session-wide tool grants; those grants survive restarting Rope.
- `web/js/status.js:192` displays the tool name and complete pretty-printed
  arguments in a scrollable approval card. `tests/web/e2e.mjs:333` checks this.
- `src/ui/mod.rs:1892` displays approval keys in the composer, but the arguments
  appear only in an expanded tool block (`src/ui/mod.rs:3022`). Tool blocks start
  collapsed, and an approval request does not expand them.
- `cargo test --locked approval` passed all four matching tests: shared approval,
  persistence, batch timing, and shell polling. The browser E2E was inspected,
  not run. No existing test establishes a readable mail/event preview.

The execution gate already exists. A complete, automatically visible preview in
both clients still needs work.

## Configuration and permission boundaries

Start with one connected Google account. Make each capability independently
configurable using the existing policy vocabulary; all default to `deny`.

```toml
[google]
gmail_read = "allow"
gmail_send = "ask"
calendar_read = "allow"
calendar_write = "ask"
calendar_ids = ["primary"]

# optional maintainer override for development builds; global config only
# oauth_client_file = "/home/me/.config/rope/google-client.json"
```

`deny` disables the capability and removes its tools from the advertised catalog.
`ask` enables it but requires tool approval. `allow` permits execution without a
prompt. The account must also have granted the required Google scopes: a policy
change alone cannot authorize Google access. The UI shows configured capability,
actual grant, and whether reconnecting is needed separately.

| Capability | Google scopes, under `https://www.googleapis.com/auth/` |
| --- | --- |
| Gmail read/search | `gmail.readonly` |
| Gmail send | `gmail.send` |
| Calendar read | `calendar.events.readonly`, `calendar.calendarlist.readonly` |
| Calendar write | `calendar.events`, `calendar.calendarlist.readonly` |

Add basic account identity scopes (`openid email`) to identify the connection
without requiring Gmail read access. Do not request `gmail.modify`,
`gmail.compose`, or full mailbox access. Send-only works without read permission;
reading mail does not enable sending. Google Calendar's event-write scope also
permits reads, so Calendar read/write separation is enforced by Rope as well.
[Gmail scopes](https://developers.google.com/workspace/gmail/api/auth/scopes),
[Calendar scopes](https://developers.google.com/workspace/calendar/api/auth).

Treat global capability policies as the ceiling: project config may disable a
capability or tighten `allow` to `ask`, but cannot enable a globally denied
capability or loosen `ask`. Implement this explicitly outside the ordinary TOML
overlay. Calendar IDs are a Rope-side allowlist, also intersected with project
restrictions; Google consent is not limited to that list. Resolve `primary` to
the connected account's actual calendar ID before checking or previewing calls.

Show calendar names and IDs in settings. Choose the primary calendar by default;
additional calendars require user selection. Connection credentials are global
and cannot be supplied or redirected by project configuration.

## Connecting during onboarding or later

1. After saving provider setup, offer an optional Google connection step, with
   separate controls for reading mail, sending mail, reading calendars, and
   changing calendar events. Skipping or cancelling leaves Rope usable.
2. Use the same setup flow later through `rope google connect`, `/google` in the
   TUI, and a Google settings panel in the web client. Also provide status,
   permission changes, reconnect, and disconnect. CLI management must work
   without starting a model session or requiring provider onboarding.
3. For a local installation, open the system browser and receive the OAuth
   callback on a temporary loopback listener. Use authorization code + PKCE
   S256, random state, a timeout, and cancellation. Request the selected scope
   set and retain refresh credentials for silent renewal. Print a clickable URL
   if browser launch fails. Do not use Rope's automated browser context.
4. Validate the returned grants and identity before enabling tools. Partial
   consent leaves only the granted capabilities usable. Preserve a working
   connection if an attempt to add access is cancelled; never silently switch
   the connected account during a permission update.
5. Adding permissions later repeats authorization for the complete desired
   scope set. Google explicitly does not support incremental authorization for
   installed apps. Do not base the desktop flow on `include_granted_scopes`.
   [Google desktop OAuth](https://developers.google.com/identity/protocols/oauth2/native-app).
6. Disabling a capability blocks its calls immediately in Rope, including calls
   waiting for approval. Explain that this does not remove previously granted
   Google scopes. Provide a separate revoke-and-reconnect path for reducing the
   Google grant; a full disconnect removes local credentials and attempts remote
   revocation, reporting if that remote step fails.

For a headless host, reuse an existing connection or run the same connect command
with a loopback callback port forwarded over SSH to the browser's computer.
The web UI must not offer a remote user an unusable server-loopback URL as if it
were a local login. Arbitrary remote-browser/mobile sign-in needs a separately
configured Web OAuth client and registered HTTPS callback; defer that additional
flow unless it is a launch requirement. Connected headless servers can already
execute tools and receive approvals through either client.

Store refresh credentials separately from `config.toml`, sessions, and model
context, in a user-private global file with atomic replacement and restrictive
platform permissions. Never serialize them in snapshots, debug requests, tool
results, or logs. One shared connection in the core owns refresh coordination;
serialize credential writes across Rope processes as well. Keep previous refresh
credentials when a renewal omits a replacement; clear authorization state on
revocation/`invalid_grant` and expose a reconnect action without repeated prompts.

Connection changes refresh Google tools in all open sessions before their next
model request. Recheck capability, account identity, and connection revision at
execution time so an old registry entry or pending approval cannot bypass a
disconnect or permission change. Keep account management outside model tools.

## Initial tools

| Tools | Behavior |
| --- | --- |
| `gmail_search` | Gmail query syntax; bounded result page with IDs, headers, snippets, and next-page token |
| `gmail_read` | Read a message or thread; readable bodies, message IDs, reply headers, attachment metadata |
| `gmail_send` | Structured To/Cc/Bcc, subject, plain-text body, optional explicit reply/thread metadata |
| `calendar_list` | List selected calendars and their time zones/access roles |
| `calendar_search`, `calendar_get` | Bounded date-range event search and event details, including recurring-instance identity and ETag |
| `calendar_create`, `calendar_update`, `calendar_delete` | Explicit event fields and notification behavior, guarded by write policy and calendar access |

Implement HTTP calls with the existing `reqwest` client stack and small Google
modules. Use a maintained MIME parser/builder for mail headers, encoding, and
multipart bodies. Convert HTML-only messages to text without loading remote
content. Preserve pagination and truncation indicators within Rope's output
budget. Email and calendar descriptions remain untrusted tool-result content.

Send-only never looks up a mailbox profile or original message behind the user's
back. Replies require explicit threading headers supplied from an earlier read;
otherwise send a new message. The preview is a local prepared message, not a
Gmail draft. Encode the prepared MIME only for the API request.
[Gmail sending](https://developers.google.com/workspace/gmail/api/guides/sending).

Calendar inputs distinguish all-day dates from timed events and retain explicit
time zones. Updates patch only requested fields. Start with ordinary events and
individual recurring instances; reject whole-series edits until their behavior
and preview are implemented. Display whether invitations, updates, or
cancellations will notify attendees. Do not infer additional guests or use
opaque natural-language quick-add mutations.

Defer mailbox modification/deletion, draft management, attachment transfer,
aliases, push sync, multiple accounts, calendar sharing, and recurrence-rule
editing. These are separate features, not prerequisites for the requested tools.

## Approval previews and exact execution

Extend the current approval mechanism with a small optional preparation step
and structured preview. Ordinary tools retain their existing path. Google write
tools validate and normalize their input into a pending operation before asking;
both the preview and eventual HTTP request derive from that same operation.

- Mail preview: sending account, every To/Cc/Bcc recipient, subject, full readable
  body, and reply target if present. No raw MIME/base64 as the main display.
- Calendar preview: account, calendar name/ID, action, title, dates, time zone,
  location, description, attendees, and notification behavior. Updates show
  before/after values; deletes show the event being removed.
- Fetch any Calendar state needed for the preview under the enabled write
  capability, whose Google scope permits that read. Freeze the retrieved ETag.
  Apply updates/deletes with `If-Match`; a conflict requires a fresh preview and
  approval rather than silently changing the operation.
  [Calendar conditional writes](https://developers.google.com/workspace/calendar/api/guides/version-resources).
- Display the preview automatically in both clients, with scrolling for the
  complete content and raw arguments available separately. Render user content
  as text, never executable HTML. Reconnecting clients receive the same pending
  preview through snapshots regardless of tool-block collapse state.
- With policy `ask`, Google mutations offer approve-once and deny. Suppress
  session-wide approval in both UI and server validation for these calls. Users
  who deliberately want unattended writes can set the capability to `allow`.
  Read tools can retain ordinary session grants, keyed by account, OAuth client,
  capability revision, and tool so old grants cannot cross connections.
- Editing the proposed operation requires a new tool call/approval. Preserve
  Rope's first-wins decisions and turn cancellation. Approval is not permission
  to execute a later modified body or switch to another account.
- Bound read retries and refresh attempts. Do not blindly retry a mail send or
  event creation after an ambiguous timeout. Report that the outcome is unknown
  and require reconciliation before another mutation. Cancellation after a
  request was dispatched cannot promise the remote action was undone.

## Implementation sequence

1. **Configuration and connection:** add `src/google/{mod,auth,credentials}.rs`,
   configuration parsing/validation, management commands, and the optional
   onboarding step. Connect the shared Google state to `Core`; implement the
   settings/status requests in `src/protocol.rs` and `src/server.rs` plus TUI/web
   settings. Reuse `ToolRegistry::replace_origin` for catalog updates.
2. **Read tools:** add `src/tool/google/{mod,gmail,calendar}.rs`, register only
   enabled/granted tools, implement pagination, MIME decoding, and calendar
   filtering. Keep the transport/auth helpers separate from tool schemas.
3. **Preview path:** extend `src/tool/mod.rs`, runtime approval state,
   `src/core/state.rs`, protocol snapshots, `src/ui/{mod,state}.rs`, and
   `web/js/status.js`. Add the mail/event preview rendering and enforce
   approve-once for mutations before exposing writes.
4. **Write tools and verification:** implement send/create/update/delete using
   prepared operations, then update `config.example.toml`, `README.md`,
   `FEATURES.md`, and client/server documentation with the implemented behavior.

Use mock HTTP endpoints for deterministic tests; CI must not require Google
credentials. Cover read-only/send-only combinations, partial consent, cancelled
reconnect, project restrictions, refresh concurrency, token redaction, live
catalog changes, and disconnect during a pending approval. Verify no mutation
request occurs before approval; the approved recipients/body/event changes must
equal the executed payload. Cover denial, duplicate decisions, restored clients,
stale ETags, ambiguous sends, Unicode mail, HTML-only bodies, pagination, and
all-day/DST boundaries. Exercise previews in TUI rendering tests and web E2E,
including long messages and Bcc. Run `cargo test --locked` and the web suite;
manually smoke-test consent and refresh with a dedicated Google test account.

## Confirmed launch choice: Rope-owned OAuth app

Ship a Rope-owned Desktop OAuth client. Users only choose permissions and sign
in; they must not create a Google Cloud project, enable APIs, or supply client
credentials. A client JSON override is only a maintainer development option.

The Rope maintainer must register the application once in a Google Cloud project,
enable Gmail and Calendar APIs, configure its consent screen, create the Desktop
OAuth client, and complete the applicable verification. Ship that app identity
with Rope. This is an app-level prerequisite, not per-user setup, and it does
not require hosting Rope itself on Google Cloud. If no maintainer will own this
registration, direct Google API sign-in cannot ship as planned; delegating OAuth
to a third-party integration service would be a separate architecture decision.
[Google client registration](https://developers.google.com/workspace/guides/create-credentials),
[consent setup](https://developers.google.com/workspace/guides/configure-oauth-consent).

A public Rope client needs Google app/scopes verification. Gmail read access is
restricted, while send access is sensitive. Google's rules also address sending
restricted-scope data to servers; Rope passes tool results to the selected model
provider, so that data path needs explicit review before public launch rather
than assuming a local executable is exempt. Explain that data flow during setup.
[Gmail verification requirements](https://developers.google.com/workspace/gmail/api/auth/scopes).

An external OAuth project left in Testing issues seven-day refresh tokens for
these scopes, which would defeat persistent low-friction login. Resolve the
publishing status for Rope's public client before launch.
[Google refresh-token expiration](https://developers.google.com/identity/protocols/oauth2#expiration).
