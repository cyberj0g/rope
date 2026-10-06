# Agents support implementation plan

Add file-configured agents to Rope, with selectable primary agents and observable, steerable subagents in both the TUI and web UI. Keep one runtime and one chat implementation: a subagent is a normal Rope session linked to the tool call that created it.

Reference: [OpenCode agents](https://opencode.ai/docs/agents/) uses global/project agent files, primary/subagent/all modes, model defaults, tool permissions, and navigable child sessions. Borrow those concepts, using Rope's existing TOML configuration and session machinery. This is a Rope design, not an OpenCode configuration compatibility layer.

## 1. Configuration and defaults

Add `src/agent.rs` for loading and resolving agent definitions. Discover `~/.config/rope/agents/*.md` and `<project>/.rope/agents/*.md`. Use TOML front matter delimited by `+++`, parsed with the existing TOML dependency; the remaining Markdown is the agent's additional instructions. The filename stem is its stable ID. A project file replaces the global definition with the same ID, without merging prompt bodies or individual fields. Load at core startup; file watching and live reload are out of scope.

Example `.rope/agents/review.md`:

```markdown
+++
description = "Review changes for bugs and missing tests"
mode = "all"
model = "qwen"
can_call_subagents = false

[tools]
write = "deny"
edit = "deny"
shell = "ask"
+++

Review the changes and report actionable findings with file references.
```

Fields:

| Field | Meaning / default |
| --- | --- |
| `description` | Required, nonempty description used in pickers and delegation discovery |
| `mode` | `primary`, `subagent`, or `all`; default `all`. Controls selection versus delegation eligibility |
| `model` | Optional existing Rope model profile name, including its provider routing |
| `can_call_subagents` | Default `false` for custom agents; separate from whether the agent itself is callable as a subagent |
| `[tools]` | Optional `allow` / `ask` / `deny` overrides over configured tool policies |

Validate unknown fields, invalid modes/policies, and unknown model profiles with errors naming the file and field. Keep the format limited to these fields and the prompt body.

Built-in `assistant`, displayed as **Assistant**, is always selectable, has mode `primary`, and allows delegation. Reserve its ID rather than requiring an `assistant.md` file. It uses Rope's existing prompt assembly, global/project `AGENTS.md`, default model, and `[tools]` configuration. Do not add a separate Assistant persona prompt. Add `subagent = "allow"` to the default tool configuration; users may set it to `ask` or `deny`.

All custom agents receive the same project/runtime instructions plus their Markdown body. Primary-agent model selection uses the definition's model or the configured default. A new child uses its definition's model or the caller's current model. A user's subsequent `/model` selection remains active until another agent is selected. Switching agents applies the new default model and its supported reasoning defaults atomically; reopening a session restores its saved agent/model/reasoning settings. Switching back does not need a per-agent settings history.

## 2. Per-agent tool policy

Reuse `Approval` and existing built-in/external/MCP discovery. Resolve policies once per selected agent, and use the same resolution for advertised schemas and execution. Extend policy lookup to accept exact registered external/MCP tool names as well as existing category defaults; reuse existing MCP server/tool rules. Precedence is agent exact tool, agent category, existing configured tool/server/category policy. No new wildcard policy language is needed.

Omitted agent entries inherit configured policies. An explicit agent entry overrides them. Child policy is resolved from the child's definition and global/project configuration, not from the parent's session approvals. Delegation intentionally permits running a differently configured agent; describe that behavior in the documentation.

Denied tools must be absent from model schemas and rejected if a model nevertheless calls them. Recheck effective policy before honoring stored approvals. Scope session approvals by agent ID and existing approval key so selecting another agent cannot inherit an earlier agent's approvals. Preserve ownership restrictions and the existing safe shell polling/cancellation behavior.

Expose `subagent` only when `can_call_subagents` is true, its effective policy is not `deny`, and at least one eligible agent exists. List callable IDs and descriptions in its tool description/schema, filtered by mode. Do not duplicate a large catalog in every prompt. Enforcement must also reject direct calls that bypass discovery. Agent mode controls selection/delegation, not whether users can view and steer an existing child.

## 3. Child sessions and the `subagent` tool

Use one blocking tool invocation:

```json
{"agent":"review","prompt":"Review the current diff for correctness bugs."}
```

The tool creates a child session, submits the task, and waits for its turn to settle while both session actors continue accepting commands. Return a bounded structured result with `session_id`, `agent`, `status`, and final response or error. Status values are `completed`, `user_cancelled`, `failed`, and `interrupted`. For cancellation, include the human-readable message `user cancelled`. Preserve these fields when applying tool output limits; truncate only response text. Partial work remains in the child transcript.

A session waits on at most one direct child at a time, even when its assistant message batches several `subagent` calls, so top-level steering has an unambiguous destination. Separate root sessions can still run concurrently. Support nested delegation only when the child's configuration allows it; use a small explicit maximum depth, e.g. four child levels, to stop recursive delegation loops. Do not introduce background job APIs, parallel sibling scheduling of child sessions, worktrees, agent factories, or a new orchestration framework. (Tool calls that are not delegations do run concurrently within one assistant message; that is scheduling of tool work, not of child sessions.)

Each child has its own conversation, compaction, plan, usage, approvals, shell jobs, browser state, MCP/tool resources, and raw requests. It shares the project directory. Its initial context is the task prompt plus its normal instructions; do not silently copy the parent's full transcript. Parent tools/results convey the relevant task context and final answer.

Persist the child's parent session ID, parent turn ID, tool call ID, and original agent ID. Persist the reciprocal child reference on the parent call before child work starts. Use `(parent session, parent turn, tool call)` as the invocation identity to prevent duplicate creation. The child reference and status must be structured metadata, never inferred from displayed tool output or lost when output is truncated.

Keep child sessions out of the default root-session list, but expose them through their parent and allow reopening by ID. Preserve links through compaction, restart, and history reconstruction. Never count child usage as parent model usage; display child usage in the child chat and optionally its call card without double counting. Reject deletion of sessions with active linked work. Use the existing delete confirmation for deleting an idle session subtree, making the scope explicit.

## 4. Steering and cancellation contract

Implement routing on the server/core, not independently in clients. The active delegation relationship is turn-scoped. A compact receipt records origin, recipient session/turn, and a message ID, so the UI can show where input went and retries cannot duplicate it.

| User action | Required behavior |
| --- | --- |
| Send while viewing a running child | Deliver a steer to that child; if it is itself waiting on a child, follow the active chain to the working descendant |
| Send from a parent while its subagent is working | Forward immediately to the active working descendant, including while it runs a tool or awaits approval; do not leave the message waiting only in the parent's queue |
| Send when there is no active delegation | Preserve Rope's normal send/steer behavior |
| Esc in a running child's chat | Cancel that viewed child's turn and its active descendants; its caller receives one `user_cancelled` result and can continue |
| Esc in the root chat | Cancel the root turn and all descendants owned by that turn |
| Navigate back, switch sessions, or disconnect | Leave work running |

Show the parent's composer destination, e.g. `Steer review`, while forwarding applies. Show a forwarded-steer receipt in the origin transcript and the actual steer in the destination transcript, including its origin. The parent should also see a concise record of user steering when it resumes, so it does not undo the correction. The steer must enter the child's normal queue exactly once and reach its next model request; do not abort a model stream solely to inject it. Preserve attachments by resolving them in the source session and storing valid destination references; validate destination model capabilities before accepting input.

Serialize routing acceptance against child completion/cancellation. If the child has already settled before acceptance, re-resolve the active route and use the originating session's normal send behavior. If the steer was accepted first, do not report child completion to the parent until the queued input has been handled, including Rope's existing follow-up-turn behavior when it misses the last model request. Never silently lose an accepted steer or restart a cancelled child. Preserve accepted but undelivered steers with the cancellation record.

Esc targets the viewed session and its captured turn ID, never a stale parent's ID. While that session is running, cancellation takes precedence over dismissing a picker/search/approval overlay. When idle, retain ordinary Esc behavior. Provide a separate breadcrumb/back action for navigation. The web Cancel control has the same semantics as Esc.

Cancellation must stop descendant tools/process trees and approval waits before reporting the terminal result. Child cancellation resumes the waiting caller with a tool result; it does not cancel the caller. Parent cancellation must invalidate its delegation before cleanup so late child events cannot resume it or route new messages into it. Terminal completion, cancellation, and failure are first-wins and settle each waiting call exactly once.

After completion, the child remains a fully usable chat. Further messages can start independent turns there; these do not rewrite or redeliver the original parent's tool result. Graceful shutdown cancels linked work. On restart, reconcile unfinished invocation records as `interrupted` and close unmatched tool calls; do not silently restart tasks or leave permanent running badges. This needs invocation-specific persistence, not a general event-sourcing rewrite.

## 5. Shared runtime and protocol changes

Build on these existing boundaries:

| Files | Changes |
| --- | --- |
| `src/agent.rs`, `src/config.rs`, `src/project.rs` | Agent loading/validation, effective model/policy resolution, additional prompt body |
| `src/session.rs`, `src/runtime/message.rs` | Backward-compatible agent settings, historical agent attribution, parent/child invocation metadata and durable terminal state |
| `src/core/mod.rs` | Agent catalog, linked session creation, active delegation routing, completion/cancellation coordination |
| `src/runtime/actor.rs`, `src/runtime/mod.rs` | Revision-checked agent selection, per-turn agent config, delegation wait, steering handoff and terminal results |
| `src/tool/mod.rs` | Shared effective-policy filtering/enforcement and `subagent` schema |
| `src/protocol.rs`, `src/core/state.rs`, `src/server.rs` | Agent catalog, `SetAgent`, agent-attributed blocks, structured child call state, steering destination/receipts |

Keep coordination small: a typed internal delegation request/reply channel between runtime and core is sufficient. The runtime supplies session/turn/call identity; do not force generic tools to guess it. Core creates the child and routes commands; the parent's actor remains responsive while its generation waits. Never hold the core session-map/loading mutex across child completion, and avoid an owning `Core -> session -> Core` reference cycle.

Add `SetAgent { agent, revision }`, with existing settings revision semantics. Selection requires the viewed session to be idle, just as model changes do; show a clear busy explanation in both clients. In a child, selection may include its current subagent-only definition plus normally selectable primary/all definitions. Preserve the transcript and historical response labels across switching. Rebuild prompts, policies, and schemas for the next turn without disturbing session-owned resources.

Snapshots and replay must contain selected agent, per-response agent attribution, parent breadcrumb, linked child sessions, current delegation/steer destination, and child activity. Show approval-required state on the parent call card with an action to open the child; the child uses normal approval controls. Keep snapshot recovery, lazy block revelation, deduplicated commands, and first-wins approval semantics. Return only public agent metadata to clients, not prompt files or credentials.

Update protocol versions and both clients together if required. The current code uses protocol version 2 while `CLIENT_SERVER.md` still says version 1; update that document to the actual implemented protocol as part of this change.

Old sessions default to Assistant, preserving saved model/reasoning. If a saved custom agent disappears, allow transcript inspection but block new turns with a clear notice until the user selects an available agent; do not silently substitute broader permissions. Old approvals require an explicit migration to Assistant-scoped keys.

## 6. TUI and web UX

Reuse normal session subscriptions, chat renderers, and composers for child views. Do not build a read-only subagent log viewer.

- Display the selected agent beside the model in the chat status/header. Clicking it opens a searchable agent picker with descriptions and model defaults. Label assistant response headers with the agent that produced them, including older turns after a switch.
- Add `/agent` to the TUI command palette and command handling; `/agent` opens the picker and `/agent NAME` selects a primary/all agent. Add the same command and clickable picker to the web UI.
- Render a subagent invocation as a tool card with agent name, task preview, model, elapsed time, live state, and an `Open chat` action. Distinguish running, awaiting approval, completed, user cancelled, failed, and interrupted. Keep raw arguments/result inspection available.
- TUI: support mouse activation and keyboard focus/Enter to open the child. Add a visible parent breadcrumb/back action. Preserve each session's draft and scroll position when navigating.
- Web: use the same live chat for child routes, with breadcrumbs, mobile-friendly open/back controls, agent picker, and normal Send/Steer/Cancel behavior. Restore the selected child after reload using existing navigation state or a session route.
- Child chat must retain thinking/tool expansion, streaming output, approvals, attachments, images/files/diffs, plans, search, model/reasoning selection while idle, compaction, usage, and raw request inspection. Scope subscriptions, uploads, drafts, and cancel IDs to the correct session.
- Keep parent call cards live when a different client is viewing the child. Child state changes must reach the parent projection even when nobody subscribes directly to the child.

Likely client files: `src/ui/{mod,state,client,history}.rs`, `web/js/{app,state,protocol,chat,composer,status,panels}.js`, `web/index.html`, and `web/styles.css`. Extend existing picker and navigation patterns without introducing a frontend framework.

## 7. Implementation order and verification

1. Add agent parsing, defaults, policy resolution, and configuration examples. Test global/project replacement, malformed files, model lookup, all modes, Assistant compatibility, and delegation eligibility.
2. Add persisted identity/settings, agent selection, protocol catalog, and historical labels. Test legacy sessions, stale revisions, busy rejection, model defaults/overrides, removed agents, and approval isolation.
3. Implement linked child sessions and the tool with deterministic mock-provider tests. Cover final answers, failures, denied calls, nested delegation/depth limits, and denied schemas plus execution. Exercise built-in, external, and MCP policy resolution, including newly discovered tools.
4. Implement steering/cancellation coordination before UI polish. Use controlled provider/tool barriers rather than sleep-based races. Test steering from parent and child during streaming, tool execution, approval waits, and nested delegation; attachments; completion-versus-steer; cancellation-versus-steer; duplicate requests; parent versus child cancellation; descendant cleanup; and no duplicate tool results or orphan processes.
5. Add both client experiences. Extend TUI state/input tests and `tests/web/e2e.mjs`. Verify click and `/agent`, open/back navigation, parent card updates, real child controls, Esc from the focused composer and overlays, mobile layout, multiple viewers, reload/reconnect, and restored drafts. Inspect screenshots for both root and child chats.
6. Test persistence and restart in `tests/core.rs` and `tests/server.rs`: linked transcripts, agent attribution, compaction, partial/cancelled work, lazy reveal, snapshot recovery, interrupted calls, root catalog filtering, and subtree deletion rules.
7. Update `README.md`, `config.example.toml`, `CLIENT_SERVER.md`, and `FEATURES.md` to describe the implemented behavior. Run `cargo fmt --check`, `cargo test --locked`, and the existing browser E2E runner with a supported browser/runtime. Report actual results and any unavailable checks.

Acceptance walkthrough: configure a selectable `review` agent and a subagent-only helper; switch between Assistant and review by click and `/agent`; delegate from Assistant; watch the live call card; open the complete child chat; steer from both views while it works; press Esc in the child and observe `user cancelled` in the parent followed by normal parent continuation. Repeat in the web UI, including a reconnect while the child is running. Verify a denied tool cannot execute and an agent with delegation disabled cannot create children.
