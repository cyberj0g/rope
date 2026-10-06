# rope

[![Build](https://github.com/cyberj0g/rope/actions/workflows/build.yml/badge.svg)](https://github.com/cyberj0g/rope/actions/workflows/build.yml)

A minimalistic terminal coding agent harness in Rust. The model chats, calls tools, edits
files, and drives a headless browser — while every one of those states is
shown live in your terminal or the included web UI.

The main focus is **observability**.
Whether it is thinking, generating, running a tool, waiting for approval,
or compacting context, you can watch it: streamed tool calls, per-call
diffs, a live git pane, a live plan pane, token and cost counters, turn
timers, and a raw provider request inspector.

## Demo

https://github.com/user-attachments/assets/c8a0636d-a37e-4877-9472-f46b9bea4d68

## Quickstart

Download a version for your system from [releases](https://github.com/cyberj0g/rope/releases).

First run walks you through provider setup — OpenAI or any
OpenAI-compatible endpoint. Or start from
the sample config (project-level overrides live in `./.rope/config.toml`):

```sh
cp config.example.toml ~/.config/rope/config.toml
```

Jump straight in by passing a startup request:

```sh
rope "what is this repo up to?"
```

## Shared core and browser clients

The TUI is an in-process client of the same core used by remote clients. Start
one server in your project directory:

```sh
rope --headless --listen 127.0.0.1:8787
```

Open `http://127.0.0.1:8787` for the included web UI — a mobile-first page
with conversation rendering, tools, plans, Git pane, approvals, steering,
and image or camera attachments. Switch models and reasoning effort, search
or delete sessions, and download files sent by the agent. Paste the token from the
file whose path Rope prints at startup (normally `~/.config/rope/server-token`).
You can also supply `ROPE_SERVER_TOKEN` or `--token-file PATH`. Open the served
page over HTTP, rather than opening `web/index.html` directly.

The web UI lives in `web/`: `index.html` contains the markup, `styles.css` the
styles, and `js/` native JavaScript modules for the connection, chat, composer,
attachments, and panels. Rope embeds these files at compile time and serves
them at `/` and `/assets/`; rebuild Rope after editing them. No frontend build
step or runtime asset directory is needed.

Use `rope --listen 127.0.0.1:8787` to attach the TUI to that same core. With a
listener enabled, exiting the TUI leaves the foreground server running; Ctrl-C
stops it. Plain `rope` still starts a private core and TUI without a listener.
Headless mode requires existing provider configuration and creates no session
until requested, unless you supply `--session NAME` or a startup request.

Clients share the catalog and can open the same session or work concurrently in
different sessions. Every client can send, steer, cancel, and approve. Sending
during a turn queues a steer; disconnecting or switching sessions leaves work
running. Each session has its own model settings, approvals, shell jobs, and
browser context. Sessions share the project's files.

The listener defaults to loopback. Opening the page over the machine's LAN
address just works — same-origin requests (matching the address the connection
arrived on) are accepted automatically. For access through a reverse proxy, use
HTTPS/WSS and add the browser's exact origin with `--allow-origin https://rope.example`.
All authenticated clients have equal control. See [the protocol documentation](CLIENT_SERVER.md)
for WebSocket messages, attachments, reconnect behavior, and current limits.


## Highlights

- **Highly observable** - the runtime's full state (generation, tool
  execution, approvals, context fill, cost) is projected into the UI in
  real time, with recorded provider requests inspectable in either UI.
- **Integrated web search and web browsing** - built-in `web_search` and
  `web_browser` tools run against a local headless browser, so all web
  traffic stays under your control for improved privacy. Cookie popups are
  automatically opted out through [DuckDuckGo AutoConsent](https://github.com/duckduckgo/autoconsent).
  No need for extra providers. Powered by embedded [Patchright](https://github.com/Kaliiiiiiiiii-Vinyzu/patchright).
- **Vision models** - the chat supports pasting and displaying images, if terminal emulator supports it.
- **OpenAI-compatible API** - works with local generation
  (vLLM, llama.cpp) and remote hosted APIs, with per-model profiles for routing.
- **MCP tools** - connects to configured stdio and Streamable HTTP MCP servers,
  discovers paginated tool catalogs, and applies Rope's per-tool approval flow
  to every call.
- **Written in Rust** - a single self-contained binary.
- **No database** - sessions are plain JSONL files on disk.
- **No subagents** (yet)

## Requirements

- For the web tools: a headless Chrome-compatible browser (Google Chrome,
  Chromium, Brave, or Microsoft Edge). rope looks for `google-chrome`,
  `google-chrome-stable`, `chromium`, `chromium-browser`,
  `brave-browser`, and `microsoft-edge` on `PATH`, and also checks the
  standard install locations on macOS and Windows. If no browser
  is found, the web tools are unavailable, but the rest of the harness
  works normally.
- [Ripgrep](https://ripgrep.org/) is recommended. If missing, relevant tools use fallback utilities.
- For the TUI, an interactive terminal (a real TTY) supporting raw mode,
  the alternate screen, mouse reporting, and bracketed paste.
  `--headless` works without a terminal, including with redirected input/output.
- A Rust toolchain to build from source.

## Features

- Provider-independent streaming runtime for OpenAI-compatible endpoints
  (Responses API by default, `chat_completions` fallback) with named
  model profiles: context size, temperature, reasoning effort, and
  vision capability.
- Embedded images rendered inline when the terminal supports Sixel, the Kitty
  graphics protocol, or the iTerm2 image protocol (auto-detected at runtime;
  works on kitty, Ghostty, WezTerm, iTerm2, foot, Xterm, mlterm, Rio, and
  others), with a text placeholder otherwise
- Persistent sessions as JSONL under `~/.local/share/harness/sessions`,
  with auto-generated titles, token totals, and resume via
  `rope --session NAME`, `/new`, or the `/session` picker.
- Iterative model → tool → model execution: the tool calls one reply
  batches run at the same time and their results reach the model in call
  order, with immediate cancellation that preserves completed work, and
  automatic retry with backoff on transient failures.
- Steering: send a message while a turn is in progress and it is injected
  into the conversation at the next model request, shown as a `Steer` message.
- Per-tool `allow` / `ask` / `deny` approval policies in `config.toml`,
  with session-persisted decisions.
- Built-in tools: `read`, `write`, `edit` (with per-call diffs),
  `shell`, `shell_poll`, `shell_cancel`, `search_files`, `list_files`,
  and `org_outline`. Long-running shell commands stream output and can be
  polled or stopped by the model.
- File delivery: `send_file` publishes local files in chat, with inline
  images and downloads in the web UI and clickable file paths in the TUI.
- Web tools: `web_search` (DuckDuckGo with Bing fallback) and
  `web_browser` (JavaScript-rendered, visible page content) over a shared
  headless browser session.
- External tools: any executable that reads JSON on stdin and returns
  `{"output": "..."}` on stdout, discovered from `./.rope/tools/` and
  `~/.config/rope/tools/`.
- MCP tools: stdio or Streamable HTTP servers configured under
  `[mcp.servers.NAME]`, with include/exclude filters, environment-backed bearer
  tokens and headers, call timeouts, and server- or tool-level approval
  policies. Server failures are reported as session notices without hiding
  Rope's built-in tools. Tool-list change notifications update the next model
  request without restarting the session; text, structured data, resource
  links, embedded text, and images are retained in tool results.
- Context management: live context-fill tracking, bounded tool output,
  automatic compaction throughout long turns with visible summary markers,
  manual `/compact`, and global/project `AGENTS.md` instructions.
- Raw request inspector: open `[raw]` in either UI to search and expand the
  recorded provider JSON, including instructions, messages, tools, and
  model settings. Requests and total turn times survive session reloads.
- Terminal UI: CommonMark rendering with syntax-colored code, image
  attachments (Sixel / iTerm2 / Kitty), live git status pane with a
  full-screen diff view (when the directory is a git worktree), plan pane,
  chat search, model picker, and prompt history.

## Disclaimer
Rope is a simple harness meant for power users and developers. It has a very basic permission system and no sandbox. Models can leak your data via tools or perform undesired actions on live system. Use it at your own risk.
