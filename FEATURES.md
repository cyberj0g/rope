# Rope features

## Distribution

- test-only GitHub Actions checks for regular changes, plus cached tagged releases for Linux x64, macOS arm64, and Windows x64

## Runtime and providers

- provider-independent streaming runtime with an OpenAI-compatible provider
- Responses API by default for OpenAI and local vLLM endpoints, with endpoint-level `chat_completions` compatibility
- stateless Responses conversations backed by Rope sessions, including persisted opaque reasoning and model-scoped output-item replay that remains safe across model switches
- deterministic mock provider for runtime tests
- streamed text and OpenAI function-call assembly
- layered `~/.config/rope/config.toml` and `./.rope/config.toml` configuration
- colored, keyboard-navigable first-run setup for multiple OpenAI-compatible providers with optional API-key entry, API model discovery, and private global config creation
- OpenAI-first onboarding with official endpoint recognition and pinned current OpenAI models above the full API catalog
- explicit per-model provider routing, including duplicate model IDs across endpoints and legacy single-endpoint config compatibility
- named model profiles with context size, temperature, reasoning defaults/options, and vision capabilities; omitted names use the API model ID
- minimal `[[models]]` entries use neutral omitted-field defaults (no vision, temperature, or reasoning) instead of inheriting the built-in default model's profile
- stale saved model selections fall back to the config's model with a startup warning instead of failing to start
- built-in defaults for popular OpenAI-compatible model families, including Qwen3.8
- searchable recent-first model picker shared by `/model`, Alt+M, and the clickable model status; model selections in the terminal UI persist as the next startup default
- current time pinned as a hidden `<runtime-context>` block at the end of every user message — invisible in the chat but visible in the raw request inspector, with the session plan riding in the block only while any plan step is still open (a fully completed plan is already fully present as un-compacted `update_plan` calls) — built once at send time and never rewritten afterwards, like the rest of history; steer messages carry no runtime context; the working directory included in the system prompt alongside global and project instructions
- automatic global and project `AGENTS.md` instructions

## Sessions

- browser UI sources in `web/`, with separate HTML, CSS, and native JavaScript modules embedded in the executable without a frontend build step
- one shared core with an in-process TUI client, optional authenticated WebSocket clients via `--listen`, and terminal-free `--headless` server mode
- headless mode writes timestamped activity logs to stderr: project and listener startup, client connections, session readiness, model requests and response timing, tool execution, approval waits, retries, compaction, turn completion/cancellation, errors, and graceful shutdown; session activity is labeled by session ID, without streaming text or tool argument/output dumps
- one project directory per server, a live shared session catalog, concurrent turns in separate sessions, and multiple viewers/controllers of the same session
- shared message acceptance and steering, first-wins approvals, turn-scoped cancellation, and revision-checked model/reasoning settings persisted per session
- switching sessions or disconnecting leaves work running; late clients receive snapshots containing partial text, queued steers, approval waits, timers, usage, and plans — thinking text and tool arguments/output are withheld per connection while their section is collapsed, and are delivered in full (with their live updates) once that client first expands the section
- sequenced session updates, bounded subscriptions with snapshot recovery on lag, and mutation reply deduplication across reconnects within a server lifetime
- per-session shell jobs and browser contexts, project-bound session metadata, atomic metadata replacement, and exclusive session writer locks
- loopback-default listener, server token authentication, explicit browser Origin allowlist, authenticated image upload/download, and a mobile-first web client served at `/` — conversation rendering, tools, thinking, plans, Git, approvals, steering with queued-steer badges, in-chat search that survives editing and clears on close, a session sidebar with per-conversation deletion (inline confirmation) and a paginated list (20 rows per page plus a "Load more" affordance) whose filter runs on the server against every session rather than only the loaded rows, a model & reasoning sheet whose reasoning-effort options for the current model lead the sheet (above the model list) and whose model switching is revision-checked per session, with the active model and its current reasoning effort shown on the status strip, and thinking/tool sections whose content is withheld while collapsed — the header keeps the tool name, live status, and running timer, and the first expansion shows a loading placeholder until the content arrives — image/camera attachments whose plates spin only during the transfer, distinct Send/Steer and Cancel composer controls (Esc cancels even from the focused composer), tool results that render their published images, diffs, and `send_file` files (inline image or a file tile that downloads the file on click), assistant response headers that show the turn's total time, and a Git pane whose rows open per-file diffs
- cancellable manual compaction and graceful process shutdown that preserves interrupted work and stops session tools
- automatic sessions under `~/.local/share/harness/sessions`
- persisted 2-3 word model-generated titles for automatically named sessions, created after the first completed response
- JSONL conversation persistence after completed turns
- persisted session token totals and per-model cost estimates, hidden when any model used in the session has no configured token price
- `--session NAME` to create or resume a session, plus `/new [NAME]`
- `/session` popup listing the project's sessions plus unbound legacy sessions — name, creation date, and summary (the generated title, else the first user message) — with a live filter and keyboard navigation, switching clients without interrupting running turns
- `/compact` to compact the conversation on demand while idle: summarizes exactly what the next request would send (previous summary plus the messages that outlived it), persists the summary, the boundary, and the `Context compacted` transcript marker at the end of the live conversation (including clients attached during compaction), and refreshes the context gauge; refuses with a notice while a response is running and with an error when there is nothing to compact yet
- optional positional startup request submitted as soon as the terminal UI opens
- exit summary with tokens used, estimated cost when available, and the exact session resume command

