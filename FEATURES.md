# Rope features

## Runtime and providers

- provider-independent streaming runtime with an OpenAI-compatible provider
- deterministic mock provider for runtime tests
- streamed text and OpenAI function-call assembly

## Sessions

- automatic sessions under `~/.local/share/harness/sessions`
- JSONL conversation persistence after completed turns
- persisted session token totals and configurable per-token cost estimates
- `--continue [NAME]`, `--session NAME`, `/new [NAME]`, and `/save`

## Tools

- iterative model → tool → model execution
- built-in `read`, `write`, `edit`, `shell`, `grep`, and `glob` tools
- per-tool `allow`, `ask`, and `deny` policies in `config.toml`
- executable JSON tools discovered from `.harness/tools/` and `~/.config/harness/tools/`
- local external tools override global tools with the same filename

External tools receive their function arguments as JSON on stdin and must return
`{"output":"..."}` on stdout.

## Coder UI

- global persistent prompt history with Bash-style Up/Down navigation and Shift+Enter newlines
- CommonMark/GFM rendering with inline emphasis, links, aligned tables, and syntax-colored fenced code blocks
- streamed tool-call arguments, results, and approval prompts
- live tool counters that switch from characters to lines after the first literal or escaped line break
- streamed and persisted reasoning blocks (`reasoning` and legacy `reasoning_content`)
- collapsible messages plus collapsed-by-default thinking and tool sections
- live elapsed time on thinking and tool calls with compact duration units
- one-space conversation content padding with flush section headers
- color-coded connecting, generating, idle, and error status with compact chat errors
- separately colored model and reasoning details on the padded input box; session tokens and cost on the status bar
- generating model recorded beside each assistant response
- full-width conversation view with deliberate trailing whitespace
- current directory and compact session identity on the status bar
- `/add PATH`, `/drop PATH`, `/context`, and `/diff`
