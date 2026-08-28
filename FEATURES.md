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
- built-in defaults for popular OpenAI-compatible model families, including Qwen3.8
- searchable recent-first model picker shared by `/model`, Alt+M, and the clickable model status
- automatic global and project `AGENTS.md` instructions

## Sessions

- automatic sessions under `~/.local/share/harness/sessions`
- persisted 2-3 word model-generated titles for automatically named sessions, created after the first completed response
- JSONL conversation persistence after completed turns
- persisted session token totals and per-model cost estimates, hidden when any model used in the session has no configured token price
- `--session NAME` to create or resume a session, plus `/new [NAME]` and `/save`
- optional positional startup request submitted as soon as the terminal UI opens
- exit summary with tokens used, estimated cost when available, and the exact session resume command

## Tools

- iterative model → tool → model execution
- immediate Escape cancellation with force-killed child processes, preserved partial output, failed in-flight tools, and a persisted cancellation marker
- automatic 2/5/10/30-second retry backoff for transient model failures
- configurable context fill tracking and automatic continuation compaction
- preserved visible transcripts with persisted `Context compacted` markers
- model-managed `update_plan` state persisted across restarts, with visible tool calls and only the latest full plan projected into model context
- tool approval controls in the composer with paused execution timing across batched calls, session-persisted approvals, and decision markers retained in conversation history
- built-in `read`, `write`, `edit`, `shell`, `search_files`, and `list_files` tools, with optimized ripgrep execution and ignore-aware built-in fallbacks
- persisted per-call diffs for `write` and `edit`, opened from the tool header without mixing in unrelated changes
- Patchright-backed `web_search` using DuckDuckGo with Bing fallback, a shared headless Chromium context, version-matched browser identity, a process-lifetime profile, and a `ROPE_BROWSER` override
- text-first `web_browser` using browser-visible content after JavaScript rendering, with resolved visible links, a shared browser session, and no model-controlled truncation
- reproducible, checksum-verified Node and Patchright runtime embedding with lazy versioned extraction on first browser use
- eager Patchright extraction plus Chrome/Chromium diagnostics during first-run setup, including `ROPE_BROWSER` override guidance
- automatic post-consumption `web_browser` result ejection from model context while retaining the full visible and persisted transcript
- per-call tool output capped at roughly one fifth of the available model context with an explicit truncation marker
- multimodal `view_image` tool advertised only by vision-enabled model profiles
- per-tool `allow`, `ask`, and `deny` policies in `config.toml`
- executable JSON tools discovered from `.rope/tools/` and `~/.config/rope/tools/`
- local external tools override global tools with the same filename

External tools receive their function arguments as JSON on stdin and must return
`{"output":"..."}` on stdout.

## Coder UI

- global persistent prompt history with Bash-style Up/Down navigation and Shift+Enter newlines
- Fish-style `Alt+Left/Right` word jumps and `Alt+Backspace` word deletion on non-alphanumeric boundaries
- bracketed multiline paste handling, width-aware growing composer, and configurable collapsed large-paste tokens
- original soft line breaks preserved when rendering user messages
- sequential duplicate filtering in persistent prompt history
- CommonMark/GFM rendering with inline emphasis, links, aligned tables, and syntax-colored fenced code blocks
- streamed tool-call arguments, line-break-preserving results, and approval prompts
- live tool counters that switch from characters to lines after the first literal or escaped line break
- streamed and persisted reasoning blocks (`reasoning` and legacy `reasoning_content`)
- collapsible messages plus collapsed-by-default thinking and tool sections; right-clicking anywhere in an expanded section collapses it
- live elapsed time on thinking and tool calls with compact duration units
- one-space conversation content padding with flush section headers
- fixed-width, color-coded connecting, waiting-for-first-response, generating, tool-running, idle, and error status
- failed turns preserve the visible transcript and append the error to the conversation
- case-insensitive `Ctrl+F` chat search with highlighted, wrapping `F3` navigation
- separately colored model and reasoning details on the padded input box; session tokens and cost on the status bar
- generating model recorded beside each assistant response
- full-width conversation view with deliberate trailing whitespace
- bounded bottom-follow chat scrolling that holds the viewport through streaming, collapses, and full-screen diff visits
- distinctly colored session, token, context, price, and current-directory fields on the status bar, plus estimated live generation speed and the exact reported average while idle
- asynchronously refreshed Git pane that updates after every tool call as well as at turn end, cancel, and failure, with git runs serialized and coalesced so at most one refresh is in flight; mouse-resizable split, clickable files, independently scrollable status and diff views, fixed back navigation, and viewport indicators; plus a bounded full-screen `/diff` view
- auto-opening plan pane below Git status with live progress, `/plan` visibility control, independent scrolling, and a mouse-resizable horizontal split
- drag-to-copy conversation selection with a non-blocking clipboard toast
- recent-first filtered slash-command palette with keyboard navigation and command hotkeys
- non-blocking clipboard and `/image` image attachments with an elapsed processing plate
- inline, vertically sliced Sixel, iTerm2, and Kitty image rendering that follows chat scrolling when supported by the terminal, with text fallback
- bracketed paste plus direct Shift+Insert clipboard fallback
- `/thinking` and `/tools` global visibility toggles plus clickable model and reasoning selectors