## Agents and subagents

- Markdown agent files with a `+++`-delimited TOML front matter, discovered from `~/.config/rope/agents/` (global) and `<project>/.rope/agents/` (project), where a project file replaces the global definition with the same ID; the filename stem is the stable ID (lowercase, digits, `-`, `_`), `description` is required, `mode` is `primary` / `subagent` / `all` (default `all`) controlling selection versus delegation eligibility, `model` names an existing model profile (omitted = the configured default), `can_call_subagents` defaults to `false`, and `[tools]` holds per-agent `allow` / `ask` / `deny` overrides keyed by exact tool name or category (`external`, `mcp`, or a built-in name); anything else is a validation error naming the file and field, and malformed files fail startup
- the built-in `assistant` is always present, reserved, selectable, delegable, and `can_call_subagents`: the ordinary Rope session with its existing prompt assembly, default model, and `[tools]` configuration; every catalog contains it and a child session may keep a subagent-only definition it was created on
- agent selection (`/agent [id]`, Alt+A, the clickable agent name on the input title line, and a filtered keyboard-navigable picker) is revision-checked and requires an idle session; switching applies the agent's default model and reasoning defaults atomically, persists the selection with the session settings (the built-in assistant is stored as `None`), and a saved agent that has vanished keeps the transcript inspectable but blocks new turns with a notice until an agent is selected
- each assistant response is stamped with the agent that produced it, so historical attribution survives later switches
- per-agent tool policy resolves the agent's exact tool entry, then its category entry, then the configured base policy; denied tools are absent from the model's schema and approval requests are keyed by agent, so one agent's session grants never apply to another; the agent's Markdown body rides at the end of the project system prompt of every request
- the `subagent` tool delegates a self-contained task to a named agent, advertised only to agents with `can_call_subagents` and listing the delegable agents with descriptions in its schema; the runtime executes the call itself, validating the agent and prompt, and waits on the child's structured outcome as the tool result — session, agent, status, response, and tokens as one JSON document whose long text fields are cut (keeping the tail) while it stays valid JSON within the call's output budget
- child sessions are named `sub-<id>`, stay out of the root catalog, and are linked durably in both directions: the parent persists the `tool_call_id → child` record and child list before the child starts, and the child persists the creating session, turn, and call; nesting is bounded to four child levels and a parent waits on at most one direct child at a time
- a message sent to a session whose delegation is still running is routed to the active working descendant, with the routed destination in the reply and a receipt recorded in the origin transcript; cancellation walks the delegation chain deepest first, a cancelled or shut-down parent abandons its waiting call so the core stops the child's turn and lets it settle, and the child's terminal state (completed, user cancelled, failed, or interrupted by a restart) resumes the parent's call exactly once
- the TUI shows the active agent, opens a child by clicking its tool call or pressing Alt+Down, and returns to the parent with Alt+Up; the web UI shows an agent picker, `/agent`, a live subagent card that opens the normal child chat, and a parent breadcrumb; both clients can steer and cancel the viewed child

## Tools

