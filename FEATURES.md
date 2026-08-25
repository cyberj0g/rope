# Rope features

## Runtime and providers

- provider-independent streaming runtime with an OpenAI-compatible provider
- deterministic mock provider for runtime tests
- streamed text and OpenAI function-call assembly
- layered `~/.config/rope/config.toml` and `./.rope/config.toml` configuration
- named model profiles with context size, temperature, and vision capabilities; omitted names use the API model ID
- automatic global and project `AGENTS.md` instructions

## Sessions

- automatic sessions under `~/.local/share/harness/sessions`
- persisted 2-3 word model-generated titles for automatically named sessions, created after the first completed response
- JSONL conversation persistence after completed turns
- persisted session token totals and configurable per-token cost estimates
- `--session NAME` to create or resume a session, plus `/new [NAME]` and `/save`
- optional positional startup request submitted as soon as the terminal UI opens
- exit summary with tokens used, estimated cost, and the exact session resume command

## Tools

- iterative model → tool → model execution
- immediate Escape cancellation with force-killed child processes
- automatic 2/5/10/30-second retry backoff for transient model failures
- configurable context fill tracking and automatic continuation compaction
- preserved visible transcripts with persisted `Context compacted` markers
- model-managed `update_plan` state persisted across restarts, with visible tool calls and only the latest full plan projected into model context
- built-in `read`, `write`, `edit`, `shell`, `grep`, and `glob` tools
- persisted per-call diffs for `write` and `edit`, opened from the tool header without mixing in unrelated changes
- browser-backed `web_search` using DuckDuckGo with Bing fallback, Chromium-family browser discovery, and a `ROPE_BROWSER` override
- text-first `web_browser` using browser-visible content after JavaScript rendering, with resolved visible links and no model-controlled truncation
- automatic post-consumption `web_browser` result ejection from model context while retaining the full visible and persisted transcript
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
- streamed tool-call arguments, results, and approval prompts
- live tool counters that switch from characters to lines after the first literal or escaped line break
- streamed and persisted reasoning blocks (`reasoning` and legacy `reasoning_content`)
- collapsible messages plus collapsed-by-default thinking and tool sections
- live elapsed time on thinking and tool calls with compact duration units
- one-space conversation content padding with flush section headers
- fixed-width, color-coded connecting, waiting-for-first-response, generating, tool-running, idle, and error status
- failed turns preserve the visible transcript and append the error to the conversation
- case-insensitive `Ctrl+F` chat search with highlighted, wrapping `F3` navigation
- separately colored model and reasoning details on the padded input box; session tokens and cost on the status bar
- generating model recorded beside each assistant response
- full-width conversation view with deliberate trailing whitespace
- bounded bottom-follow chat scrolling that holds the viewport through streaming, collapses, and full-screen diff visits
- distinctly colored session, token, context, price, and current-directory fields on the status bar
- asynchronously refreshed Git pane with a mouse-resizable split, clickable files, independently scrollable status and diff views, fixed back navigation, and viewport indicators; plus a bounded full-screen `/diff` view
- auto-opening plan pane below Git status with live progress, `/plan` visibility control, independent scrolling, and a mouse-resizable horizontal split
- drag-to-copy conversation selection with a non-blocking clipboard toast
- filtered slash-command palette with keyboard navigation and command hotkeys
- non-blocking clipboard and `/image` image attachments with an elapsed processing plate
- inline, vertically sliced Sixel, iTerm2, and Kitty image rendering that follows chat scrolling when supported by the terminal, with text fallback
- bracketed paste plus direct Shift+Insert clipboard fallback
- `/thinking` and `/tools` global visibility toggles plus clickable model and reasoning selectors
- `/add PATH`, `/drop PATH`, and `/diff`