- UTF-8 file reads with optional 1-based line offsets and line limits
- iterative model → tool → model execution, with the 64 tool call cap applied per assistant message so long turns keep running instead of failing after a fixed number of model turns
- the tool calls one assistant message batches run at the same time, and the turn waits for the whole batch before asking the model again: the model receives one consistent snapshot, always in the order its calls were made, while streamed output and results still arrive per call as each finishes. Interactive approvals are requested one call at a time (a session carries a single pending approval), a batch of `subagent` calls still delegates one child at a time, and concurrent `write`/`edit` calls take a per-file lock so two changes to one file are applied in order instead of the last one silently winning; a call the turn cannot run at all answers with its own error while the calls that did run keep their recorded place, and a turn the user stops keeps every result that had finished and closes only the calls that never answered
- immediate Escape cancellation with force-killed command process trees, preserved partial model and command output, failed in-flight tools, and the turn's completed work — assistant messages, tool results, and steers the model already saw, plus any compaction it applied — persisted into the conversation with a cancellation marker (open tool calls closed by cancellation results), so the next turn continues from the real state of the work instead of from before the cancelled turn
- steering messages: prompts sent while a turn is in progress are queued and injected as `Steer` messages into the conversation at the turn's next model request — even while tools run — and persist with the turn; a steer that misses the turn's final model request is resubmitted as a fresh turn, and steers queued on a cancelled turn are persisted with the cancellation marker
- automatic 2/5/10/30-second retry backoff for transient model failures
- configurable context fill tracking and automatic continuation compaction that runs the summary request without reasoning when the model supports it (lightest effort otherwise — reasoning would spend the shared output budget and could starve the summary itself), asks for the summary both in a system prompt and as the final user turn (long conversations that replay the model's own earlier reasoning routinely ignore a leading system prompt and role-play the conversation instead), and replays the persisted summary into model context, with the turn-start fill prediction anchored on the provider's last reported usage plus the new user message (whole-context estimate only as the cold-start fallback) and the summary request itself budgeted to the model context: output budgeted to the remaining context and scaled to the conversation being summarized (one eighth, floored at 4096 and capped at one quarter of the model context, with a 128-token minimum), the oldest conversation trimmed until the input *and* that budget both fit while retaining the final summary instruction and refusing to summarize when no source messages remain so a reasoning model always has room to think, a clear refusal instead of a rejected request when even the minimum summary no longer fits, and a clear error — never a persisted summary — whenever the response yields no output text: truncation (including providers like vLLM that report a budget-exhausted response as a completed event carrying `incomplete_details`), a stream that ends without a terminal event, or a completed response that only thought, so a cut-off or role-played reasoning monologue never becomes the compaction summary
- automatic compaction checks the configured fill threshold between model requests throughout a turn, using reported usage plus tool results and queued steers; it can summarize completed work from the first turn and repeat as the turn grows, recover a response cut off at the context limit instead of treating it as a finished turn, and refresh the context gauge before continuing, preserving the full transcript and correct continuation boundary on completion or cancellation, with steers received during compaction delivered to the next model request
- preserved visible transcripts with persisted `Context compacted` markers whose summary stays in history as a collapsed chat section, inserted before the current user with every stored tool-block index shifted so mid-turn results still land on the active tool block
- model-managed `update_plan` state persisted across restarts, with the plan riding pinned in the current user message's runtime context while any step is still open — every `update_plan` call (arguments and result) stays in the model context un-compacted, so plan history is append-only and the server's KV cache carries over turn boundaries — and a call arriving without a `plan` array gets a clear error telling it to send the complete plan instead of an opaque decode failure
- tool approval controls in the composer with paused execution timing across batched calls, session-persisted approvals, and decision markers retained in conversation history
- built-in `read`, `write`, `edit`, `shell`, `shell_poll`, `shell_cancel`, `search_files`, `list_files`, and `org_outline` tools, with optimized ripgrep execution and ignore-aware built-in fallbacks
- `org_outline` returns the Org-mode heading hierarchy of a file as a flat, source-ordered node list with inclusive 1-based line ranges for each subtree, from a single linear scan that ignores heading-like text inside `#+begin_...#+end_` blocks, so large Org files can be navigated with `read`/`edit` ranges instead of loading whole files into context
- model-driven long-polling `shell`: each call returns a compact status/job_id/output envelope once the command exits, the yield period (default 10s, capped at 30s) expires, or the envelope's output payload fills the budget; finished and cancelled results drain in budgeted chunks marked `has_more`, and `shell_poll` retrieves the remainder without gaps or repeats until the final chunk, after which the job is gone; running envelopes always fit their output budget with the status and job_id prioritized, and `shell_cancel` stops a job deliberately and returns its remaining output as cancelled
- bounded retention of shell job output: the job keeps only a 256 KiB tail of the stream, delivered prefixes are compacted away, and oldest undelivered bytes beyond the cap are discarded with a discard note in the envelope, so a verbose command cannot grow memory without bound
- `shell_poll` and `shell_cancel` are always allowed because they can only observe or stop an already-approved command; shell jobs never outlive the turn that started them — turn end, failure, Escape, and shutdown all kill them together with their whole process tree: the full process group on Unix (held until turn end, including already-delivered jobs) and a Windows job object the shell is assigned to while still suspended, so a containment failure or a failed thread resume fails the call and the kill-on-close job handle terminates the suspended tree
- live `shell` output read from both pipes as the command runs, with stdout and stderr interleaved in arrival order and a UTF-8-safe decoder for split multi-byte sequences
- per-call tool output capped at roughly one fifth of the available model context with an explicit truncation marker, and live `shell` output streaming bounded by the same cap, sliced at a character boundary so one delta never overshoots it; the cap is measured on the populated, JSON-serialized Tool message — role, call id, and content escaping counted — after reserving the message's own framing, and floored at 32 tokens so control fields like a job_id survive near the context limit, so a tool result never pushes the next model request past the limit under Rope's own estimator; within a concurrent batch the cap is additionally bounded by the call's equal share of the context a turn keeps before it would compact — or of the room left, when no truncation could keep the batch under that line anyway — so the results of a batch never spend the reserve the following model request needs; when even the floor no longer fits, the conversation is compacted mid-turn — keeping the pending assistant calls and their immediately preceding prompts — before the batch starts, and the turn fails with a clear error if compaction cannot free the room
- image results counted in the context budget at the OpenAI `auto`-detail cost (85 base tokens plus 170 per 512px tile after the 2048px fit and 768px shortest-side cap; unknown dimensions reserve the per-image maximum), with `view_image` reading header dimensions so its reservation is exact, and a tool image that would crowd the result past its budget dropped with a note in the content so the next request still fits
- persisted per-call diffs for `write` and `edit`, opened from the tool header without mixing in unrelated changes
- Patchright-backed `web_search` using DuckDuckGo with Bing fallback, a headless Chromium context shared within each session, version-matched browser identity, a session-lifetime profile, automatic cookie-popup opt-out via DuckDuckGo AutoConsent, and a `ROPE_BROWSER` override
- text-first `web_browser` using browser-visible content after JavaScript rendering, with resolved visible links, a shared browser session, and no model-controlled truncation
- reproducible, checksum-verified Node, Patchright, and AutoConsent runtime embedding with lazy versioned extraction on first browser use
- eager Patchright extraction plus Chrome/Chromium diagnostics during first-run setup, including `ROPE_BROWSER` override guidance
- automatic post-consumption `web_browser` result ejection from model context while retaining the full visible and persisted transcript
- multimodal `view_image` tool advertised only by vision-enabled model profiles
- `send_file` tool that sends a local file into the chat: images render inline in the web UI like `view_image` results, other files appear as a clickable file tile that downloads the file, and the TUI renders the path as a link that opens the file in its default application; the model sees the path, size, and an explicit delivery confirmation that no further delivery action is needed for that file in the tool output — the file reference never enters the model context or provider requests — and the web client fetches files through authenticated session-scoped URLs that resolve only published chat files (a shared 100 MiB limit checked before sending and while downloading, safely encoded download filenames, extension-guessed content type); each send has its own image cache entry so resending a changed file displays the new version; delivered images, file tiles, and TUI links appear below the tool section and stay visible when its details are collapsed or hidden
- per-tool `allow`, `ask`, and `deny` policies in `config.toml`
- executable JSON tools discovered from `.rope/tools/` and `~/.config/rope/tools/`
- local external tools override global tools with the same filename
- stdio MCP servers with paginated discovery, stable provider-safe tool names,
  include/exclude filters, environment-backed secrets, bounded startup and call
  times, cancellation, graceful shutdown, and approval grants tied to the
  server configuration fingerprint
- Streamable HTTP MCP servers with environment-backed bearer authentication
  and custom headers, stateless/session-aware operation, and session recovery
- atomic MCP tool-catalog refresh on `tools/list_changed`, with updated schemas
  included in the next model request and rich tool results preserving text,
  structured JSON, resource links, embedded text resources, and images

External tools receive their function arguments as JSON on stdin and must return
`{"output":"..."}` on stdout.

## Coder UI

- global persistent prompt history with Bash-style Up/Down navigation and Shift+Enter newlines
- Fish-style `Alt+Left/Right` word jumps and `Alt+Backspace` word deletion on non-alphanumeric boundaries
- bracketed multiline paste handling, width-aware growing composer, and configurable collapsed large-paste tokens
- original soft line breaks preserved when rendering user messages
- sequential duplicate filtering in persistent prompt history
- CommonMark/GFM rendering with inline emphasis, links, aligned tables, and syntax-colored fenced code blocks
- streamed tool-call arguments, live shell output while the command runs, line-break-preserving results, and approval prompts
- live tool counters that switch from characters to lines after the first literal or escaped line break, counting streamed output as it arrives
- streamed and persisted reasoning blocks (`reasoning` and legacy `reasoning_content`)
- collapsible messages plus collapsed-by-default thinking and tool sections; right-clicking anywhere in an expanded section collapses it
- live elapsed time on thinking and tool calls with compact duration units
- total turn time on the final assistant response header (from the user's prompt to the completed answer), persisted with the turn and restored on session reload
- one-space conversation content padding with flush section headers
- blank lines around You, Steer, Assistant, and System messages only; thinking and tool blocks render line after line with no spacing between them
- fixed-width, color-coded connecting, waiting-for-first-response, generating, tool-running, idle, and error status
- terminal bell that rings when the agent's turn finishes
- failed turns preserve the visible transcript and append the error to the conversation
- case-insensitive `Ctrl+F` chat search with highlighted, wrapping `F3` navigation
- separately colored model and reasoning details on the padded input box; session tokens and cost on the status bar
- generating model recorded beside each assistant response
- intermediate assistant turns whose model request ends in tool calls relabel live from `Assistant` to a gray `Status` header as soon as the first tool call starts streaming, while the turn's final answer keeps the blue `Assistant` header; reloaded sessions apply the same rule from persisted tool calls
- steering: the composer accepts a prompt while a turn is in progress and renders it as a distinct yellow `Steer` message; the input title switches from `Esc to cancel` to `Enter to steer · Esc to cancel` while generating
- full-width conversation view with deliberate trailing whitespace
- bounded bottom-follow chat scrolling that holds the viewport through streaming, collapses, and full-screen diff visits
- distinctly colored session, token, context, price, and current-directory fields on the status bar, plus estimated live generation speed and the exact reported average while idle
- asynchronously refreshed Git pane that updates after every tool call as well as at turn end, cancel, and failure, with git runs serialized and coalesced so at most one refresh is in flight; mouse-resizable split, clickable files that open the file's diff in the full-screen view, independently scrollable status view, and viewport indicators; plus a bounded full-screen `/diff` view whose title names the file when a single-file diff is shown
- auto-opening plan pane below Git status with live progress, `/plan` visibility control, independent scrolling, and a mouse-resizable horizontal split
- drag-to-copy selection in the conversation and in the composer (the composer copies the underlying text, expanding collapsed paste/image plates and dropping image sentinels), with a non-blocking clipboard toast
- clickable links in every pane: clicking a URL opens it in the default browser — markdown links and raw URLs in conversation messages, thinking, tool arguments, and tool output, plus URLs in the Git pane, plan pane, and the full-screen diff view — and `send_file` tool results render the file's path as a link that opens the file in its default application; links are rendered underlined in a distinct color so they stand out, with a toast on open and a notice when the opener cannot be started
- recent-first filtered slash-command palette with keyboard navigation and command hotkeys
- non-blocking clipboard and `/image` image attachments with an elapsed processing plate
- inline, vertically sliced Sixel, iTerm2, and Kitty image rendering that follows chat scrolling when supported by the terminal, with text fallback
- bracketed paste plus direct Shift+Insert clipboard fallback
- `/thinking` and `/tools` global visibility toggles plus clickable model and reasoning selectors

## Performance

- per-block memoized chat rendering keyed on content revisions: streaming re-lays-out only the changed block, so long sessions stay responsive instead of re-parsing and re-wrapping the whole transcript on every frame
- dirty-driven frame loop that skips redraws entirely while idle, and keeps redrawing only while the display ticks on its own (live timers, streaming, toasts, pending image loads)
- memoized git diff line rendering for the full-screen diff

## Raw request inspector

- `[raw]` on conversation turns in the web UI and TUI opens the recorded provider request: complete system instructions, messages/input, tool definitions, reasoning settings, and other JSON fields, with historical requests retained across tool iterations, compaction, cancellation, and session restarts
- request bodies are stored separately under each session's `requests/` directory; ordinary snapshots and updates carry only a reference, and the full JSON is read and delivered only when the viewer opens
- mobile-first full-height web dialog with searchable, expandable JSON tree, expand/collapse all, and Copy JSON; search reveals matches inside collapsed branches, and Escape or the close button returns to the conversation
- TUI fullscreen tree opened by clicking `[raw]` or pressing `r` on a selected chat section: type to search, Ctrl+U to clear, arrows/Enter to navigate and fold, `+`/`-` to expand/collapse all, PageUp/PageDown or mouse wheel to scroll, and Escape to close
- encoded image data and opaque reasoning values retain a short prefix and original byte count; readable model input is preserved in full, while uploaded/published file bytes are never fetched by the inspector
- older turns without recorded requests show an explicit unavailable message; auxiliary session-title generation is excluded
