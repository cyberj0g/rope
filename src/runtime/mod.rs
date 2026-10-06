mod actor;
mod message;
#[cfg(test)]
use actor::run;
pub use actor::spawn_session;

use std::{
    collections::HashSet,
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Error, Result, bail};
use futures_util::{StreamExt, future::join_all};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::{
    agent::{ASSISTANT_ID, Agent, AgentCatalog},
    config::Config,
    project::ProjectState,
    provider::{Provider, ResponseDelta, Usage},
    session::{Session, SessionMeta},
    tool::{
        Approval, DelegationCommand, DelegationRequest, ExecutionPlan, PlanStatus, ToolDefinition,
        ToolRegistry, ToolResult,
    },
};
pub use message::{
    FileContent, ImageContent, MAX_FILE_BYTES, Message, SubagentOutcome, SubagentStatus, ToolCall,
    bounded_subagent_json, guess_mime_type,
};

pub const CANCELLED_BY_USER: &str = "cancelled by user";
/// Tool result recorded for a call still in flight when the user stopped
/// the turn, so the interrupted history stays a valid message sequence
/// and the model knows the call never completed.
const CANCELLED_TOOL_OUTPUT: &str = "Cancelled by user before the tool completed.";
/// Prefix of the persisted System marker for a compacted context. The
/// remainder of the marker content is the summary the context was reduced to.
pub const COMPACTION_MARKER: &str = "Context compacted";
const TOOL_OUTPUT_TRUNCATED: &str = "\n[tool output truncated]";
/// Floor for the per-call tool output budget, in tokens (4 bytes each).
/// Even near the context limit, a tool result always carries its control
/// fields — e.g. a shell job's status and job_id — so the model can keep
/// polling or cancelling instead of losing track of the command. The
/// budget is measured after reserving the Tool message's own framing
/// (role, call id) and per-message overhead, and when even the floor no
/// longer fits the conversation is compacted mid-turn before the tool
/// runs, so the result can never push the next model request past
/// max_context_tokens.
const MIN_TOOL_OUTPUT_TOKENS: u64 = 32; // 128 bytes
/// Floor for the compaction request's own output: below it the summary
/// would be too short to continue the conversation from, and the request
/// is trimmed or refused instead of risking a one-line summary.
const SUMMARY_MIN_OUTPUT_TOKENS: u64 = 128;
/// Hard cap on the tool calls one assistant message may batch. The cap
/// resets for every assistant message, so a turn may run as many model
/// turns as needed; only a single message is bounded.
const MAX_TOOL_CALLS_PER_MESSAGE: usize = 64;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl FromStr for ReasoningEffort {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "none" => Ok(Self::None),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::XHigh),
            "max" => Ok(Self::Max),
            _ => bail!("reasoning effort must be none, minimal, low, medium, high, xhigh, or max"),
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        })
    }
}

#[derive(Clone, Debug)]
pub struct CompletionRequest {
    pub provider: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub tools: Vec<ToolDefinition>,
}

#[derive(Clone, Debug)]
pub struct UserPrompt {
    pub content: String,
    pub images: Vec<ImageContent>,
}

impl UserPrompt {
    /// The user message this prompt starts when no turn is running.
    pub fn user_message(self, plan: Option<&ExecutionPlan>) -> Message {
        Message::user_with_images(with_runtime_context(self.content, plan), self.images)
    }

    /// The Steer message this prompt persists as in conversation history:
    /// the raw prompt, with no runtime context pinned.
    pub fn steer_message(self) -> Message {
        Message::steer(self.content, self.images)
    }
}

/// The model-facing boundary of the runtime context pinned to a user
/// message. A namespaced tag pair keeps the boundary parseable for the
/// chat renderers (which hide the block) and for `strip_runtime_context`.
const RUNTIME_CONTEXT_BEGIN: &str = "<runtime-context>";
const RUNTIME_CONTEXT_END: &str = "</runtime-context>";
/// The pre-tag boundary, kept so messages from older sessions still strip.
const LEGACY_RUNTIME_CONTEXT: &str = "Runtime context:";

/// The context a user message pins at send time: the current time and,
/// while any plan step is still open, the session plan. It is appended to
/// the message once, when the prompt is sent, and never updated afterwards
/// — every message keeps the context it went out with, like the rest of
/// history. The block is invisible in the chat; only the model sees it.
fn with_runtime_context(content: String, plan: Option<&ExecutionPlan>) -> String {
    let mut content = content;
    if !content.is_empty() {
        content.push_str("\n\n");
    }
    content.push_str(&runtime_context(plan));
    content
}

fn runtime_context(plan: Option<&ExecutionPlan>) -> String {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M");
    let mut context = format!("{RUNTIME_CONTEXT_BEGIN}\n- current time: {now}");
    // The plan rides along only while work is open: every update_plan call
    // stays in context un-compacted, so a fully completed plan adds nothing
    // the model does not already have.
    if let Some(plan) = plan
        && plan
            .plan
            .iter()
            .any(|step| step.status != PlanStatus::Completed)
    {
        context.push_str(&format!(
            "\n- current plan:\n{}",
            serde_json::to_string_pretty(plan).unwrap()
        ));
    }
    context.push_str(&format!("\n{RUNTIME_CONTEXT_END}"));
    context
}

/// The part of a user message before its pinned runtime context, if any.
pub fn strip_runtime_context(content: &str) -> &str {
    let mut cut = content.len();
    for marker in [RUNTIME_CONTEXT_BEGIN, LEGACY_RUNTIME_CONTEXT] {
        if content.starts_with(marker) {
            return "";
        }
        if let Some(index) = content.find(&format!("\n\n{marker}")) {
            cut = cut.min(index);
        }
    }
    &content[..cut]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

pub enum Command {
    Request {
        action: crate::protocol::Action,
        images: Vec<ImageContent>,
        reply: oneshot::Sender<crate::protocol::Result<crate::protocol::Accepted>>,
        published: oneshot::Sender<()>,
    },
    SetReasoning(Option<ReasoningEffort>),
    Submit(UserPrompt),
    /// A prompt sent while a turn is in progress. Queued for injection at
    /// the turn's next model request; if the turn already finished it
    /// starts a fresh turn instead.
    Steer(UserPrompt),
    Cancel,
    Approve(ApprovalDecision),
    SelectModel(String),
    SetAgent(String),
    /// Persists the parent side of one delegation record before the
    /// child's work starts: `tool_call_id -> child` plus the child in
    /// `children`. Idempotent per tool call.
    RecordDelegation {
        turn_id: String,
        tool_call_id: String,
        child: String,
        agent: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Flush earlier actor events through the core's projection.
    Observe(oneshot::Sender<()>),
    /// Manually compacts the idle conversation into a continuation summary.
    Compact,
    Shutdown(oneshot::Sender<SessionSummary>),
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionSummary {
    pub error: Option<String>,
    pub name: String,
    pub total_tokens: u64,
    pub total_cost: Option<f64>,
}

#[derive(Clone, Debug)]
pub enum Event {
    Barrier(Arc<Mutex<Option<oneshot::Sender<()>>>>),
    Update(Arc<crate::core::Update>),
    PromptRejected(UserPrompt, String),
    RefreshProject,
    Ready,
    Snapshot(Box<crate::core::state::Snapshot>),
    Catalog(Vec<crate::session::SessionInfo>),
    Notice(String),
    Diff {
        path: Option<std::path::PathBuf>,
        content: String,
    },
    MessageAccepted(Message),
    SteersDelivered(usize),
    OperationStarted {
        id: String,
        compacting: bool,
    },
    SettingsRevision(u64),
    ApprovalResolved {
        approval_id: String,
        tool: String,
        decision: ApprovalDecision,
    },
    ToolImage {
        call_id: String,
        image: ImageContent,
    },
    ToolFile {
        call_id: String,
        file: FileContent,
    },
    History(Vec<Message>),
    SessionChanged(String),
    UsageChanged {
        total_tokens: u64,
        total_cost: Option<f64>,
    },
    ContextChanged {
        tokens: u64,
        max_tokens: u64,
    },
    SettingsChanged {
        model: String,
        reasoning_effort: Option<ReasoningEffort>,
        /// The selected agent ID; `None` is the built-in assistant.
        agent: Option<String>,
    },
    /// The session's active delegation (the child a `subagent` call is
    /// waiting on) and its linked children with terminal states. Applied
    /// by the core directly to the parent's projection.
    DelegationChanged {
        child: Option<String>,
        children: Vec<crate::core::state::ChildLink>,
    },
    /// A steer the user sent from this session was forwarded to a
    /// descendant; the receipt keeps the origin transcript honest.
    SteerReceipt {
        to: String,
        agent: String,
    },
    ProjectChanged(ProjectState),
    PlanChanged(Option<ExecutionPlan>),
    GenerationStarted,
    ModelRequestStarted(String),
    RawRequest(Option<String>),
    RawData {
        session_id: String,
        block_id: String,
        result: Result<serde_json::Value, String>,
    },
    ResponseHeadersReceived,
    ResponseStarted,
    ModelResponseFinished {
        output_tokens: u64,
        duration: Duration,
    },
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        name: Option<String>,
        arguments: String,
    },
    ToolCallFinished {
        index: usize,
        call: ToolCall,
    },
    ToolStarted {
        call_id: String,
    },
    ApprovalRequested {
        approval_id: String,
        call: ToolCall,
    },
    ToolOutputDelta {
        call_id: String,
        delta: String,
    },
    ToolResult {
        call_id: String,
        output: String,
        success: bool,
        diff: Option<String>,
    },
    Retrying {
        seconds: u64,
    },
    /// A response stream broke before finishing: the partial reply is
    /// dropped from the transcript before the request starts over, so the
    /// chat never shows text the model's context will not carry.
    ResponseDiscarded,
    CompactionStarted,
    ContextCompacted {
        summary: String,
    },
    GenerationFinished {
        /// Total wall-clock time of the finished turn, from the user's
        /// prompt to the final response; `None` for operations like a
        /// manual compaction that have no user-facing answer.
        duration: Option<Duration>,
    },
    GenerationCancelled,
    Error(String),
}

enum InternalEvent {
    Scoped {
        id: String,
        event: Box<InternalEvent>,
    },
    Visible(Event),
    Compacted(Result<(String, Option<String>), String>),
    Finished(TurnResult),
    Failed(String),
    Usage(Usage),
    AuxiliaryUsage(Usage),
    PlanUpdated(ExecutionPlan),
    Approval {
        call: ToolCall,
        approval_key: String,
        reply: oneshot::Sender<ApprovalDecision>,
    },
    ProjectRefresh,
}

struct TurnResult {
    completed: Vec<Message>,
    compaction: Option<Compaction>,
    title: Option<String>,
}

/// The in-flight state of an active turn, shared with the runtime so an
/// interrupted turn's completed work can be persisted instead of lost:
/// without it, the aborted agent task would take its partial conversation
/// with it and the next turn would start as if nothing had happened.
#[derive(Clone, Default)]
struct TurnProgress {
    raw_request: Option<String>,
    /// the full turn transcript, including work removed from model context
    messages: Vec<Message>,
    /// Compaction the turn applied (turn-start or mid-turn), if any.
    compaction: Option<Compaction>,
}

type TurnProgressHandle = Arc<Mutex<TurnProgress>>;

/// One active generation: the agent task and the progress it publishes.
struct ActiveTurn {
    id: String,
    task: JoinHandle<()>,
    progress: TurnProgressHandle,
    /// When the turn's user prompt was submitted, so the finished turn can
    /// report how long it took end to end.
    started: Instant,
}

#[derive(Clone)]
struct Compaction {
    raw_request: Option<String>,
    summary: String,
    through: usize,
}

struct PendingApproval {
    id: String,
    tool: String,
    approval_key: String,
    reply: oneshot::Sender<ApprovalDecision>,
}

/// Steering messages queued for the active turn. The message is built
/// when the user sends the prompt — the raw steer text, with no runtime
/// context pinned — so what the client displays is exactly what reaches
/// the model. Drained by the turn's agent at each model request and, if
/// the turn ends first, by the runtime, which resubmits the leftovers as
/// a fresh turn.
type SteerQueue = Arc<Mutex<Vec<Message>>>;

/// Persists the work an interrupted turn already completed, so the
/// conversation continues from the real state instead of from before the
/// interruption: the turn's user prompt, every assistant message and
/// tool result the model saw, any compaction the turn applied, the
/// steers queued but never delivered, and a marker that the user stopped
/// the turn.
async fn persist_interrupted_turn(
    messages: &mut Vec<Message>,
    session: &mut Session,
    progress: TurnProgressHandle,
    pending_prompts: &SteerQueue,
    marker: &str,
) -> Result<()> {
    // The turn's user prompt: spawn_turn pushed it when the turn began,
    // and the runtime appends nothing else while a turn is active.
    let turn_from = messages.len().saturating_sub(1);
    let TurnProgress {
        messages: mut tail,
        compaction,
        raw_request,
    } = progress.lock().unwrap().clone();
    let compacted = compaction.is_some();
    let tool_error = if marker == CANCELLED_BY_USER {
        CANCELLED_TOOL_OUTPUT.to_owned()
    } else {
        format!("Error: {marker}")
    };
    close_open_tool_calls(&mut tail, &tool_error);
    messages.truncate(turn_from);
    if let Some(compaction) = compaction {
        let marker = Message::system(format!("{COMPACTION_MARKER}\n{}", compaction.summary))
            .with_raw_request(compaction.raw_request);
        session.meta.compaction_summary = Some(compaction.summary);
        // the marker precedes the turn, shifting boundaries inside it
        session.meta.compacted_through =
            compaction.through + usize::from(compaction.through > turn_from);
        messages.push(marker);
    }
    messages.extend(tail);
    // Steers queued for the turn never reached the model; persist them
    // with the turn so history keeps what the user said.
    messages.extend(pending_prompts.lock().unwrap().drain(..));
    messages.push(Message::system(marker.into()).with_raw_request(raw_request));
    if compacted {
        session.meta.context_tokens = estimate_tokens(&request_context(messages, &session.meta));
    }
    session.append(&messages[turn_from..]).await?;
    session.save().await
}

/// Closes the tool calls an interrupted turn left hanging: the agent may
/// have been stopped between an assistant message and the results of its
/// calls, and providers refuse a tool call without its result. Each
/// unanswered call receives a cancellation result, in call order.
fn close_open_tool_calls(tail: &mut Vec<Message>, output: &str) {
    let Some(start) = tail
        .iter()
        .rposition(|message| matches!(message, Message::Assistant { .. }))
    else {
        return;
    };
    let Message::Assistant { tool_calls, .. } = &tail[start] else {
        return;
    };
    // The calls of one message run at the same time, so the results already
    // in the transcript are the ones whose calls finished — not a prefix of
    // them. What is left is every call id none of them answered.
    let answered = tail[start + 1..]
        .iter()
        .filter_map(|message| match message {
            Message::Tool { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<HashSet<_>>();
    let open = tool_calls
        .iter()
        .filter(|call| !answered.contains(call.id.as_str()))
        .map(|call| call.id.clone())
        .collect::<Vec<_>>();
    for call_id in open {
        tail.push(Message::tool(call_id, output.into(), None, None));
    }
}

#[allow(clippy::too_many_arguments)]
async fn spawn_turn<P: Provider + ?Sized>(
    generation: &mut Option<ActiveTurn>,
    pending_prompts: &mut SteerQueue,
    messages: &mut Vec<Message>,
    session: &mut Session,
    project: &ProjectState,
    provider: &Arc<P>,
    tools: &ToolRegistry,
    config: &Config,
    first: Message,
    agent: &Agent,
    session_name: String,
    delegation: &crate::tool::DelegationPort,
    agents: Arc<AgentCatalog>,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) {
    let persist_from = messages.len();
    let progress: TurnProgressHandle = Arc::new(Mutex::new(TurnProgress {
        messages: vec![first.clone()],
        compaction: None,
        raw_request: None,
    }));
    messages.push(first);
    let request_messages = request_context(messages, &session.meta);
    let context_tokens = session.meta.context_tokens;
    let generate_title = session.needs_title();
    // The agent's own instructions extend the project prompt; both travel
    // as the single system message at the head of every request.
    let project_prompt = async {
        let prompt = project.prompt().await?;
        Ok(prompt
            .map(|prompt| {
                if agent.body.is_empty() {
                    prompt
                } else {
                    format!("{prompt}\n\n{}", agent.body)
                }
            })
            .or_else(|| (!agent.body.is_empty()).then(|| agent.body.clone())))
    }
    .await;
    *pending_prompts = Arc::new(Mutex::new(Vec::new()));
    let steers = pending_prompts.clone();
    let provider = provider.clone();
    let tools = tools.clone();
    let config = config.clone();
    let agent = agent.clone();
    let delegation = delegation.clone();
    let id = uuid::Uuid::new_v4().to_string();
    events
        .send(Event::OperationStarted {
            id: id.clone(),
            compacting: false,
        })
        .await
        .ok();
    events.send(Event::GenerationStarted).await.ok();
    let parent = internal.clone();
    let (events, internal, forward) = worker_channels(&id, internal);
    *generation = Some(ActiveTurn {
        id: id.clone(),
        progress: progress.clone(),
        started: Instant::now(),
        task: tokio::spawn(async move {
            let result = match project_prompt {
                Ok(prompt) => {
                    turn(
                        provider,
                        &tools,
                        &config,
                        request_messages,
                        persist_from,
                        context_tokens,
                        prompt,
                        generate_title,
                        steers,
                        &progress,
                        &events,
                        &internal,
                        &agent,
                        &session_name,
                        &id,
                        &delegation,
                        &agents,
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let event = match result {
                Ok(result) => InternalEvent::Finished(result),
                Err(error) => InternalEvent::Failed(format!("{error:#}")),
            };
            drop(events);
            drop(internal);
            forward.await.ok();
            parent
                .send(InternalEvent::Scoped {
                    id,
                    event: Box::new(event),
                })
                .await
                .ok();
        }),
    });
}

fn worker_channels(
    id: &str,
    parent: &mpsc::Sender<InternalEvent>,
) -> (
    mpsc::Sender<Event>,
    mpsc::Sender<InternalEvent>,
    JoinHandle<()>,
) {
    let (events, mut event_rx) = mpsc::channel(64);
    let (internal, mut internal_rx) = mpsc::channel(8);
    let id = id.to_owned();
    let parent = parent.clone();
    let forward = tokio::spawn(async move {
        loop {
            // visible events preceding an internal effect must be published first
            let event = tokio::select! {
                biased;
                Some(event) = event_rx.recv() => InternalEvent::Visible(event),
                Some(event) = internal_rx.recv() => event,
                else => break,
            };
            if parent
                .send(InternalEvent::Scoped {
                    id: id.clone(),
                    event: Box::new(event),
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });
    (events, internal, forward)
}

async fn send_settings(events: &mpsc::Sender<Event>, config: &Config, agent: &Agent) {
    events
        .send(Event::SettingsChanged {
            model: config.model_name().to_owned(),
            reasoning_effort: config.effective_reasoning_effort(),
            // `None` is the built-in assistant, matching how the session
            // persists its selection.
            agent: (agent.id != ASSISTANT_ID).then(|| agent.id.clone()),
        })
        .await
        .ok();
}

async fn send_usage(events: &mpsc::Sender<Event>, session: &Session) {
    events
        .send(Event::UsageChanged {
            total_tokens: session.meta.total_tokens,
            total_cost: session.total_cost(),
        })
        .await
        .ok();
}

async fn send_context(events: &mpsc::Sender<Event>, session: &Session, config: &Config) {
    events
        .send(Event::ContextChanged {
            tokens: session.meta.context_tokens,
            max_tokens: config.active_model().max_context_tokens,
        })
        .await
        .ok();
}

fn request_context(messages: &[Message], meta: &SessionMeta) -> Vec<Message> {
    let mut context = if let Some(summary) = &meta.compaction_summary {
        let mut context = vec![Message::system(format!(
            "Conversation summary for continuation:\n{summary}"
        ))];
        context.extend(
            messages[meta.compacted_through.min(messages.len())..]
                .iter()
                .filter(|message| !is_compaction_marker(message))
                .cloned(),
        );
        context
    } else {
        messages.to_vec()
    };
    eject_consumed_web_results(&mut context);
    strip_display_metadata(&mut context);
    context
}

fn strip_display_metadata(messages: &mut [Message]) -> usize {
    messages
        .iter_mut()
        .filter_map(|message| match message {
            Message::Tool { diff, .. } => diff.take(),
            Message::Assistant { raw_request, .. } | Message::System { raw_request, .. } => {
                *raw_request = None;
                None
            }
            _ => None,
        })
        .count()
}

fn is_compaction_marker(message: &Message) -> bool {
    matches!(message, Message::System { content, .. } if content.starts_with(COMPACTION_MARKER))
}

fn eject_consumed_web_results(messages: &mut [Message]) -> usize {
    let mut browser_calls = HashSet::new();
    let mut pending = Vec::new();
    let mut consumed = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        match message {
            Message::Assistant { tool_calls, .. } => {
                if tool_calls.is_empty() {
                    consumed.append(&mut pending);
                }
                browser_calls.extend(
                    tool_calls
                        .iter()
                        .filter(|call| call.name == "web_browser")
                        .map(|call| call.id.clone()),
                );
            }
            Message::Tool { call_id, .. } if browser_calls.remove(call_id) => pending.push(index),
            _ => {}
        }
    }

    let mut ejected = 0;
    for index in consumed {
        let Message::Tool { content, .. } = &mut messages[index] else {
            continue;
        };
        if let Some(marker) = web_result_marker(content) {
            *content = marker;
            ejected += 1;
        }
    }
    ejected
}

fn web_result_marker(content: &str) -> Option<String> {
    let result: serde_json::Value = serde_json::from_str(content).ok()?;
    let page = result.get("content")?.as_str()?;
    serde_json::to_string_pretty(&serde_json::json!({
        "ejected": true,
        "tool": "web_browser",
        "url": result.get("url").and_then(serde_json::Value::as_str).unwrap_or_default(),
        "title": result.get("title").and_then(serde_json::Value::as_str).unwrap_or_default(),
        "original_chars": page.chars().count(),
        "note": "The full page was consumed by the previous assistant response. Call web_browser again if it is needed."
    }))
    .ok()
}

fn is_ejected_web_result(message: &Message) -> bool {
    let Message::Tool { content, .. } = message else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|value| value.get("ejected").and_then(serde_json::Value::as_bool))
        == Some(true)
}

#[allow(clippy::too_many_arguments)]
async fn turn<P: Provider + ?Sized>(
    provider: Arc<P>,
    tools: &ToolRegistry,
    config: &Config,
    mut messages: Vec<Message>,
    visible_through: usize,
    context_tokens: u64,
    project_prompt: Option<String>,
    generate_title: bool,
    steers: SteerQueue,
    progress: &TurnProgressHandle,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
    agent: &Agent,
    session_name: &str,
    turn_id: &str,
    delegation: &crate::tool::DelegationPort,
    agents: &AgentCatalog,
) -> Result<TurnResult> {
    let mut compaction = None;
    // The provider's last reported usage is ground truth for everything
    // already in the context, so predict the next request as that usage
    // plus the new user message. Fall back to the whole-context estimate
    // on a cold start, before any request has reported usage.
    let (known, unknown) = if context_tokens == 0 {
        (0u64, &messages[..])
    } else {
        (
            context_tokens,
            &messages[messages.len().saturating_sub(1)..],
        )
    };
    let schema_tokens =
        estimate_tool_tokens(&tools.definitions_for(agent, config.active_model().vision));
    let estimated = known
        .saturating_add(estimate_tokens(unknown))
        .saturating_add(if context_tokens == 0 {
            schema_tokens
        } else {
            0
        });
    let max_tokens = config.active_model().max_context_tokens;
    if estimated as f64 >= max_tokens as f64 * config.compaction_threshold as f64 {
        let user = messages.pop().context("missing user message")?;
        let (summary, raw_request) = summarize(
            provider.clone(),
            config,
            &messages,
            events,
            internal,
            Some(progress),
        )
        .await?;
        messages = vec![
            Message::system(format!("Conversation summary for continuation:\n{summary}")),
            user,
        ];
        compaction = Some(Compaction {
            raw_request,
            summary,
            through: visible_through,
        });
    }
    let persist_from = messages.len().saturating_sub(1);
    // A turn-start compaction replaces the whole context with its summary,
    // so record it for interruption salvage: the summary subsumes the
    // history that stays in the session file but leaves the model context.
    progress.lock().unwrap().compaction = compaction.clone();
    if let Some(compaction) = &compaction {
        events
            .send(Event::ContextCompacted {
                summary: compaction.summary.clone(),
            })
            .await
            .ok();
    }
    let (completed, mid_turn_compaction) = self::agent(
        provider.clone(),
        tools,
        config,
        messages,
        persist_from,
        visible_through,
        project_prompt,
        &steers,
        progress,
        events,
        internal,
        agent,
        session_name,
        turn_id,
        delegation,
        agents,
    )
    .await?;
    if let Some(mid_turn) = mid_turn_compaction {
        compaction = Some(mid_turn);
    }
    let title = if generate_title {
        Some(generate_session_title(provider, config, &completed, events, internal).await)
    } else {
        None
    };
    Ok(TurnResult {
        completed,
        compaction,
        title,
    })
}

/// Summarizes `messages` into a dense continuation summary in its own
/// light-reasoning model request. Used for the turn-start compaction and
/// for the mid-turn compaction that frees room for a tool result.
async fn summarize<P: Provider + ?Sized>(
    provider: Arc<P>,
    config: &Config,
    messages: &[Message],
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
    progress: Option<&TurnProgressHandle>,
) -> Result<(String, Option<String>)> {
    events.send(Event::CompactionStarted).await.ok();
    events
        .send(Event::ModelRequestStarted(config.model_id().to_owned()))
        .await
        .ok();
    let max_context = config.active_model().max_context_tokens;
    let mut request_messages = vec![Message::system(
        "Summarize this conversation for seamless continuation. Preserve requirements, decisions, files, commands, errors, results, and unresolved work. Be dense and factual. Return only the summary."
            .into(),
    )];
    request_messages.extend(messages.iter().cloned());
    // The instruction is repeated as the final user turn. A system prompt
    // alone at the top of a long conversation — especially one that
    // replays the model's own earlier reasoning — is routinely ignored:
    // the model role-plays the conversation's continuation and never
    // writes the summary.
    request_messages.push(Message::user_with_images(
        "Write the continuation summary of the conversation above now. Return only the summary."
            .into(),
        Vec::new(),
    ));
    // The request must leave the summary inside the context, and the
    // summary should not outgrow a sane fraction of the conversation it
    // replaces. The budget also must not be a small fixed number: a
    // reasoning model spends part of the same output budget on thinking
    // before it writes the summary, so scale it to the conversation
    // (one eighth), floored at 4096 tokens.
    //
    // A long conversation can crowd the request past the context. Drop
    // the oldest messages — a tool result leaves only with the assistant
    // message that called it — until the input *and* the scaled output
    // budget both fit. Trimming only to the minimum would leave a
    // reasoning model a few hundred tokens to think and answer in, so it
    // runs out mid-thought and writes no summary at all.
    // small windows need a smaller output reserve to retain the source
    let output_budget = |input| {
        summary_output_budget(input)
            .min(max_context / 4)
            .max(SUMMARY_MIN_OUTPUT_TOKENS)
    };
    strip_display_metadata(&mut request_messages);
    let mut input_tokens = estimate_tokens(&request_messages);
    while input_tokens + output_budget(input_tokens) > max_context
        && drop_oldest_message(&mut request_messages)
    {
        input_tokens = estimate_tokens(&request_messages);
    }
    if request_messages.len() <= 2 {
        bail!("context exhausted: no conversation fits in the compaction request");
    }
    let max_tokens = max_context
        .saturating_sub(input_tokens)
        .min(output_budget(input_tokens));
    if max_tokens < SUMMARY_MIN_OUTPUT_TOKENS {
        bail!("context exhausted: the compaction request itself does not fit the context");
    }
    // The summary is extraction, not problem solving: with reasoning on,
    // the model can spend the entire shared output budget thinking and
    // never write the summary. Run it without reasoning when the model
    // allows, at the lightest effort otherwise.
    let first_effort = if config
        .active_model()
        .reasoning_efforts
        .contains(&ReasoningEffort::None)
    {
        Some(ReasoningEffort::None)
    } else {
        config.light_reasoning_effort()
    };
    let request = CompletionRequest {
        provider: config.provider_name().to_owned(),
        model: config.model_id().to_owned(),
        messages: request_messages,
        temperature: config.effective_temperature(),
        reasoning_effort: first_effort,
        max_tokens: Some(max_tokens.min(u32::MAX as u64) as u32),
        stream: true,
        tools: Vec::new(),
    };
    let (summary, end, raw_request) =
        summarize_stream(&provider, request, events, internal, progress).await?;
    // The summary must be real output text. A reasoning block is the
    // model's chain of thought, not a dense continuation summary, and
    // persisting it is exactly the "thinking leak" that corrupts the
    // replayed context — so an answer-less response fails instead of
    // masquerading as a summary.
    let summary = summary.trim().to_owned();
    if summary.is_empty() {
        let why = match end {
            SummaryEnd::Completed => "completed without writing any summary text".to_string(),
            SummaryEnd::Truncated(reason) => {
                format!("was truncated ({reason}) before writing any summary text")
            }
            SummaryEnd::Ended => {
                "ended before finishing without writing any summary text".to_string()
            }
        };
        bail!("compaction produced no summary text: {why}");
    }
    Ok((summary, raw_request))
}

/// How a compaction response ended.
enum SummaryEnd {
    /// A terminal event confirmed the response finished.
    Completed,
    /// The response ran out of its output budget.
    Truncated(String),
    /// The stream closed without any terminal event.
    Ended,
}

/// Runs one compaction request and returns its final output text and how
/// the response ended. Reasoning deltas are not part of the answer.
async fn summarize_stream<P: Provider + ?Sized>(
    provider: &Arc<P>,
    request: CompletionRequest,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
    progress: Option<&TurnProgressHandle>,
) -> Result<(String, SummaryEnd, Option<String>)> {
    let (mut stream, raw_request) =
        stream_with_retry(provider, request, events, true, progress).await?;
    let mut summary = String::new();
    let mut end = None;
    let mut started = false;
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        if !started {
            events.send(Event::ResponseStarted).await.ok();
            started = true;
        }
        match delta {
            ResponseDelta::Text(text) => summary.push_str(&text),
            ResponseDelta::Completed => end = Some(SummaryEnd::Completed),
            ResponseDelta::Truncated(reason) => end = Some(SummaryEnd::Truncated(reason)),
            ResponseDelta::Usage(usage) => {
                internal.send(InternalEvent::AuxiliaryUsage(usage)).await?;
            }
            ResponseDelta::Reasoning(_)
            | ResponseDelta::ToolCall { .. }
            | ResponseDelta::OutputItem(_) => {}
        }
    }
    Ok((summary, end.unwrap_or(SummaryEnd::Ended), raw_request))
}

/// The compaction's output budget: one eighth of the conversation it
/// summarizes, floored at 4096 tokens so a reasoning model has room to
/// think before it writes the summary.
fn summary_output_budget(input_tokens: u64) -> u64 {
    input_tokens.div_ceil(8).max(4_096)
}

/// drops the oldest message and its tool results, retaining both summary instructions
fn drop_oldest_message(request: &mut Vec<Message>) -> bool {
    if request.len() <= 2 {
        return false;
    }
    let mut extra = 0;
    if let Message::Assistant { tool_calls, .. } = &request[1] {
        let ids = tool_calls
            .iter()
            .map(|call| &call.id)
            .collect::<HashSet<_>>();
        for message in request.iter().skip(2) {
            if let Message::Tool { call_id, .. } = message
                && ids.contains(call_id)
            {
                extra += 1;
            } else {
                break;
            }
        }
    }
    request.drain(1..2 + extra);
    true
}

async fn generate_session_title<P: Provider + ?Sized>(
    provider: Arc<P>,
    config: &Config,
    messages: &[Message],
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> String {
    let fallback = fallback_session_title(messages);
    let Some(user) = messages.iter().find_map(|message| match message {
        Message::User { content, .. } => Some(strip_runtime_context(content).to_owned()),
        _ => None,
    }) else {
        return fallback;
    };
    let assistant = messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant { content, .. } => Some(content.as_str()),
            _ => None,
        })
        .unwrap_or_default();
    let prompt = format!(
        "User:\n{}\n\nAssistant:\n{}",
        user.chars().take(1200).collect::<String>(),
        assistant.chars().take(1200).collect::<String>()
    );
    events
        .send(Event::ModelRequestStarted(config.model_id().to_owned()))
        .await
        .ok();
    let request = CompletionRequest {
        provider: config.provider_name().to_owned(),
        model: config.model_id().to_owned(),
        messages: vec![
            Message::system(
                "Create a concise 2-3 word title for this conversation. Return only the title without quotes, markdown, punctuation, or explanation."
                    .into(),
            ),
            Message::user_with_images(prompt, Vec::new()),
        ],
        temperature: None,
        reasoning_effort: config.light_reasoning_effort(),
        max_tokens: Some(512),
        stream: true,
        tools: Vec::new(),
    };
    let result: Result<(String, String)> = async {
        let (mut stream, _) = stream_with_retry(&provider, request, events, false, None).await?;
        let mut reasoning = String::new();
        let mut text = String::new();
        let mut started = false;
        while let Some(delta) = stream.next().await {
            let delta = delta?;
            if !started {
                events.send(Event::ResponseStarted).await.ok();
                started = true;
            }
            match delta {
                ResponseDelta::Reasoning(delta) => reasoning.push_str(&delta),
                ResponseDelta::Text(delta) => text.push_str(&delta),
                ResponseDelta::Usage(usage) => {
                    internal.send(InternalEvent::AuxiliaryUsage(usage)).await?;
                }
                ResponseDelta::ToolCall { .. } | ResponseDelta::OutputItem(_) => {}
                // The title is best-effort; a truncated stream still
                // yields whatever text arrived.
                ResponseDelta::Truncated(_) => {}
                ResponseDelta::Completed => {}
            }
        }
        Ok((text, reasoning))
    }
    .await;
    result
        .ok()
        .and_then(|(text, reasoning)| {
            clean_session_title(&text).or_else(|| clean_session_title(&reasoning))
        })
        .unwrap_or(fallback)
}

fn clean_session_title(raw: &str) -> Option<String> {
    let raw = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| value.get("title")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| raw.to_owned());
    let mut line = raw
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())?
        .trim();
    line = line.trim_matches(|char: char| {
        char.is_whitespace() || matches!(char, '"' | '\'' | '`' | '#' | '*' | '.')
    });
    if line
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("title:"))
    {
        line = line[6..].trim();
    }
    let mut title = line
        .split_whitespace()
        .map(|word| {
            word.trim_matches(|char: char| !char.is_alphanumeric() && !matches!(char, '-' | '\''))
        })
        .filter(|word| !word.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" ");
    if !title.is_empty() && !title.contains(char::is_whitespace) {
        title.push_str(" Conversation");
    }
    (!title.is_empty()).then_some(title)
}

fn fallback_session_title(messages: &[Message]) -> String {
    let content = messages.iter().find_map(|message| match message {
        Message::User { content, .. } => Some(content.as_str()),
        _ => None,
    });
    content
        .map(strip_runtime_context)
        .and_then(clean_session_title)
        .unwrap_or_else(|| "New Conversation".into())
}

fn estimate_tokens(messages: &[Message]) -> u64 {
    let mut tokens = messages
        .iter()
        .map(|message| serde_json::to_string(message).map_or(0, |value| value.len()))
        .sum::<usize>()
        .div_ceil(4) as u64
        + messages.len() as u64 * 4;
    // `ImageContent.data` is skipped during serialization, so the JSON pass
    // above never sees an image; reserve the tokens the provider charges
    // for sending it.
    for message in messages {
        tokens += message.images().iter().map(image_tokens).sum::<u64>();
    }
    tokens
}

/// Tokens one attached image costs the OpenAI provider. Rope sends images
/// with the default `auto` detail; for the tile-based models (gpt-4o and
/// gpt-4.1) that fits the image into a 2048px square, caps the shortest
/// side at 768px, and bills 85 base tokens plus 170 per 512px tile.
/// Images without recorded dimensions reserve the rule's per-image
/// maximum, so an image can never cost more than the budget reserved.
fn image_tokens(image: &ImageContent) -> u64 {
    const BASE: u64 = 85;
    const TILE: u64 = 170;
    const MAX_SIDE: u32 = 2048;
    const SHORT_SIDE: u32 = 768;
    const TILE_SIZE: u32 = 512;
    if image.width == 0 || image.height == 0 {
        return BASE + 8 * TILE;
    }
    let (mut width, mut height) = (image.width, image.height);
    // Fit into a 2048px square, preserving aspect ratio, never enlarging.
    let longest = width.max(height);
    if longest > MAX_SIDE {
        if width >= height {
            height = fit_side(height, longest);
            width = MAX_SIDE;
        } else {
            width = fit_side(width, longest);
            height = MAX_SIDE;
        }
    }
    // Cap the shortest side at 768px, flooring the other dimension.
    let (shortest, longest) = (width.min(height), width.max(height));
    if shortest > SHORT_SIDE {
        // `other` is strictly below `longest`, so it fits a u32.
        let other = (longest as u64 * SHORT_SIDE as u64 / shortest as u64) as u32;
        if width <= height {
            width = SHORT_SIDE;
            height = other;
        } else {
            height = SHORT_SIDE;
            width = other;
        }
    }
    BASE + (width.div_ceil(TILE_SIZE) * height.div_ceil(TILE_SIZE)).min(8) as u64 * TILE
}

/// `side * MAX_SIDE / longest`, rounded up so the estimate never undershoots.
fn fit_side(side: u32, longest: u32) -> u32 {
    let scaled = (side as u64 * 2048 + longest as u64 - 1) / longest as u64;
    scaled.clamp(1, u32::MAX as u64) as u32
}

/// Tokens a Tool message costs before its content: the role, call id,
/// image and diff slots, plus the per-message overhead of
/// `estimate_tokens`. Reserved before the tool runs, so the content budget
/// is what the next model request actually has left.
fn tool_message_overhead(call: &ToolCall) -> u64 {
    let message = Message::tool(call.id.clone(), String::new(), None, None);
    estimate_tokens(std::slice::from_ref(&message))
}

#[allow(clippy::too_many_arguments)]
async fn agent<P: Provider + ?Sized>(
    provider: Arc<P>,
    tools: &ToolRegistry,
    config: &Config,
    mut messages: Vec<Message>,
    persist_from: usize,
    user_full_index: usize,
    project_prompt: Option<String>,
    steers: &SteerQueue,
    progress: &TurnProgressHandle,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
    agent: &Agent,
    session_name: &str,
    turn_id: &str,
    delegation: &crate::tool::DelegationPort,
    agents: &AgentCatalog,
) -> Result<(Vec<Message>, Option<Compaction>)> {
    let mut compaction = None;
    let mut used_context_tokens: Option<u64> = None;
    let mut recovered_truncation = false;
    progress.lock().unwrap().messages = messages[persist_from..].to_vec();
    loop {
        let queued = steers.lock().unwrap().iter().cloned().collect::<Vec<_>>();
        let predicted =
            used_context_tokens.map(|used| used.saturating_add(estimate_tokens(&queued)));
        let max_tokens = config.active_model().max_context_tokens;
        let compact_before_request = predicted.is_some_and(|used| {
            used as f64 >= max_tokens as f64 * config.compaction_threshold as f64
        });
        if compact_before_request {
            let boundary = messages.len();
            compaction = Some(
                compact_mid_turn(
                    provider.clone(),
                    config,
                    &mut messages,
                    boundary,
                    user_full_index,
                    progress,
                    events,
                    internal,
                )
                .await?,
            );
        }
        // Steering prompts sent during this turn are injected here, so the
        // next model request carries them after everything delivered so
        // far — including in-flight tool results.
        let steered = steers.lock().unwrap().drain(..).collect::<Vec<_>>();
        if !steered.is_empty() {
            events
                .send(Event::SteersDelivered(steered.len()))
                .await
                .ok();
            messages.extend(steered.iter().cloned());
            progress.lock().unwrap().messages.extend(steered);
        }
        if compact_before_request {
            let available = available_context_tokens(
                &messages,
                tools,
                config,
                project_prompt.as_deref(),
                agent,
            );
            if available == 0 {
                bail!("context exhausted: compaction could not free room for the next request");
            }
            events
                .send(Event::ContextChanged {
                    tokens: max_tokens - available,
                    max_tokens,
                })
                .await
                .ok();
        }
        events
            .send(Event::ModelRequestStarted(config.model_id().to_owned()))
            .await
            .ok();
        let mut request_messages = messages.clone();
        strip_display_metadata(&mut request_messages);
        if let Some(prompt) = &project_prompt {
            request_messages.insert(0, Message::system(prompt.clone()));
        }
        let tool_definitions = tools.definitions_for(agent, config.active_model().vision);
        let request = CompletionRequest {
            provider: config.provider_name().to_owned(),
            model: config.model_id().to_owned(),
            messages: request_messages,
            temperature: config.effective_temperature(),
            reasoning_effort: config.effective_reasoning_effort(),
            max_tokens: None,
            stream: true,
            tools: tool_definitions,
        };
        // A stream can break partway through a response — the connection
        // drops, or a chunk or a tool call arrives that cannot be decoded.
        // Such a response is discarded and the request starts over, like a
        // connection failure before the first byte: only complete responses
        // reach the transcript, and the retry pacing is the same capped
        // backoff. A response with a permanent error propagates unchanged.
        let (reasoning, text, mut calls, usage, response_items, truncated, raw_request) = {
            let mut attempt = 0;
            loop {
                let (stream, raw_request) =
                    stream_with_retry(&provider, request.clone(), events, true, Some(progress))
                        .await?;
                match collect(stream, events, internal).await {
                    Ok((reasoning, text, calls, usage, response_items, truncated)) => {
                        break (
                            reasoning,
                            text,
                            calls,
                            usage,
                            response_items,
                            truncated,
                            raw_request,
                        );
                    }
                    Err(error) if is_retryable(&error) => {
                        let seconds = retry_delay(attempt);
                        events.send(Event::ResponseDiscarded).await.ok();
                        events.send(Event::Retrying { seconds }).await.ok();
                        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                        attempt += 1;
                        events
                            .send(Event::ModelRequestStarted(config.model_id().to_owned()))
                            .await
                            .ok();
                    }
                    Err(error) => return Err(error),
                }
            }
        };
        // The tool call cap applies between assistant messages, not per turn.
        calls.truncate(MAX_TOOL_CALLS_PER_MESSAGE);
        let response = Message::assistant_response(
            text,
            config.model_id().to_owned(),
            reasoning,
            calls.clone(),
            response_items,
        )
        .with_raw_request(raw_request)
        .with_agent(agent.id.clone());
        messages.push(response.clone());
        progress.lock().unwrap().messages.push(response);
        let mut used = usage.map_or_else(
            || {
                config
                    .active_model()
                    .max_context_tokens
                    .saturating_sub(available_context_tokens(
                        &messages,
                        tools,
                        config,
                        project_prompt.as_deref(),
                        agent,
                    ))
            },
            |usage| usage.total_tokens,
        );
        if calls.is_empty() {
            if let Some(reason) = truncated {
                if !recovered_truncation
                    && used as f64 >= max_tokens as f64 * config.compaction_threshold as f64
                {
                    // a full context can cut off the response before it emits a tool call
                    recovered_truncation = true;
                    used_context_tokens = Some(used);
                    continue;
                }
                bail!("model response was truncated before finishing: {reason}");
            }
            // The turn is done: no job outlives it, so a command the model
            // stopped polling is killed instead of running unattended.
            tools.cancel_active().await;
            return Ok((progress.lock().unwrap().messages.clone(), compaction));
        }

        for (index, call) in calls.iter().enumerate() {
            events
                .send(Event::ToolCallFinished {
                    index,
                    call: call.clone(),
                })
                .await
                .ok();
        }

        // The calls one assistant message batches are independent by
        // construction: the model cannot have seen any of their results
        // yet. They run side by side, and the turn waits for the whole
        // batch before asking the model again, so the results the model
        // receives are one consistent snapshot, in the order the calls
        // were made. Those results share the context that is left, so the
        // budgets — and any mid-turn compaction they require — are planned
        // for the whole batch before the first tool starts.
        let budgets = tool_output_budgets(
            provider.clone(),
            tools,
            config,
            &mut messages,
            user_full_index,
            progress,
            events,
            internal,
            project_prompt.as_deref(),
            agent,
            &mut compaction,
            &mut used,
            &calls,
        )
        .await?;
        let lane = ToolLane {
            tools,
            config,
            events,
            internal,
            agent,
            session_name,
            turn_id,
            delegation,
            agents,
            approval: AsyncMutex::new(()),
            child: AsyncMutex::new(()),
        };
        // The calls finish whenever their work does; the turn's transcript
        // takes their results in call order as soon as the calls before them
        // have theirs. Recording each result when its call finishes is also
        // what an interrupted turn preserves: a call that already wrote a
        // file or ran a command is not reported as cancelled.
        let batch = Mutex::new(TurnBatch::new(calls.len()));
        // A call the turn cannot run at all — the events of a closing
        // client could not be delivered, so not even an approval could
        // reach the user — answers with its error, so the batch stays a
        // complete answer to the model's tool calls and the calls that did
        // run keep their place. The turn still stops with the first such
        // error, in the order the model made the calls. A call that names
        // no tool is not one of these: its error answers as a failed tool
        // result and the turn goes on, leaving the model to correct the
        // name it used.
        let failures: Mutex<Vec<Option<Error>>> =
            Mutex::new((0..calls.len()).map(|_| None).collect());
        let guard = BatchGuard {
            batch: &batch,
            transcript: progress.clone(),
        };
        join_all(calls.iter().enumerate().map(|(index, call)| {
            let batch = &batch;
            let lane = &lane;
            let transcript = progress.clone();
            let failures = &failures;
            let budget = budgets[index];
            async move {
                let message = match run_tool_call(lane, call, budget).await {
                    Ok(message) => message,
                    Err(error) => {
                        // The answer of a call the turn cannot run is
                        // a tool result like any other, with the
                        // budget the batch was planned with.
                        let text = truncate_tool_output(
                            &call.id,
                            None,
                            None,
                            format!("Error: {error:#}"),
                            budget.message_budget,
                        );
                        failures.lock().unwrap()[index] = Some(error);
                        // The call asked for nothing further, but the
                        // client is waiting for its answer too.
                        lane.events
                            .send(Event::ToolResult {
                                call_id: call.id.clone(),
                                output: text.clone(),
                                success: false,
                                diff: None,
                            })
                            .await
                            .ok();
                        Message::tool(call.id.clone(), text, None, None)
                    }
                };
                let ready = batch.lock().unwrap().record(index, &message);
                if !ready.is_empty() {
                    transcript.lock().unwrap().messages.extend(ready);
                }
            }
        }))
        .await;
        drop(guard);
        // The whole batch is awaited before the turn continues.
        let failure = failures
            .lock()
            .unwrap()
            .iter_mut()
            .find_map(|failure| failure.take());
        // The working conversation and the context accounting take the whole
        // batch at once, before the next model request.
        for message in batch.lock().unwrap().results() {
            used = used.saturating_add(estimate_tokens(std::slice::from_ref(&message)));
            messages.push(message);
        }
        if let Some(error) = failure {
            return Err(error);
        }
        recovered_truncation = false;
        used_context_tokens = Some(used);
    }
}

/// The results of one batch of tool calls, held so they can be recorded in
/// call order. Calls finish in whatever order their work takes, while the
/// conversation — the next model request, and the work an interrupted turn
/// keeps — follows the order the model made them in.
struct TurnBatch {
    /// One slot per call of the batch, filled as its result arrives.
    slots: Vec<Option<Message>>,
    /// The first result the transcript is still waiting for.
    next: usize,
}

impl TurnBatch {
    fn new(count: usize) -> Self {
        Self {
            slots: (0..count).map(|_| None).collect(),
            next: 0,
        }
    }

    /// Records one call's result and returns the results the transcript can
    /// take now: the run of finished calls starting at its cursor.
    fn record(&mut self, index: usize, message: &Message) -> Vec<Message> {
        self.slots[index] = Some(message.clone());
        let mut ready = Vec::new();
        while matches!(self.slots.get(self.next), Some(Some(_))) {
            ready.push(self.slots[self.next].clone().unwrap());
            self.next += 1;
        }
        ready
    }

    /// The results that are in but were held back behind a call that never
    /// answered. An interrupted turn takes them: what a call already did
    /// happened, whatever the calls around it were doing when the user
    /// stopped it.
    fn unreleased(&self) -> Vec<Message> {
        self.slots[self.next..].iter().flatten().cloned().collect()
    }

    /// Every result of the batch, in call order.
    fn results(&mut self) -> Vec<Message> {
        self.slots.iter_mut().filter_map(Option::take).collect()
    }
}

/// Takes the results a batch holds back when its future is dropped
/// mid-flight — a turn the user stopped — so a call that finished is not
/// written off as one that never answered. The results still land in call
/// order, because the slots are ordered by the calls.
struct BatchGuard<'a> {
    batch: &'a Mutex<TurnBatch>,
    transcript: TurnProgressHandle,
}

impl Drop for BatchGuard<'_> {
    fn drop(&mut self) {
        let unreleased = self.batch.lock().unwrap().unreleased();
        if !unreleased.is_empty() {
            self.transcript.lock().unwrap().messages.extend(unreleased);
        }
    }
}

/// Runs one tool call of a batch to completion: policy and approval, the
/// tool itself or a delegation, the budgeted truncation, and the events
/// the clients render. The Tool message it returns is appended to the
/// conversation by the caller, in call order, once every call in the batch
/// has finished.
async fn run_tool_call(
    lane: &ToolLane<'_>,
    call: &ToolCall,
    budget: ToolBudget,
) -> Result<Message> {
    // A call that names no tool answers with its error like any other
    // failed tool: the model sees the name it got wrong and the turn
    // goes on, instead of the whole turn stopping on the unknown name.
    let entry = lane.tools.get(&call.name);
    // The effective policy and the approval key are scoped to the acting
    // agent, so one agent's grants never apply to another.
    let approved = match &entry {
        Ok(entry) => match lane
            .agent
            .effective_policy(&call.name, &entry.category, entry.approval)
        {
            Approval::Allow => true,
            Approval::Deny => false,
            Approval::Ask => {
                // A session carries one pending approval at a time, so calls
                // take turns asking while the rest of the batch keeps working.
                let _gate = lane.approval.lock().await;
                let (reply, decision) = oneshot::channel();
                lane.internal
                    .send(InternalEvent::Approval {
                        call: call.clone(),
                        approval_key: format!("{}|{}", lane.agent.id, entry.approval_key),
                        reply,
                    })
                    .await
                    .with_context(|| format!("deliver approval for {}", call.name))?;
                decision.await.unwrap_or(ApprovalDecision::Deny) != ApprovalDecision::Deny
            }
        },
        Err(_) => false,
    };
    let result = if approved {
        let entry = entry.expect("approval implies a known tool");
        // A parent session waits on at most one direct child: the core
        // links one child per parent and routes steers down that single
        // chain, so a batch delegates one child at a time instead of
        // growing a sibling scheduler. The call is only marked as started
        // once it holds the lane, so the chat never shows several children
        // working at once.
        let _child = if call.name == crate::tool::SUBAGENT_TOOL {
            Some(lane.child.lock().await)
        } else {
            None
        };
        lane.events
            .send(Event::ToolStarted {
                call_id: call.id.clone(),
            })
            .await
            .ok();
        if call.name == crate::tool::SUBAGENT_TOOL {
            // The runtime executes delegations itself: it supplies the
            // session, turn, and call identity the core needs to link the
            // child.
            run_delegation(
                lane.delegation,
                lane.session_name,
                lane.turn_id,
                call,
                lane.config.model_name(),
                lane.agent,
                lane.agents,
            )
            .await
        } else {
            let (delta_tx, delta_rx) = mpsc::unbounded_channel();
            let forward = tokio::spawn(forward_tool_output_deltas(
                call.id.clone(),
                delta_rx,
                budget.max_streamed_bytes,
                lane.events.clone(),
            ));
            let result = entry
                .tool
                .run_streamed(
                    call.arguments.clone(),
                    Some(delta_tx),
                    budget.max_streamed_bytes,
                )
                .await;
            // Let in-flight deltas land before the final result.
            forward.await.ok();
            result
        }
    } else if let Err(error) = entry {
        Err(error)
    } else {
        bail_tool_denied(&call.name)
    };
    let (mut output, mut image, file, diff, success) = match result {
        Ok(result) => (
            result.output,
            result.image,
            result.file,
            result.diff,
            !result.is_error,
        ),
        Err(error) => (format!("Error: {error:#}"), None, None, None, false),
    };
    // A plan document is session state, not prose: read it from the full
    // result, so a call budget short enough to cut the model's copy of it
    // never breaks the plan the pane and the pinned runtime context keep.
    let plan = if success && call.name == "update_plan" {
        Some(
            serde_json::from_str::<ExecutionPlan>(&output)
                .context("decode normalized execution plan")?,
        )
    } else {
        None
    };
    // A delegation result is one JSON document: shrink it within the
    // content budget while it stays valid JSON, so the control fields the
    // UI and the model rely on survive the limit.
    if call.name == crate::tool::SUBAGENT_TOOL {
        output = bounded_subagent_json(&output, budget.max_streamed_bytes);
    }
    // The pre-run reservation covered the text, not the image. An image
    // that would crowd the result past its budget is replaced by a note in
    // the content, so the next model request still fits.
    if let Some(reserved) = image.as_ref() {
        let with_image = Message::tool(
            call.id.clone(),
            String::new(),
            Some(reserved.clone()),
            diff.clone(),
        );
        if estimate_tokens(std::slice::from_ref(&with_image)) + MIN_TOOL_OUTPUT_TOKENS
            > budget.message_budget
        {
            output.push_str("\n[image omitted: no room left in the context]");
            image = None;
        }
    }
    let output = truncate_tool_output(
        &call.id,
        image.clone(),
        diff.clone(),
        output,
        budget.message_budget,
    );
    if let Some(image) = &image {
        lane.events
            .send(Event::ToolImage {
                call_id: call.id.clone(),
                image: image.clone(),
            })
            .await
            .ok();
    }
    if let Some(file) = &file {
        lane.events
            .send(Event::ToolFile {
                call_id: call.id.clone(),
                file: file.clone(),
            })
            .await
            .ok();
    }
    // A plan is session state the panes keep, so it is delivered before the
    // result event: if the client is already gone and this send fails, the
    // turn ends with one failed result for the call, not two.
    if let Some(plan) = plan {
        lane.internal.send(InternalEvent::PlanUpdated(plan)).await?;
    }
    lane.events
        .send(Event::ToolResult {
            call_id: call.id.clone(),
            output: output.clone(),
            success,
            diff: diff.clone(),
        })
        .await
        .ok();
    if approved {
        // The working tree may have changed; the runtime coalesces these refreshes.
        lane.internal.send(InternalEvent::ProjectRefresh).await.ok();
    }
    Ok(Message::Tool {
        call_id: call.id.clone(),
        content: output,
        image,
        file,
        diff,
    })
}

/// The context one call of a batch may use. `message_budget` bounds the
/// whole Tool message — framing, image, and content, counted on the
/// populated message — and `max_streamed_bytes` is the matching byte cap
/// for streamed output: it bounds the final truncation in raw bytes, so
/// the chat never shows more output than the model keeps (the final
/// truncation may keep less once JSON escaping of the content is
/// counted), while the floor keeps a control envelope — a shell job's
/// status and job_id — intact near the context limit.
#[derive(Clone, Copy)]
struct ToolBudget {
    message_budget: u64,
    max_streamed_bytes: usize,
}

/// Shares the context left among the calls of one batch, before any of
/// them runs: their results arrive together, so the room they may fill is
/// divided up front instead of call by call. Each call keeps its message
/// framing and gets the smaller of one fifth of the room that is left —
/// the cap a single call has always been given — and an equal share of the
/// context a turn keeps before it would compact, which is what stops a
/// batch from spending the reserve the next model request needs. That
/// second bound applies only while the batch can still fit inside that
/// window; a turn already at its compaction point shares the room, since
/// its next request compacts whatever the results cost. Either way the
/// results of a batch never cost more context than the room, and no call
/// is cut below one control envelope. When framing plus those envelopes no
/// longer fit the model context, the conversation is compacted mid-turn
/// first, keeping the pending batch and the prompts that precede it, and
/// the turn fails with a clear error if compaction cannot free the room.
#[allow(clippy::too_many_arguments)]
async fn tool_output_budgets<P: Provider + ?Sized>(
    provider: Arc<P>,
    tools: &ToolRegistry,
    config: &Config,
    messages: &mut Vec<Message>,
    user_full_index: usize,
    progress: &TurnProgressHandle,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
    project_prompt: Option<&str>,
    agent: &Agent,
    compaction: &mut Option<Compaction>,
    used: &mut u64,
    calls: &[ToolCall],
) -> Result<Vec<ToolBudget>> {
    let max_tokens = config.active_model().max_context_tokens;
    // The result is one Tool message per call: reserve its framing (role,
    // call id) and per-message overhead up front, so the budget bounds the
    // next model request, not just the content.
    let framing = calls.iter().map(tool_message_overhead).collect::<Vec<_>>();
    let framing_total = framing.iter().sum::<u64>();
    // Every call of the batch keeps at least one control envelope.
    let envelope = calls.len() as u64 * MIN_TOOL_OUTPUT_TOKENS;
    let floor = framing_total.saturating_add(envelope);
    if floor > max_tokens.saturating_sub(*used) {
        // keep the pending batch and its preceding prompts together
        let mut boundary = messages
            .iter()
            .rposition(|message| matches!(message, Message::Assistant { .. }))
            .unwrap();
        while boundary > 0
            && matches!(
                messages[boundary - 1],
                Message::User { .. } | Message::Steer { .. }
            )
        {
            boundary -= 1;
        }
        *compaction = Some(
            compact_mid_turn(
                provider,
                config,
                messages,
                boundary,
                user_full_index,
                progress,
                events,
                internal,
            )
            .await?,
        );
        *used = max_tokens.saturating_sub(available_context_tokens(
            messages,
            tools,
            config,
            project_prompt,
            agent,
        ));
        if floor > max_tokens.saturating_sub(*used) {
            bail!("context exhausted: compaction could not free room for a tool result");
        }
    }
    // A batch is never empty: this runs only for a message that asked for
    // tools. The room is the context that is left.
    let room = max_tokens
        .saturating_sub(*used)
        .saturating_sub(framing_total);
    // The window is the part of the context a turn keeps before it would
    // compact. Measuring a batch against the whole room lets its results
    // spend that reserve, so the next model request stops to summarize
    // before the model has read them. The measure only means anything while
    // the batch can still fit inside the window: a turn already at its
    // compaction point compacts on the next request whatever this batch
    // does, so there the calls share the room instead, and their results
    // stay worth reading rather than being cut to a control envelope.
    let window = ((max_tokens as f64 * config.compaction_threshold as f64) as u64)
        .saturating_sub(*used)
        .saturating_sub(framing_total);
    let share = if window >= envelope { window } else { room } / calls.len() as u64;
    // A fifth of the room is the ceiling one tool result has always had; an
    // equal share of the window is the ceiling a batch has. Whichever is
    // smaller bounds a call — and the control floor keeps a result an
    // envelope, not only prose.
    let cap = (room / 5).min(share).max(MIN_TOOL_OUTPUT_TOKENS);
    Ok(framing
        .into_iter()
        .map(|overhead| ToolBudget {
            message_budget: overhead.saturating_add(cap),
            max_streamed_bytes: cap.saturating_mul(4).min(usize::MAX as u64) as usize,
        })
        .collect())
}

/// What a tool call needs from the turn that owns it, borrowed. The two
/// lanes keep the session's guarantees intact while calls run side by
/// side: a session protocol carries one pending approval at a time, and a
/// parent session waits on at most one direct child.
struct ToolLane<'a> {
    tools: &'a ToolRegistry,
    config: &'a Config,
    events: &'a mpsc::Sender<Event>,
    internal: &'a mpsc::Sender<InternalEvent>,
    agent: &'a Agent,
    session_name: &'a str,
    turn_id: &'a str,
    delegation: &'a crate::tool::DelegationPort,
    agents: &'a AgentCatalog,
    approval: AsyncMutex<()>,
    child: AsyncMutex<()>,
}

/// Flips the delegation's `abandon` flag when the waiting call's future
/// drops — the turn was cancelled and no one else will — so the core stops
/// the child's turn and lets it settle.
struct AbandonGuard(watch::Sender<bool>);

impl Drop for AbandonGuard {
    fn drop(&mut self) {
        self.0.send(true).ok();
    }
}

/// Executes one `subagent` call: validates the arguments, hands a
/// `DelegationRequest` to the core over the delegation port, and waits for
/// the child's turn to settle. The child's structured outcome is the tool
/// result's only content; the runtime keeps it valid JSON within its
/// output budget.
async fn run_delegation(
    delegation: &crate::tool::DelegationPort,
    session_name: &str,
    turn_id: &str,
    call: &ToolCall,
    caller_model: &str,
    caller: &Agent,
    agents: &AgentCatalog,
) -> Result<ToolResult> {
    if !caller.can_call_subagents {
        bail!("this agent may not delegate work to subagents");
    }
    let agent = call
        .arguments
        .get("agent")
        .and_then(serde_json::Value::as_str)
        .context("the subagent call is missing its 'agent' argument")?;
    if !agents
        .get(agent)
        .is_some_and(|candidate| candidate.delegable())
    {
        bail!("agent '{agent}' is not available for delegation");
    }
    let prompt = call
        .arguments
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_owned();
    if prompt.is_empty() {
        bail!("the subagent call has no prompt");
    }
    let (reply, outcome) = oneshot::channel::<SubagentOutcome>();
    let (abandon, _abandon_rx) = watch::channel(false);
    let _guard = AbandonGuard(abandon.clone());
    delegation
        .send(DelegationCommand::Spawn(DelegationRequest {
            parent_session: session_name.to_owned(),
            parent_turn: turn_id.to_owned(),
            tool_call_id: call.id.clone(),
            agent: agent.to_owned(),
            prompt,
            caller_model: caller_model.to_owned(),
            reply,
            abandon,
        }))
        .context("core is shutting down")?;
    let outcome = outcome
        .await
        .context("the delegation was dropped before it could settle")?;
    Ok(ToolResult {
        is_error: outcome.is_error(),
        output: outcome.json(),
        image: None,
        file: None,
        diff: None,
    })
}

#[allow(clippy::too_many_arguments)]
async fn compact_mid_turn<P: Provider + ?Sized>(
    provider: Arc<P>,
    config: &Config,
    messages: &mut Vec<Message>,
    boundary: usize,
    user_full_index: usize,
    progress: &TurnProgressHandle,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> Result<Compaction> {
    let through = {
        let progress = progress.lock().unwrap();
        let through = user_full_index
            + progress
                .messages
                .len()
                .saturating_sub(messages.len() - boundary);
        if boundary == 0
            || progress
                .compaction
                .as_ref()
                .is_some_and(|previous| previous.through == through)
        {
            bail!("context exhausted: no new history to compact");
        }
        through
    };
    let (summary, raw_request) = summarize(
        provider,
        config,
        &messages[..boundary],
        events,
        internal,
        Some(progress),
    )
    .await?;
    messages.splice(0..boundary, [Message::system(format!(
        "Conversation summary for continuation:\n{summary}\n\nContinue the unfinished work from this summary."
    ))]);
    let compaction = Compaction {
        summary,
        through,
        raw_request,
    };
    progress.lock().unwrap().compaction = Some(compaction.clone());
    events
        .send(Event::ContextCompacted {
            summary: compaction.summary.clone(),
        })
        .await
        .ok();
    Ok(compaction)
}

fn available_context_tokens(
    messages: &[Message],
    tools: &ToolRegistry,
    config: &Config,
    project_prompt: Option<&str>,
    agent: &Agent,
) -> u64 {
    let mut context = messages.to_vec();
    strip_display_metadata(&mut context);
    if let Some(prompt) = project_prompt {
        context.insert(0, Message::system(prompt.into()));
    }
    let used = estimate_tokens(&context).saturating_add(estimate_tool_tokens(
        &tools.definitions_for(agent, config.active_model().vision),
    ));
    config
        .active_model()
        .max_context_tokens
        .saturating_sub(used)
}

fn estimate_tool_tokens(tools: &[ToolDefinition]) -> u64 {
    serde_json::to_string(tools)
        .map_or(0, |value| value.len())
        .div_ceil(4) as u64
}

/// Forwards partial tool output to the UI until the streamed byte budget is
/// exhausted. A delta larger than the remaining allowance is sliced at a
/// character boundary, so one large delta can never overshoot the cap. The
/// sender is dropped when the tool finishes, which ends the loop.
async fn forward_tool_output_deltas(
    call_id: String,
    mut deltas: mpsc::UnboundedReceiver<String>,
    max_bytes: usize,
    events: mpsc::Sender<Event>,
) {
    let mut forwarded = 0usize;
    while let Some(delta) = deltas.recv().await {
        let remaining = max_bytes - forwarded;
        if delta.len() <= remaining {
            let length = delta.len();
            events
                .send(Event::ToolOutputDelta {
                    call_id: call_id.clone(),
                    delta,
                })
                .await
                .ok();
            forwarded += length;
        } else {
            let end = floor_char_boundary(&delta, remaining);
            if end > 0 {
                events
                    .send(Event::ToolOutputDelta {
                        call_id: call_id.clone(),
                        delta: delta[..end].to_string(),
                    })
                    .await
                    .ok();
            }
            forwarded = max_bytes;
        }
        if forwarded >= max_bytes {
            break;
        }
    }
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut end = index.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// The serialized length of one character inside a JSON string: quotes,
/// backslashes, and the common escapes double, other control characters
/// sextuple, and everything else keeps its UTF-8 length.
fn escaped_char_len(character: char) -> usize {
    match character {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        character if (character as u32) < 0x20 => 6,
        character => character.len_utf8(),
    }
}

/// Truncates tool output so the completed Tool message — role, call id,
/// image, diff, and the JSON-escaped content — stays within
/// `max_tokens` under `estimate_tokens`. The budget bounds the message,
/// not the raw bytes: quotes, backslashes, and newlines expand when the
/// content is serialized, so a bytes-per-token cap on the raw output
/// could still leave the next model request over the limit.
fn truncate_tool_output(
    call_id: &str,
    image: Option<ImageContent>,
    diff: Option<String>,
    mut output: String,
    max_tokens: u64,
) -> String {
    // `estimate_tokens` charges four bytes per token, four per message,
    // and the image its own reservation; the serialized message is the
    // empty-content framing plus the escaped content.
    let image_reserve = image.as_ref().map_or(0, image_tokens);
    let framed = serde_json::to_string(&Message::tool(
        call_id.to_owned(),
        String::new(),
        image,
        diff,
    ))
    .map_or(0, |message| message.len() as u64);
    let content_limit = max_tokens
        .saturating_sub(4)
        .saturating_sub(image_reserve)
        .saturating_mul(4)
        .saturating_sub(framed)
        .saturating_sub(2);
    let escaped = |text: &str| -> u64 {
        serde_json::to_string(text).map_or(text.len() as u64, |value| value.len() as u64)
    };
    if escaped(&output) <= content_limit {
        return output;
    }
    // Drop the smallest suffix so the escaped content — marker included
    // when the marker itself still fits — meets the limit, ending on a
    // character boundary.
    let keep_marker = escaped(TOOL_OUTPUT_TRUNCATED) <= content_limit;
    let target = content_limit.saturating_sub(escaped(TOOL_OUTPUT_TRUNCATED));
    let total = escaped(&output);
    let (mut raw_removed, mut escaped_removed) = (0u64, 0u64);
    for character in output.chars().rev() {
        raw_removed += character.len_utf8() as u64;
        escaped_removed += escaped_char_len(character) as u64;
        if total.saturating_sub(escaped_removed) <= target {
            break;
        }
    }
    output.truncate(output.len() - raw_removed as usize);
    if keep_marker {
        output.push_str(TOOL_OUTPUT_TRUNCATED);
    }
    output
}

async fn stream_with_retry<P: Provider + ?Sized>(
    provider: &Arc<P>,
    request: CompletionRequest,
    events: &mpsc::Sender<Event>,
    record: bool,
    progress: Option<&TurnProgressHandle>,
) -> Result<(crate::provider::ResponseStream, Option<String>)> {
    let raw_request = if record {
        provider.record_request(&request).await?
    } else {
        None
    };
    if let Some(progress) = progress {
        progress.lock().unwrap().raw_request = raw_request.clone();
    }
    if raw_request.is_some() {
        events
            .send(Event::RawRequest(raw_request.clone()))
            .await
            .ok();
    }
    let mut attempt = 0;
    loop {
        match provider.stream(request.clone()).await {
            Ok(stream) => {
                events.send(Event::ResponseHeadersReceived).await.ok();
                return Ok((stream, raw_request));
            }
            Err(error) if is_retryable(&error) => {
                let seconds = retry_delay(attempt);
                events.send(Event::Retrying { seconds }).await.ok();
                tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

fn retry_delay(attempt: usize) -> u64 {
    [2, 5, 10, 30].get(attempt).copied().unwrap_or(30)
}

fn is_retryable(error: &anyhow::Error) -> bool {
    let error = format!("{error:#}").to_ascii_lowercase();
    [
        "send completion request",
        "send responses api request",
        "connection",
        "timed out",
        "timeout",
        // an SSE stream that broke mid-response: a transport failure, a
        // corrupted event, or a chunk or tool call that does not decode
        "transport error",
        "parse error",
        "utf8 error",
        "decode response chunk",
        "decode tool arguments",
        "server returned 408",
        "server returned 429",
        "server returned 500",
        "server returned 502",
        "server returned 503",
        "server returned 504",
    ]
    .iter()
    .any(|needle| error.contains(needle))
}

fn bail_tool_denied(name: &str) -> Result<crate::tool::ToolResult> {
    bail!("tool {name} was denied")
}

#[derive(Default)]
struct ToolDraft {
    id: String,
    name: String,
    arguments: String,
}

async fn collect(
    mut stream: crate::provider::ResponseStream,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> Result<(
    String,
    String,
    Vec<ToolCall>,
    Option<Usage>,
    Vec<serde_json::Value>,
    Option<String>,
)> {
    let mut reasoning = String::new();
    let mut text = String::new();
    let mut calls: Vec<ToolDraft> = Vec::new();
    let mut usage = None;
    let mut response_items = Vec::new();
    let mut truncated = None;
    let mut started = None;
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        if started.is_none() {
            started = Some(Instant::now());
            events.send(Event::ResponseStarted).await.ok();
        }
        match delta {
            ResponseDelta::Reasoning(delta) => {
                reasoning.push_str(&delta);
                events.send(Event::ReasoningDelta(delta)).await.ok();
            }
            ResponseDelta::Text(delta) => {
                text.push_str(&delta);
                events.send(Event::TextDelta(delta)).await.ok();
            }
            ResponseDelta::Usage(tokens) => usage = Some(tokens),
            ResponseDelta::OutputItem(item) => response_items.push(item),
            ResponseDelta::Truncated(reason) => truncated = Some(reason),
            ResponseDelta::Completed => {}
            ResponseDelta::ToolCall {
                index,
                id,
                name,
                arguments,
            } => {
                while calls.len() <= index {
                    calls.push(ToolDraft::default());
                }
                let call = &mut calls[index];
                if let Some(id) = id {
                    call.id = id;
                }
                if let Some(name) = name {
                    call.name.push_str(&name);
                }
                call.arguments.push_str(&arguments);
                events
                    .send(Event::ToolCallDelta {
                        index,
                        name: (!call.name.is_empty()).then(|| call.name.clone()),
                        arguments,
                    })
                    .await
                    .ok();
            }
        }
    }
    if let Some(tokens) = usage {
        if let Some(started) = started {
            events
                .send(Event::ModelResponseFinished {
                    output_tokens: tokens.total_tokens.saturating_sub(tokens.prompt_tokens),
                    duration: started.elapsed(),
                })
                .await
                .ok();
        }
        internal.send(InternalEvent::Usage(tokens)).await?;
    }
    let calls = calls
        .into_iter()
        .map(|call| {
            Ok(ToolCall {
                id: call.id,
                name: call.name,
                arguments: serde_json::from_str(&call.arguments)
                    .context("decode tool arguments")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((reasoning, text, calls, usage, response_items, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Provider, ResponseDelta, ResponseStream, mock::MockProvider};
    use crate::tool::{
        Approval, ShellCancelTool, ShellJobManager, ShellPollTool, ShellTool, Tool, ToolResult,
    };
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio::sync::Barrier;
    /// The built-in assistant, for call sites that pass an agent by hand.
    fn assistant_agent() -> Agent {
        crate::agent::assistant()
    }

    /// A catalog holding only the built-in assistant.
    fn test_catalog() -> Arc<AgentCatalog> {
        Arc::new(AgentCatalog::builtin())
    }

    /// A delegation port whose receiver is dropped immediately: delegation
    /// calls in these tests fail fast with a clear error, never hang.
    fn delegation_port() -> crate::tool::DelegationPort {
        let (port, _rx) = mpsc::unbounded_channel();
        port
    }

    fn no_steers() -> SteerQueue {
        Arc::new(Mutex::new(Vec::new()))
    }

    fn fresh_progress() -> TurnProgressHandle {
        Arc::new(Mutex::new(TurnProgress::default()))
    }

    struct Echo;

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo a value"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn run(&self, args: Value) -> Result<ToolResult> {
            Ok(ToolResult {
                is_error: false,
                output: args["value"].as_str().unwrap().to_owned(),
                image: None,
                file: None,
                diff: None,
            })
        }
    }

    struct SlowEcho(Vec<String>);

    #[async_trait]
    impl Tool for SlowEcho {
        fn name(&self) -> &str {
            "slow_echo"
        }
        fn description(&self) -> &str {
            "echo in chunks"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn run(&self, _args: Value) -> Result<ToolResult> {
            self.run_streamed(_args, None, usize::MAX).await
        }
        async fn run_streamed(
            &self,
            _args: Value,
            sink: Option<mpsc::UnboundedSender<String>>,
            _max_output_bytes: usize,
        ) -> Result<ToolResult> {
            let mut output = String::new();
            for chunk in &self.0 {
                if let Some(sink) = &sink {
                    sink.send(chunk.clone()).ok();
                }
                output.push_str(chunk);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Ok(ToolResult {
                is_error: false,
                output,
                image: None,
                file: None,
                diff: None,
            })
        }
    }

    #[tokio::test]
    async fn runtime_streams_mock_response_without_a_terminal() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Reasoning("thinking".into()),
            ResponseDelta::Text("hel".into()),
            ResponseDelta::Text("lo".into()),
            ResponseDelta::OutputItem(json!({
                "id": "rs_1",
                "type": "reasoning",
                "summary": [],
                "encrypted_content": "opaque",
            })),
        ]]));
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (internal_tx, _internal_rx) = mpsc::channel(2);
        let (completed, _) = agent(
            provider,
            &ToolRegistry::default(),
            &Config::default(),
            vec![Message::user("hi".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();
        assert_eq!(completed.last().unwrap().content(), "hello");
        assert!(matches!(
            completed.last().unwrap(),
            Message::Assistant { reasoning, .. } if reasoning == "thinking"
        ));
        assert!(matches!(
            completed.last().unwrap(),
            Message::Assistant { response_items, .. }
                if response_items[0]["encrypted_content"] == "opaque"
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ModelRequestStarted(_))
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ResponseHeadersReceived)
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ResponseStarted)
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ReasoningDelta(reasoning)) if reasoning == "thinking"
        ));
    }

    #[tokio::test]
    async fn runtime_reports_streamed_usage() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Text("done".into()),
            ResponseDelta::Usage(Usage {
                prompt_tokens: 200,
                total_tokens: 321,
            }),
        ]]));
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (internal_tx, mut internal_rx) = mpsc::channel(2);

        agent(
            provider,
            &ToolRegistry::default(),
            &Config::default(),
            vec![Message::user("hi".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        assert!(matches!(
            internal_rx.recv().await,
            Some(InternalEvent::Usage(Usage {
                prompt_tokens: 200,
                total_tokens: 321
            }))
        ));
        while let Some(event) = event_rx.recv().await {
            if let Event::ModelResponseFinished {
                output_tokens,
                duration,
            } = event
            {
                assert_eq!(output_tokens, 121);
                assert!(!duration.is_zero());
                break;
            }
        }
    }

    #[tokio::test]
    async fn runtime_executes_tool_calls_and_returns_to_model() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("echo".into()),
                    arguments: "{\"value\":\"".into(),
                },
                ResponseDelta::ToolCall {
                    index: 0,
                    id: None,
                    name: None,
                    arguments: "done\"}".into(),
                },
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (internal_tx, _internal_rx) = mpsc::channel(2);
        let (completed, _) = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        assert_eq!(completed.len(), 4);
        assert!(matches!(&completed[2], Message::Tool { content, .. } if content == "done"));
        assert_eq!(completed[3].content(), "finished");

        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ModelRequestStarted(_))
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ResponseHeadersReceived)
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ResponseStarted)
        ));
        assert!(
            matches!(event_rx.recv().await, Some(Event::ToolCallDelta { arguments, .. }) if arguments == "{\"value\":\"")
        );
        assert!(
            matches!(event_rx.recv().await, Some(Event::ToolCallDelta { arguments, .. }) if arguments == "done\"}")
        );
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ToolCallFinished { .. })
        ));
    }

    /// Finishes only once both of its calls are in flight, and then in the
    /// reverse of the order they were made: a batch runs at the same time,
    /// while its results stay in the order the model asked for.
    struct Raced(Arc<Barrier>);

    #[async_trait]
    impl Tool for Raced {
        fn name(&self) -> &str {
            "raced"
        }
        fn description(&self) -> &str {
            "wait for the batch, then answer after a delay"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn run(&self, args: Value) -> Result<ToolResult> {
            self.0.wait().await;
            tokio::time::sleep(Duration::from_millis(
                args["delay"].as_u64().unwrap_or_default(),
            ))
            .await;
            Ok(ToolResult {
                is_error: false,
                output: args["value"].as_str().unwrap_or_default().to_owned(),
                image: None,
                file: None,
                diff: None,
            })
        }
    }

    #[tokio::test]
    async fn a_batch_of_tool_calls_runs_at_once_and_reports_in_call_order() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("raced".into()),
                    arguments: r#"{"value":"first","delay":60}"#.into(),
                },
                ResponseDelta::ToolCall {
                    index: 1,
                    id: Some("call_2".into()),
                    name: Some("raced".into()),
                    arguments: r#"{"value":"second","delay":0}"#.into(),
                },
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(Raced(Arc::new(Barrier::new(2))), Approval::Allow);
        let (event_tx, event_rx) = mpsc::channel(16);
        // Never read: drop the receiver so event sends fail fast instead
        // of blocking once the buffer fills.
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(16);

        // Neither call passes its barrier before the other one starts, so a
        // loop that ran the calls one at a time would wait forever.
        let (completed, _) = tokio::time::timeout(
            Duration::from_secs(5),
            agent(
                provider,
                &tools,
                &Config::default(),
                vec![Message::user("go".into())],
                0,
                0,
                None,
                &no_steers(),
                &fresh_progress(),
                &event_tx,
                &internal_tx,
                &assistant_agent(),
                "test",
                "turn",
                &delegation_port(),
                &test_catalog(),
            ),
        )
        .await
        .expect("both calls run at the same time")
        .unwrap();

        // The second call answered first, yet the transcript and the next
        // model request keep the order the calls were made in.
        assert_eq!(completed.len(), 5);
        assert!(
            matches!(&completed[2], Message::Tool { call_id, content, .. }
                if call_id == "call_1" && content == "first")
        );
        assert!(
            matches!(&completed[3], Message::Tool { call_id, content, .. }
                if call_id == "call_2" && content == "second")
        );
    }

    #[tokio::test]
    async fn a_batch_of_asking_calls_is_approved_one_at_a_time() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("echo".into()),
                    arguments: r#"{"value":"one"}"#.into(),
                },
                ResponseDelta::ToolCall {
                    index: 1,
                    id: Some("call_2".into()),
                    name: Some("echo".into()),
                    arguments: r#"{"value":"two"}"#.into(),
                },
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Ask);
        let root = std::env::temp_dir().join(format!(
            "rope-parallel-approvals-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create_in(root.clone(), "ask".into())
            .await
            .unwrap();
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let task = tokio::spawn({
            let provider = provider.clone();
            async move {
                run(
                    Config::default(),
                    provider,
                    tools,
                    session,
                    Vec::new(),
                    ProjectState::new().await.unwrap(),
                    test_catalog(),
                    delegation_port(),
                    command_rx,
                    event_tx,
                )
                .await
            }
        });
        command_tx
            .send(Command::Submit(UserPrompt {
                content: "go".into(),
                images: Vec::new(),
            }))
            .await
            .unwrap();

        // A session carries one pending approval at a time, so calls that
        // need asking take turns asking: none of them is silently denied
        // because another call happened to be asking already.
        let mut asked = 0;
        while let Some(event) = event_rx.recv().await {
            match event {
                Event::ApprovalRequested { .. } => {
                    asked += 1;
                    command_tx
                        .send(Command::Approve(ApprovalDecision::AllowOnce))
                        .await
                        .unwrap();
                }
                Event::GenerationFinished { .. } => break,
                _ => {}
            }
        }
        assert_eq!(asked, 2, "every call in the batch was asked about");

        let (reply, _summary) = oneshot::channel();
        command_tx.send(Command::Shutdown(reply)).await.unwrap();
        task.await.unwrap();
        let (_, messages) = Session::resume_in(root.clone(), "ask").await.unwrap();
        assert!(matches!(&messages[2], Message::Tool { content, .. } if content == "one"));
        assert!(matches!(&messages[3], Message::Tool { content, .. } if content == "two"));

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn an_unknown_tool_answers_the_model_and_the_turn_goes_on() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("echo".into()),
                    arguments: r#"{"value":"one"}"#.into(),
                },
                ResponseDelta::ToolCall {
                    index: 1,
                    id: Some("call_2".into()),
                    name: Some("not_registered".into()),
                    arguments: json!({}).to_string(),
                },
                ResponseDelta::ToolCall {
                    index: 2,
                    id: Some("call_3".into()),
                    name: Some("echo".into()),
                    arguments: r#"{"value":"three"}"#.into(),
                },
            ],
            vec![ResponseDelta::Text("recovered".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let (event_tx, event_rx) = mpsc::channel(16);
        // Never read: drop the receiver so event sends fail fast instead
        // of blocking once the buffer fills.
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(16);
        let progress = fresh_progress();
        let (completed, _) = agent(
            provider.clone(),
            &tools,
            &Config::default(),
            vec![Message::user("go".into())],
            0,
            0,
            None,
            &no_steers(),
            &progress,
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .expect("an unknown tool answers as a failed result, not a failed turn");

        // The whole batch is answered in call order; the call that names
        // no tool answers with its error like any other failed tool, and
        // the turn keeps going so the model can correct the name.
        let results = completed
            .iter()
            .filter_map(|message| match message {
                Message::Tool {
                    call_id, content, ..
                } => Some((call_id.as_str(), content.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            results
                .iter()
                .map(|(call_id, _)| *call_id)
                .collect::<Vec<_>>(),
            ["call_1", "call_2", "call_3"],
            "every call of the batch is answered, in call order"
        );
        assert_eq!(results[0].1, "one");
        assert_eq!(results[2].1, "three");
        assert!(
            results[1]
                .1
                .starts_with("Error: unknown tool: not_registered")
        );
        // The model saw the failure: the retried request carried the
        // error result, and the follow-up response finished the turn.
        assert_eq!(provider.requests().len(), 2);
        let requests = provider.requests();
        assert!(
            requests[1].messages.iter().any(|message| matches!(
                message,
                Message::Tool { content, .. } if content.starts_with("Error: unknown tool")
            )),
            "the retried request carries the failed call's error result"
        );
        assert!(
            matches!(&completed[completed.len() - 1], Message::Assistant { content, .. }
            if content == "recovered")
        );
    }

    #[tokio::test]
    async fn a_broken_response_stream_retries_from_scratch() {
        let provider = Arc::new(MockProvider::falling(vec![
            vec![
                Ok(ResponseDelta::Text("half a thought".into())),
                Err(anyhow::anyhow!("transport error: connection closed")),
            ],
            vec![
                Ok(ResponseDelta::Text("clean answer".into())),
                Ok(ResponseDelta::Completed),
            ],
        ]));
        let tools = ToolRegistry::default();
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let collected = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = event_rx.recv().await {
                events.push(event);
            }
            events
        });
        let (internal_tx, _internal_rx) = mpsc::channel(16);
        let (completed, _) = agent(
            provider.clone(),
            &tools,
            &Config::default(),
            vec![Message::user("go".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .expect("a broken stream is retried, not a failed turn");
        drop(event_tx);
        let events = collected.await.unwrap();

        // The partial response is discarded where the stream broke and
        // the request starts over with the same backoff a connection
        // failure gets.
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ResponseDiscarded)),
            "the client is told the partial response is dropped"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Retrying { seconds: 2 })),
            "the retry keeps the capped backoff notice"
        );
        // The retry asks from scratch: the broken answer is in no message
        // of the transcript, and the retried request carries only the
        // prompt it started with.
        assert_eq!(provider.requests().len(), 2);
        assert_eq!(provider.requests()[1].messages.len(), 1);
        assert!(
            completed.iter().all(|message| !matches!(
                message,
                Message::Assistant { content, .. }
                    if content.contains("half a thought")
            )),
            "the partial answer never reaches the conversation"
        );
        assert!(
            matches!(&completed[completed.len() - 1], Message::Assistant { content, .. }
            if content == "clean answer")
        );
    }

    #[tokio::test]
    async fn an_unrecoverable_response_error_still_fails_the_turn() {
        let provider = Arc::new(MockProvider::falling(vec![vec![
            Ok(ResponseDelta::Text("thinking out loud".into())),
            Err(anyhow::anyhow!("server returned 400: invalid request")),
        ]]));
        let tools = ToolRegistry::default();
        let (event_tx, event_rx) = mpsc::channel(16);
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(16);
        let error = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("go".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .err()
        .expect("a permanent response error still fails the turn");
        assert!(error.to_string().contains("server returned 400"));
    }

    #[tokio::test]
    async fn a_batch_reports_the_first_call_the_turn_could_not_run() {
        // A call needing approval and a call that can run. The client is
        // gone, so the approval cannot be delivered: a call the turn
        // cannot run at all, which still stops the turn even though
        // unknown tool names no longer do.
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_1".into()),
                name: Some("asking".into()),
                arguments: json!({}).to_string(),
            },
            ResponseDelta::ToolCall {
                index: 1,
                id: Some("call_2".into()),
                name: Some("echo".into()),
                arguments: r#"{"value":"ran"}"#.into(),
            },
        ]]));
        let mut tools = ToolRegistry::default();
        tools.insert(Asking, Approval::Ask);
        tools.insert(Echo, Approval::Allow);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let collected = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = event_rx.recv().await {
                events.push(event);
            }
            events
        });
        let (internal_tx, internal_rx) = mpsc::channel(16);
        // The internal events go nowhere: the approval send fails, and the
        // turn names the call it could not run.
        drop(internal_rx);
        let error = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("go".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .err()
        .expect("a call whose approval cannot be delivered fails the turn");
        drop(event_tx);
        let events = collected.await.unwrap();

        // The turn stops with the first call it could not run, in the
        // order the model made them, not with whichever finished last.
        let reported = error.to_string();
        assert!(
            reported.contains("asking"),
            "the reported error names the first call: {reported}"
        );
        // Every call of the batch is answered exactly once, so a client
        // shows no call still waiting after the turn has ended.
        let mut answered = events
            .iter()
            .filter_map(|event| match event {
                Event::ToolResult {
                    call_id, success, ..
                } => Some((call_id.clone(), *success)),
                _ => None,
            })
            .collect::<Vec<_>>();
        answered.sort();
        assert_eq!(
            answered,
            [("call_1".to_string(), false), ("call_2".to_string(), true)],
            "one result per call, the ones that could not run marked failed"
        );
    }

    #[tokio::test]
    async fn a_denied_call_in_a_batch_does_not_stop_the_others() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("denied".into()),
                    arguments: json!({}).to_string(),
                },
                ResponseDelta::ToolCall {
                    index: 1,
                    id: Some("call_2".into()),
                    name: Some("echo".into()),
                    arguments: r#"{"value":"ran"}"#.into(),
                },
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(Denied, Approval::Deny);
        tools.insert(Echo, Approval::Allow);
        let (event_tx, event_rx) = mpsc::channel(16);
        // Never read: drop the receiver so event sends fail fast instead
        // of blocking once the buffer fills.
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(16);
        let (completed, _) = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("go".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        // A denied call is a failed result for that call alone.
        assert!(matches!(&completed[2], Message::Tool { content, .. }
            if content == "Error: tool denied was denied"));
        assert!(matches!(&completed[3], Message::Tool { content, .. } if content == "ran"));
        assert_eq!(completed[4].content(), "finished");
    }

    /// A call the user refuses: it answers with a failed result, and the
    /// tool itself is never reached.
    struct Denied;

    #[async_trait]
    impl Tool for Denied {
        fn name(&self) -> &str {
            "denied"
        }
        fn description(&self) -> &str {
            "never allowed"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn run(&self, _args: Value) -> Result<ToolResult> {
            unreachable!("a denied call never reaches the tool")
        }
    }

    /// A call whose approval can never be delivered, used to make a call
    /// the turn cannot run at all.
    struct Asking;

    #[async_trait]
    impl Tool for Asking {
        fn name(&self) -> &str {
            "asking"
        }
        fn description(&self) -> &str {
            "always asks first"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn run(&self, _args: Value) -> Result<ToolResult> {
            unreachable!("the approval never arrives")
        }
    }

    /// A tool that answers at once with more output than a small context
    /// can hold, so a batch of it shows what each call's share of the
    /// context allows.
    struct Loud;

    #[async_trait]
    impl Tool for Loud {
        fn name(&self) -> &str {
            "loud"
        }
        fn description(&self) -> &str {
            "answer with a long fixed output"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        async fn run(&self, _args: Value) -> Result<ToolResult> {
            Ok(ToolResult {
                is_error: false,
                output: "a".repeat(4_000),
                image: None,
                file: None,
                diff: None,
            })
        }
    }

    /// The budgets one batch would be planned with, without running it.
    async fn planned_budgets(
        config: &Config,
        tools: &ToolRegistry,
        calls: &[ToolCall],
        used: u64,
    ) -> Vec<ToolBudget> {
        let provider = Arc::new(MockProvider::new(Vec::new()));
        let (event_tx, event_rx) = mpsc::channel(16);
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(16);
        let mut messages = vec![Message::user("go".into())];
        messages.extend(calls.iter().map(|call| {
            Message::assistant_response(
                String::new(),
                config.model_id().to_owned(),
                String::new(),
                vec![call.clone()],
                Vec::new(),
            )
        }));
        let mut compaction = None;
        let mut used = used;
        tool_output_budgets(
            provider,
            tools,
            config,
            &mut messages,
            0,
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            None,
            &assistant_agent(),
            &mut compaction,
            &mut used,
            calls,
        )
        .await
        .expect("the batch fits the context it was planned for")
    }

    fn loud_calls(count: usize) -> Vec<ToolCall> {
        (1..=count)
            .map(|index| ToolCall {
                id: format!("call_{index}"),
                name: "loud".into(),
                arguments: "{}".into(),
            })
            .collect()
    }

    #[tokio::test]
    async fn a_whole_batch_cannot_spend_the_context_the_turn_keeps_free() {
        let mut tools = ToolRegistry::default();
        tools.insert(Loud, Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 1_024;
        // The context is the same in both cases; only the number of calls
        // in the batch differs. Six calls is well past the point where one
        // fifth of the room for each would add up to more than the turn
        // keeps free, so their equal share is the smaller bound.
        let small = planned_budgets(&config, &tools, &loud_calls(2), 100).await;
        let large = planned_budgets(&config, &tools, &loud_calls(6), 100).await;
        let framing = tool_message_overhead(&loud_calls(1)[0]);
        let max = config.active_model().max_context_tokens;
        // The room is the context that is left; the window is the part of it
        // a turn keeps before it would compact. Both are counted after the
        // framing of the batch's own result messages.
        let room = |count: u64| max - 100 - framing * count;
        let window = |count: u64| {
            ((max as f64 * config.compaction_threshold as f64) as u64)
                .saturating_sub(100)
                .saturating_sub(framing * count)
        };
        let plan = |budgets: &[ToolBudget]| {
            budgets
                .iter()
                .map(|budget| budget.message_budget)
                .collect::<Vec<_>>()
        };

        // Two calls each keep one fifth of the room, the cap one call has
        // always been given: the share of a batch is not what bounds them.
        assert_eq!(plan(&small), [framing + room(2) / 5; 2]);
        // Six calls divide the window the turn keeps free between them.
        assert_eq!(plan(&large), [framing + window(6) / 6; 6]);
        assert!(window(6) / 6 < room(6) / 5, "the share is the bound");
        // Which is what keeps the whole batch inside the window: a fifth of
        // the room for each of them would have been more than that.
        let whole: u64 = plan(&large).iter().sum();
        assert!(whole <= window(6) + framing * 6);
    }

    #[tokio::test]
    async fn a_batch_in_a_full_context_still_gets_a_share_of_the_room() {
        let mut tools = ToolRegistry::default();
        tools.insert(Loud, Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 1_024;
        // The turn is already at the point where its next request would
        // compact, so no truncation can keep this batch under that line.
        // Dividing the compaction reserve then leaves every call a control
        // envelope and nothing else, for no benefit: the calls share the
        // room that is left instead.
        let calls = loud_calls(6);
        let framing = tool_message_overhead(&calls[0]);
        let used = 700;
        let budgets = planned_budgets(&config, &tools, &calls, used).await;
        let room = config.active_model().max_context_tokens - used - framing * 6;
        assert_eq!(
            budgets
                .iter()
                .map(|budget| budget.message_budget)
                .collect::<Vec<_>>(),
            [framing + room / 6; 6]
        );
        assert!(
            budgets
                .iter()
                .all(|budget| budget.message_budget > framing + MIN_TOOL_OUTPUT_TOKENS),
            "a divided room is more than the control floor"
        );
        let whole: u64 = budgets.iter().map(|budget| budget.message_budget).sum();
        assert!(used + whole <= config.active_model().max_context_tokens);
    }

    #[tokio::test]
    async fn the_largest_allowed_batch_stays_under_the_compaction_point() {
        let mut tools = ToolRegistry::default();
        tools.insert(Loud, Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 128_000;
        let calls = loud_calls(MAX_TOOL_CALLS_PER_MESSAGE);
        let used = 10_000;
        let budgets = planned_budgets(&config, &tools, &calls, used).await;
        assert_eq!(budgets.len(), calls.len());
        let whole: u64 = budgets.iter().map(|budget| budget.message_budget).sum();
        // Every call of the batch at the ceiling it was given still leaves
        // the next model request under the point where the turn compacts.
        assert!(
            used + whole
                <= (config.active_model().max_context_tokens as f64
                    * config.compaction_threshold as f64) as u64
        );
        // And no call is cut below what it needs to be actionable.
        assert!(
            budgets
                .iter()
                .all(|budget| budget.message_budget >= MIN_TOOL_OUTPUT_TOKENS)
        );
    }

    #[tokio::test]
    async fn a_batch_that_does_not_fit_the_context_is_compacted_once_first() {
        // Three envelope-shaped results, and a context with room for none of
        // them: the older history is summarized first, once, and the whole
        // batch is then planned against the room that frees.
        let mut first = Vec::new();
        for index in 0..3u8 {
            first.push(ResponseDelta::ToolCall {
                index: usize::from(index),
                id: Some(format!("call_{}", index + 1)),
                name: Some("shell".into()),
                arguments: r#"{"command":"sleep 5","yield_time_ms":50}"#.into(),
            });
        }
        first.push(ResponseDelta::Usage(Usage {
            prompt_tokens: 0,
            total_tokens: 1_014,
        }));
        let provider = Arc::new(MockProvider::new(vec![
            first,
            // The mid-turn compaction request: summarize the earlier
            // conversation.
            vec![ResponseDelta::Text("Old work done.".into())],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        let jobs = ShellJobManager::new(std::env::temp_dir());
        tools.insert(ShellTool(jobs.clone()), Approval::Allow);
        tools.insert(ShellPollTool(jobs), Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 1_024;
        let (event_tx, event_rx) = mpsc::channel(16);
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(16);
        let (completed, compaction) = agent(
            provider.clone(),
            &tools,
            &config,
            vec![
                Message::user("old request".into()),
                Message::assistant(
                    "old reply".into(),
                    "model".into(),
                    String::new(),
                    Vec::new(),
                ),
                Message::user("run them".into()),
            ],
            2,
            7,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        let compaction = compaction.expect("the batch is planned after one compaction");
        assert_eq!(compaction.summary, "Old work done.");
        // The summary request is the model request right after the batch was
        // asked for, and it does not carry the batch itself: the room was
        // made before any of the calls ran, not after their results arrived.
        let requests = provider.requests();
        assert!(
            !requests[1]
                .messages
                .iter()
                .any(|message| matches!(message, Message::Tool { .. })),
            "the context was made for the batch after it had already run"
        );
        // The prompts and the call the turn is waiting on stay together:
        // what is summarized is the older conversation only.
        assert!(matches!(&completed[0], Message::User { content, .. }
            if content == "run them"));
        // Every call kept its control envelope, which is what the floor of
        // a share is there for: a backgrounded job whose job_id was cut off
        // is a job the model cannot poll.
        for (index, message) in completed[2..5].iter().enumerate() {
            assert!(
                matches!(message, Message::Tool { call_id, content, .. }
                    if *call_id == format!("call_{}", index + 1)
                        && content.starts_with("status: running\n")
                        && content.contains("job_id: shell-")),
                "call {} lost the control fields of its result: {message:?}",
                index + 1
            );
        }
        // The whole batch fits the context it was planned for, counted the
        // way a request is counted: with the tool definitions it carries.
        let requests = provider.requests();
        assert_eq!(
            requests.len(),
            3,
            "the batch, the summary, and the answer: no second compaction"
        );
        let last = requests.last().expect("the batch was answered");
        assert!(
            available_context_tokens(&last.messages, &tools, &config, None, &assistant_agent(),)
                > 0,
            "the next model request must fit the context"
        );
    }

    #[tokio::test]
    async fn runtime_streams_tool_output_deltas_before_the_result() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_1".into()),
                name: Some("slow_echo".into()),
                arguments: "{}".into(),
            }],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(
            SlowEcho(vec!["al".into(), "pha".into(), "beta".into()]),
            Approval::Allow,
        );
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (internal_tx, _internal_rx) = mpsc::channel(2);

        let (completed, _) = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        assert!(matches!(
            &completed[2],
            Message::Tool { content, .. } if content == "alphabeta"
        ));

        let events = tokio::time::timeout(Duration::from_secs(5), async {
            let mut events = Vec::new();
            while let Some(event) = event_rx.recv().await {
                let done = matches!(&event, Event::ToolResult { .. });
                events.push(event);
                if done {
                    break;
                }
            }
            events
        })
        .await
        .expect("tool result event");

        let sequence: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                Event::ToolStarted { call_id } if call_id == "call_1" => Some("started"),
                Event::ToolOutputDelta { call_id, .. } if call_id == "call_1" => Some("delta"),
                Event::ToolResult { call_id, .. } if call_id == "call_1" => Some("result"),
                _ => None,
            })
            .collect();
        assert_eq!(sequence, ["started", "delta", "delta", "delta", "result"]);
        let streamed: String = events
            .iter()
            .filter_map(|event| match event {
                Event::ToolOutputDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(streamed, "alphabeta");
    }

    #[tokio::test]
    async fn tool_output_streaming_stops_at_the_truncation_cap() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("slow_echo".into()),
                    arguments: "{}".into(),
                },
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 0,
                    total_tokens: 152,
                }),
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(SlowEcho(vec!["12345678".into(); 32]), Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 200;
        config.compaction_threshold = 1.0;
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (internal_tx, mut internal_rx) = mpsc::channel(2);
        // Drain events while the agent runs: with enough tool output deltas
        // the bounded channel would fill and block the agent's senders.
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = event_rx.recv().await {
                events.push(event);
            }
            events
        });

        agent(
            provider,
            &tools,
            &config,
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();
        drop(event_tx);
        let events = collector.await.unwrap();

        internal_rx.recv().await;
        let mut streamed = String::new();
        for event in &events {
            if let Event::ToolOutputDelta { delta, .. } = event {
                streamed.push_str(delta);
            }
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::ToolResult { .. })),
            "expected a tool result event"
        );
        // The budget is the 48 remaining tokens minus the 16-token Tool
        // message framing: the 32-token control minimum — 128 bytes, so
        // exactly sixteen eight-byte chunks are forwarded, and
        // 152 + 16 + 32 hits the 200-token limit exactly without
        // exceeding it.
        assert_eq!(streamed, "12345678".repeat(16));
    }

    #[tokio::test]
    async fn streaming_cap_slices_an_oversized_delta() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("slow_echo".into()),
                    arguments: "{}".into(),
                },
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 0,
                    total_tokens: 152,
                }),
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        // One 1000-byte delta, far larger than the 128-byte budget.
        tools.insert(SlowEcho(vec!["a".repeat(1_000)]), Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 200;
        config.compaction_threshold = 1.0;
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let (internal_tx, _internal_rx) = mpsc::channel(2);
        let collector = tokio::spawn(async move {
            let mut events = Vec::new();
            while let Some(event) = event_rx.recv().await {
                events.push(event);
            }
            events
        });

        agent(
            provider,
            &tools,
            &config,
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();
        drop(event_tx);
        let events = collector.await.unwrap();

        let streamed: String = events
            .iter()
            .filter_map(|event| match event {
                Event::ToolOutputDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        // The budget is the 48 remaining tokens minus the Tool message
        // framing: 128 bytes. The final delta is sliced to the remaining
        // allowance, so the cap holds exactly.
        assert_eq!(streamed, "a".repeat(128));
    }

    #[tokio::test]
    async fn tool_call_cap_resets_for_each_assistant_message() {
        let response: Vec<ResponseDelta> = (0..65)
            .map(|index| ResponseDelta::ToolCall {
                index,
                id: Some(format!("call_{index}")),
                name: Some("echo".into()),
                arguments: format!(r#"{{"value":"v{index}"}}"#),
            })
            .collect();
        let provider = Arc::new(MockProvider::new(vec![
            response,
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let (event_tx, _) = mpsc::channel(512);
        let (internal_tx, _) = mpsc::channel(2);
        let (completed, _) = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        // user, assistant, the capped tool results, final assistant
        assert_eq!(completed.len(), 1 + 1 + MAX_TOOL_CALLS_PER_MESSAGE + 1);
        let Message::Assistant { tool_calls, .. } = &completed[1] else {
            panic!("expected assistant message");
        };
        assert_eq!(tool_calls.len(), MAX_TOOL_CALLS_PER_MESSAGE);
        assert_eq!(tool_calls.last().unwrap().id, "call_63");
        assert_eq!(completed[2].content(), "v0");
        assert_eq!(completed[3].content(), "v1");
    }

    #[tokio::test]
    async fn one_turn_may_run_more_model_turns_than_the_tool_call_cap() {
        let response = vec![ResponseDelta::ToolCall {
            index: 0,
            id: Some("call".into()),
            name: Some("echo".into()),
            arguments: r#"{"value":"done"}"#.into(),
        }];
        let responses = std::iter::repeat_with(|| response.clone())
            .take(2 * MAX_TOOL_CALLS_PER_MESSAGE)
            .chain(std::iter::once(vec![ResponseDelta::Text(
                "finished".into(),
            )]))
            .collect();
        let provider = Arc::new(MockProvider::new(responses));
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let (event_tx, _) = mpsc::channel(4096);
        let (internal_tx, _) = mpsc::channel(2);
        let (completed, _) = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        // user, 2 * cap assistant/tool message pairs, final assistant
        assert_eq!(completed.len(), 1 + 2 * MAX_TOOL_CALLS_PER_MESSAGE * 2 + 1);
        assert_eq!(completed.last().unwrap().content(), "finished");
    }

    #[test]
    fn tool_output_is_truncated_on_a_character_boundary() {
        let output = truncate_tool_output("call_1", None, None, "é".repeat(100), 30);

        assert!(output.ends_with("[tool output truncated]"));
        // The completed message — framing, escaped content, and marker —
        // meets the budget on a character boundary.
        let message = Message::tool("call_1".into(), output, None, None);
        assert!(estimate_tokens(std::slice::from_ref(&message)) <= 30);
    }

    #[test]
    fn tool_output_truncation_counts_json_escaping() {
        // A thousand newlines: the escaped form doubles, so a raw-bytes
        // budget would let the completed message exceed the limit.
        // Measuring the populated message does not.
        let output = truncate_tool_output("call_1", None, None, "\n".repeat(1_000), 32);

        let message = Message::tool("call_1".into(), output, None, None);
        assert!(estimate_tokens(std::slice::from_ref(&message)) <= 32);
    }

    #[tokio::test]
    async fn tool_output_is_limited_to_a_fifth_of_available_context() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("echo".into()),
                    arguments: format!(r#"{{"value":"{}"}}"#, "x".repeat(1_000)),
                },
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 20,
                    total_tokens: 50,
                }),
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 100;
        config.compaction_threshold = 1.0;
        let (event_tx, _event_rx) = mpsc::channel(32);
        let (internal_tx, mut internal_rx) = mpsc::channel(4);

        let (completed, _) = agent(
            provider,
            &tools,
            &config,
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();
        internal_rx.recv().await;

        assert!(matches!(
            &completed[2],
            // 100 - 50 = 50 remaining tokens, minus the 16-token Tool
            // message framing: 34, which floors to the 32-token control
            // minimum. The 48-token message budget holds the serialized
            // message — framing plus the escaped content and marker — to
            // 130 escaped bytes: 105 content characters and the marker,
            // where the framing-less old budget (50 + 128) would have
            // overflowed the 100-token limit.
            Message::Tool { content, .. }
                if content.ends_with("[tool output truncated]") && content.len() <= 129
        ));
    }

    #[tokio::test]
    async fn tool_output_budget_keeps_shell_control_fields_near_context_limit() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("shell".into()),
                    arguments: r#"{"command":"sleep 5","yield_time_ms":50}"#.into(),
                },
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 0,
                    total_tokens: 150,
                }),
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        let jobs = ShellJobManager::new(std::env::temp_dir());
        tools.insert(ShellTool(jobs.clone()), Approval::Allow);
        tools.insert(ShellPollTool(jobs), Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 200;
        config.compaction_threshold = 1.0;
        let (event_tx, event_rx) = mpsc::channel(16);
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(4);

        let (completed, _) = agent(
            provider.clone(),
            &tools,
            &config,
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        // 200 - 150 = 50 remaining tokens, minus the 16-token Tool message
        // framing: 34, floored to the 32-token control minimum. Without
        // the floor the 40-byte envelope would be truncated and lose the
        // job_id, and the command could never be polled or cancelled. The
        // floor keeps the control fields, and 150 + 16 + 32 = 198 stays
        // under the limit.
        assert!(matches!(
            &completed[2],
            Message::Tool { content, .. }
                if content.starts_with("status: running\n")
                    && content.contains("job_id: shell-1\n")
                    && !content.ends_with("[tool output truncated]")
        ));
        // The next model request — with the result's framing — fits.
        let requests = provider.requests();
        assert!(
            estimate_tokens(&requests.last().unwrap().messages)
                <= config.active_model().max_context_tokens
        );
    }

    #[tokio::test]
    async fn mid_turn_compaction_summarizes_the_first_turn_and_repeats() {
        let mut responses = Vec::new();
        for index in 1..=2 {
            responses.push(vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some(format!("call_{index}")),
                    name: Some("echo".into()),
                    arguments: json!({"value": format!("result {index}")}).to_string(),
                },
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 3_000,
                    total_tokens: 3_200,
                }),
            ]);
            responses.push(vec![
                ResponseDelta::Text(format!("summary {index}")),
                ResponseDelta::Completed,
            ]);
        }
        responses.push(vec![ResponseDelta::Text("finished".into())]);
        let provider = Arc::new(MockProvider::new(responses));
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 4_096;
        let (events, receiver) = mpsc::channel(1);
        drop(receiver);
        let (internal, _receiver) = mpsc::channel(8);
        let progress = fresh_progress();

        let (completed, compaction) = agent(
            provider.clone(),
            &tools,
            &config,
            vec![Message::user("keep working".into())],
            0,
            0,
            None,
            &no_steers(),
            &progress,
            &events,
            &internal,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        let requests = provider.requests();
        assert_eq!(requests.len(), 5);
        for index in 0..2 {
            let summary = &requests[index * 2 + 1];
            assert!(summary.tools.is_empty());
            assert!(summary.messages.iter().any(|message| {
                matches!(message, Message::Tool { content, .. } if content == &format!("result {}", index + 1))
            }));
            let continuation = &requests[index * 2 + 2];
            assert!(
                continuation.messages[0]
                    .content()
                    .contains(&format!("summary {}", index + 1))
            );
            assert!(
                !continuation
                    .messages
                    .iter()
                    .any(|message| matches!(message, Message::Tool { .. }))
            );
        }
        assert!(
            requests[3]
                .messages
                .iter()
                .any(|message| message.content().contains("summary 1"))
        );
        assert_eq!(completed.len(), 6);
        assert_eq!(completed[0].content(), "keep working");
        assert_eq!(completed.last().unwrap().content(), "finished");
        assert_eq!(progress.lock().unwrap().messages, completed);
        assert_eq!(compaction.unwrap().through, 5);
    }

    #[tokio::test]
    async fn tool_output_budget_compacts_mid_turn_and_never_exceeds_the_context() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("shell".into()),
                    arguments: r#"{"command":"sleep 5","yield_time_ms":50}"#.into(),
                },
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 0,
                    total_tokens: 1_014,
                }),
            ],
            // The mid-turn compaction request: summarize the earlier
            // conversation.
            vec![ResponseDelta::Text("Old work done.".into())],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        let jobs = ShellJobManager::new(std::env::temp_dir());
        tools.insert(ShellTool(jobs.clone()), Approval::Allow);
        tools.insert(ShellPollTool(jobs), Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 1_024;
        let (event_tx, event_rx) = mpsc::channel(16);
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(4);

        // Only 10 tokens remain — not even the 32-token control envelope
        // plus the Tool message framing fits — with an earlier
        // conversation to compact.
        let (completed, compaction) = agent(
            provider.clone(),
            &tools,
            &config,
            vec![
                Message::user("old request".into()),
                Message::assistant(
                    "old reply".into(),
                    "model".into(),
                    String::new(),
                    Vec::new(),
                ),
                Message::user("run it".into()),
            ],
            2,
            7,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        // The mid-turn compaction lands its marker at the turn's user
        // message and keeps the user, the call, and the result.
        let compaction = compaction.expect("expected a mid-turn compaction");
        assert_eq!(compaction.through, 7);
        assert_eq!(completed[0], Message::user("run it".into()));
        assert!(matches!(
            &completed[2],
            Message::Tool { content, .. }
                if content.starts_with("status: running\n")
                    && content.contains("job_id: shell-1\n")
                    && !content.ends_with("[tool output truncated]")
        ));
        // The next model request fits the context: before the fix, 128
        // bytes of content were delivered at 10 remaining tokens and the
        // full message (content + role + call id) pushed the request past
        // max_context_tokens while truncating away the job_id.
        let requests = provider.requests();
        assert!(
            estimate_tokens(&requests.last().unwrap().messages)
                <= config.active_model().max_context_tokens,
            "next model request must fit the context"
        );
    }

    #[tokio::test]
    async fn shell_poll_follows_shell_in_model_context() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_1".into()),
                name: Some("shell".into()),
                arguments: r#"{"command":"sleep 0.3; echo done","yield_time_ms":50}"#.into(),
            }],
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_2".into()),
                name: Some("shell_poll".into()),
                arguments: r#"{"job_id":"shell-1","yield_time_ms":2000}"#.into(),
            }],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        let jobs = ShellJobManager::new(std::env::temp_dir());
        tools.insert(ShellTool(jobs.clone()), Approval::Allow);
        tools.insert(ShellPollTool(jobs), Approval::Allow);
        let (event_tx, event_rx) = mpsc::channel(16);
        // Never read: drop the receiver so event sends fail fast instead
        // of blocking once the buffer fills.
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(2);
        let (completed, _) = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("go".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        // user, shell call, running result, poll call, finished result, final
        assert_eq!(completed.len(), 6);
        assert!(matches!(
            &completed[1],
            Message::Assistant { tool_calls, .. }
                if tool_calls[0].name == "shell"
        ));
        assert!(matches!(
            &completed[2],
            Message::Tool { call_id, content, .. }
                if call_id == "call_1"
                    && content.starts_with("status: running\n")
                    && content.contains("job_id: shell-1\n")
        ));
        assert!(matches!(
            &completed[3],
            Message::Assistant { tool_calls, .. }
                if tool_calls[0].name == "shell_poll"
        ));
        assert!(matches!(
            &completed[4],
            Message::Tool { call_id, content, .. }
                if call_id == "call_2"
                    && content.starts_with("status: finished\n")
                    && content.contains("exit_code: 0\n")
                    && content.contains("output:\ndone\n")
        ));
        assert_eq!(completed[5].content(), "finished");
    }

    #[tokio::test]
    async fn shell_poll_and_cancel_do_not_re_request_approval() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_1".into()),
                name: Some("shell".into()),
                arguments: r#"{"command":"sleep 5","yield_time_ms":50}"#.into(),
            }],
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_2".into()),
                name: Some("shell_poll".into()),
                arguments: r#"{"job_id":"shell-1","yield_time_ms":50}"#.into(),
            }],
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_3".into()),
                name: Some("shell_cancel".into()),
                arguments: r#"{"job_id":"shell-1"}"#.into(),
            }],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        let jobs = ShellJobManager::new(std::env::temp_dir());
        tools.insert(ShellTool(jobs.clone()), Approval::Ask);
        tools.insert(ShellPollTool(jobs.clone()), Approval::Allow);
        tools.insert(ShellCancelTool(jobs), Approval::Allow);
        let (event_tx, event_rx) = mpsc::channel(16);
        // Never read: drop the receiver so event sends fail fast instead
        // of blocking once the buffer fills.
        drop(event_rx);
        let (internal_tx, mut internal_rx) = mpsc::channel(16);
        let mut agent_task = {
            let provider = provider.clone();
            let tools = tools.clone();
            let config = Config::default();
            let events = event_tx.clone();
            let internal = internal_tx.clone();
            tokio::spawn(async move {
                agent(
                    provider,
                    &tools,
                    &config,
                    vec![Message::user("go".into())],
                    0,
                    0,
                    None,
                    &no_steers(),
                    &fresh_progress(),
                    &events,
                    &internal,
                    &assistant_agent(),
                    "test",
                    "turn",
                    &delegation_port(),
                    &test_catalog(),
                )
                .await
            })
        };
        let mut approvals = 0;
        let agent_result = loop {
            // biased: once the agent completes, never poll its handle again
            // while approval events are still queued (tokio panics on a
            // re-polled, completed JoinHandle).
            tokio::select! {
                biased;
                result = &mut agent_task => {
                    // Drain queued events before asserting on approvals.
                    while let Ok(event) = internal_rx.try_recv() {
                        if matches!(event, InternalEvent::Approval { .. }) {
                            approvals += 1;
                        }
                    }
                    break result;
                }
                Some(event) = internal_rx.recv() => {
                    if let InternalEvent::Approval { call, reply, .. } = event {
                        approvals += 1;
                        assert_eq!(call.name, "shell");
                        reply.send(ApprovalDecision::AllowOnce).ok();
                    }
                }
            }
        };
        assert_eq!(approvals, 1, "only the shell start may ask for approval");
        let (completed, _) = agent_result.unwrap().unwrap();

        // user, three assistant/tool pairs, final assistant
        assert_eq!(completed.len(), 8);
        assert!(matches!(
            &completed[2],
            Message::Tool { content, .. } if content.starts_with("status: running\n")
        ));
        assert!(matches!(
            &completed[4],
            Message::Tool { content, .. } if content.starts_with("status: running\n")
        ));
        assert!(matches!(
            &completed[6],
            Message::Tool { content, .. } if content.starts_with("status: cancelled\n")
        ));
        assert_eq!(completed[7].content(), "finished");
    }

    #[tokio::test]
    async fn cancel_active_kills_retained_shell_jobs() {
        let mut tools = ToolRegistry::default();
        let jobs = ShellJobManager::new(std::env::temp_dir());
        tools.insert(ShellTool(jobs.clone()), Approval::Allow);
        tools.insert(ShellPollTool(jobs), Approval::Allow);

        let start = tools
            .get("shell")
            .unwrap()
            .tool
            .run_streamed(
                json!({ "command": "sleep 5", "yield_time_ms": 50 }),
                None,
                usize::MAX,
            )
            .await
            .unwrap();
        assert!(start.output.starts_with("status: running\n"));

        let started = Instant::now();
        tools.cancel_active().await;
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cancellation must kill the child promptly"
        );

        // Escape never delivers the job to the model again, so the retained
        // job is reaped along with its worker.
        let gone = tools
            .get("shell_poll")
            .unwrap()
            .tool
            .run_streamed(json!({ "job_id": "shell-1" }), None, usize::MAX)
            .await
            .unwrap_err();
        assert!(gone.to_string().contains("unknown shell job"));
    }

    #[tokio::test]
    async fn turn_end_kills_retained_shell_jobs() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_1".into()),
                name: Some("shell".into()),
                arguments: r#"{"command":"sleep 5","yield_time_ms":50}"#.into(),
            }],
            vec![ResponseDelta::Text("done".into())],
        ]));
        let mut tools = ToolRegistry::default();
        let jobs = ShellJobManager::new(std::env::temp_dir());
        tools.insert(ShellTool(jobs.clone()), Approval::Allow);
        tools.insert(ShellPollTool(jobs), Approval::Allow);
        let (event_tx, event_rx) = mpsc::channel(16);
        // Never read: drop the receiver so event sends fail fast.
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(2);
        let (completed, _) = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("go".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        assert!(matches!(
            &completed[2],
            Message::Tool { content, .. } if content.starts_with("status: running\n")
        ));
        assert_eq!(completed[3].content(), "done");

        // The model ended its turn while the job was still running: no
        // command survives the turn unattended.
        let gone = tools
            .get("shell_poll")
            .unwrap()
            .tool
            .run_streamed(json!({ "job_id": "shell-1" }), None, usize::MAX)
            .await
            .unwrap_err();
        assert!(gone.to_string().contains("unknown shell job"));
    }

    #[test]
    fn consumed_web_browser_results_are_ejected_only_from_model_context() {
        let output = serde_json::json!({
            "url": "https://example.com/docs",
            "title": "Documentation",
            "content": "page content ".repeat(2_000),
            "links": []
        })
        .to_string();
        let mut context = vec![
            Message::assistant(
                String::new(),
                "model".into(),
                String::new(),
                vec![ToolCall {
                    id: "browser-1".into(),
                    name: "web_browser".into(),
                    arguments: serde_json::json!({ "url": "https://example.com/docs" }),
                }],
            ),
            Message::tool("browser-1".into(), output, None, None),
            Message::assistant(
                "I used the documentation.".into(),
                "model".into(),
                String::new(),
                Vec::new(),
            ),
            Message::user("continue".into()),
        ];
        let transcript = context.clone();
        let full_tokens = estimate_tokens(&context);

        assert_eq!(eject_consumed_web_results(&mut context), 1);
        let Message::Tool { content, .. } = &context[1] else {
            panic!("expected tool result");
        };
        let marker: serde_json::Value = serde_json::from_str(content).unwrap();
        assert_eq!(marker["ejected"], true);
        assert_eq!(marker["url"], "https://example.com/docs");
        assert_eq!(marker["original_chars"], 26_000);
        assert!(estimate_tokens(&context) < full_tokens / 10);
        assert!(matches!(
            &transcript[1],
            Message::Tool { content, .. } if content.contains("page content")
        ));
    }

    #[test]
    fn tool_diffs_stay_in_history_but_not_model_context() {
        let transcript = vec![Message::tool(
            "write-1".into(),
            "wrote file.txt".into(),
            None,
            Some("large diff ".repeat(1_000)),
        )];
        let mut context = transcript.clone();

        assert_eq!(strip_display_metadata(&mut context), 1);
        assert!(matches!(
            &transcript[0],
            Message::Tool { diff: Some(diff), .. } if diff.starts_with("large diff")
        ));
        assert!(matches!(&context[0], Message::Tool { diff: None, .. }));
        assert!(estimate_tokens(&context) < estimate_tokens(&transcript) / 10);
    }

    #[test]
    fn user_message_pins_the_runtime_context_at_send_time() {
        let plan = ExecutionPlan {
            explanation: None,
            plan: vec![crate::tool::PlanStep {
                step: "current step".into(),
                status: crate::tool::PlanStatus::Pending,
            }],
        };
        let message = UserPrompt {
            content: "continue".into(),
            images: Vec::new(),
        }
        .user_message(Some(&plan));
        let Message::User { content, .. } = message else {
            panic!("expected a user message");
        };
        assert!(content.starts_with("continue\n\n<runtime-context>\n- current time: "));
        assert!(content.contains("- current plan:"));
        assert!(content.contains("current step"));
        assert!(content.ends_with("\n</runtime-context>"));
        assert_eq!(strip_runtime_context(&content), "continue");
    }

    #[test]
    fn user_message_omits_the_plan_when_all_steps_are_completed() {
        let plan = ExecutionPlan {
            explanation: None,
            plan: vec![crate::tool::PlanStep {
                step: "done step".into(),
                status: crate::tool::PlanStatus::Completed,
            }],
        };
        let message = UserPrompt {
            content: "next".into(),
            images: Vec::new(),
        }
        .user_message(Some(&plan));
        let Message::User { content, .. } = message else {
            panic!("expected a user message");
        };
        // A fully completed plan is already fully in context as
        // update_plan calls, so it is not pinned again.
        assert!(content.starts_with("next\n\n<runtime-context>\n- current time: "));
        assert!(!content.contains("- current plan:"));
        assert_eq!(strip_runtime_context(&content), "next");
    }

    #[test]
    fn strip_runtime_context_handles_legacy_and_edge_forms() {
        // Legacy sessions pinned a bare "Runtime context:" line.
        assert_eq!(
            strip_runtime_context("hi\n\nRuntime context:\n- current time: 2026-01-01 00:00"),
            "hi"
        );
        // An image-only prompt has no leading text: the block is the whole
        // message and the visible part is empty.
        let message = UserPrompt {
            content: String::new(),
            images: Vec::new(),
        }
        .user_message(None);
        let Message::User { content, .. } = message else {
            panic!("expected a user message");
        };
        assert!(content.starts_with("<runtime-context>"));
        assert_eq!(strip_runtime_context(&content), "");
        // Plain messages pass through untouched.
        assert_eq!(strip_runtime_context("plain text"), "plain text");
    }

    #[test]
    fn user_message_without_a_plan_pins_only_the_time() {
        let message = UserPrompt {
            content: "hi".into(),
            images: Vec::new(),
        }
        .user_message(None);
        let Message::User { content, .. } = message else {
            panic!("expected a user message");
        };
        assert!(!content.contains("- current plan:"));
        assert!(content.contains("<runtime-context>\n- current time: "));
        assert_eq!(strip_runtime_context(&content), "hi");
    }

    #[test]
    fn steer_message_carries_no_runtime_context() {
        let message = UserPrompt {
            content: "stay focused".into(),
            images: Vec::new(),
        }
        .steer_message();
        let Message::Steer { content, .. } = message else {
            panic!("expected a steer message");
        };
        assert_eq!(content, "stay focused");
        assert!(!content.contains("<runtime-context>"));
    }

    #[test]
    fn web_browser_result_stays_until_the_tool_loop_finishes() {
        let mut context = vec![
            Message::assistant(
                String::new(),
                "model".into(),
                String::new(),
                vec![ToolCall {
                    id: "browser-1".into(),
                    name: "web_browser".into(),
                    arguments: serde_json::json!({}),
                }],
            ),
            Message::tool(
                "browser-1".into(),
                serde_json::json!({
                    "url": "https://example.com",
                    "title": "Example",
                    "content": "full page",
                    "links": []
                })
                .to_string(),
                None,
                None,
            ),
            Message::assistant(
                String::new(),
                "model".into(),
                String::new(),
                vec![ToolCall {
                    id: "next-1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({}),
                }],
            ),
            Message::tool("next-1".into(), "file".into(), None, None),
        ];

        assert_eq!(eject_consumed_web_results(&mut context), 0);
        assert!(matches!(
            &context[1],
            Message::Tool { content, .. } if content.contains("full page")
        ));
        context.push(Message::assistant(
            "done".into(),
            "model".into(),
            String::new(),
            Vec::new(),
        ));
        assert_eq!(eject_consumed_web_results(&mut context), 1);
    }

    #[test]
    fn transient_errors_use_capped_backoff() {
        assert!(is_retryable(&anyhow::anyhow!(
            "server returned 503: unavailable"
        )));
        assert!(is_retryable(&anyhow::anyhow!(
            "send completion request: connection refused"
        )));
        assert!(is_retryable(&anyhow::anyhow!(
            "stream response: transport error: connection closed"
        )));
        assert!(is_retryable(&anyhow::anyhow!(
            "decode response chunk: expected value at line 1"
        )));
        assert!(is_retryable(&anyhow::anyhow!(
            "decode tool arguments: trailing characters"
        )));
        assert!(!is_retryable(&anyhow::anyhow!(
            "server returned 400: invalid request"
        )));
        assert!(!is_retryable(&anyhow::anyhow!(
            "deliver approval for shell: channel closed"
        )));
        assert_eq!(
            (0..6).map(retry_delay).collect::<Vec<_>>(),
            [2, 5, 10, 30, 30, 30]
        );
    }

    #[tokio::test]
    async fn session_title_uses_visible_text_or_reasoning_fallback() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Reasoning("considering options\nGit Pane Scrolling".into()),
            ResponseDelta::Usage(Usage {
                prompt_tokens: 20,
                total_tokens: 24,
            }),
        ]]));
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let (internal_tx, mut internal_rx) = mpsc::channel(2);
        let title = generate_session_title(
            provider.clone(),
            &Config::default(),
            &[
                Message::user("make the Git pane scroll".into()),
                Message::assistant(
                    "Implemented scrolling".into(),
                    "model".into(),
                    String::new(),
                    Vec::new(),
                ),
            ],
            &event_tx,
            &internal_tx,
        )
        .await;

        assert_eq!(title, "Git Pane Scrolling");
        let requests = provider.requests();
        let request = &requests[0];
        assert_eq!(request.temperature, None);
        assert_eq!(request.reasoning_effort, Some(ReasoningEffort::Low));
        assert_eq!(request.max_tokens, Some(512));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ModelRequestStarted(_))
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ResponseHeadersReceived)
        ));
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::ResponseStarted)
        ));
        assert!(matches!(
            internal_rx.recv().await,
            Some(InternalEvent::AuxiliaryUsage(Usage {
                total_tokens: 24,
                ..
            }))
        ));
    }

    #[test]
    fn session_title_cleanup_handles_json_and_empty_model_output() {
        assert_eq!(
            clean_session_title(r#"{"title":"Mouse Resizable Panes"}"#).as_deref(),
            Some("Mouse Resizable Panes")
        );
        assert_eq!(
            fallback_session_title(&[Message::user("single".into())]),
            "single Conversation"
        );
        assert_eq!(
            clean_session_title("Ржавчина Tools").as_deref(),
            Some("Ржавчина Tools")
        );
    }

    #[tokio::test]
    async fn compaction_summarizes_model_context_without_dropping_visible_history() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::Text("Earlier requirements and decisions".into()),
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 90,
                    total_tokens: 100,
                }),
            ],
            vec![
                ResponseDelta::Text("continued".into()),
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 20,
                    total_tokens: 25,
                }),
            ],
        ]));
        let mut config = Config::default();
        config.models[0].max_context_tokens = 2048;
        config.compaction_threshold = 0.75;
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (internal_tx, mut internal_rx) = mpsc::channel(4);

        let result = turn(
            provider,
            &ToolRegistry::default(),
            &config,
            vec![
                Message::user("old turn".into()),
                Message::user("continue".into()),
            ],
            1,
            1800,
            None,
            false,
            no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        assert_eq!(result.completed[0], Message::user("continue".into()));
        assert_eq!(result.completed[1].content(), "continued");
        let compaction = result.compaction.unwrap();
        assert_eq!(compaction.summary, "Earlier requirements and decisions");
        assert_eq!(compaction.through, 1);
        assert!(matches!(
            event_rx.recv().await,
            Some(Event::CompactionStarted)
        ));
        loop {
            match event_rx.recv().await {
                Some(Event::ContextCompacted { summary }) => {
                    assert_eq!(summary, "Earlier requirements and decisions");
                    break;
                }
                Some(_) => {}
                None => panic!("events closed before compaction finished"),
            }
        }
        assert!(matches!(
            internal_rx.recv().await,
            Some(InternalEvent::AuxiliaryUsage(Usage {
                total_tokens: 100,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn turn_start_compaction_anchors_on_real_usage_not_the_whole_context_estimate() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Text("ok".into()),
            ResponseDelta::Usage(Usage {
                prompt_tokens: 90,
                total_tokens: 100,
            }),
        ]]));
        let mut config = Config::default();
        config.models[0].max_context_tokens = 2048;
        config.compaction_threshold = 0.75;
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (internal_tx, _internal_rx) = mpsc::channel(4);

        // The whole-context estimate is pushed past the threshold by an
        // assistant message whose bulk (a long reasoning transcript) the
        // provider never charges for, but the last reported usage is well
        // below it: the next request is that usage plus one short word.
        let bulky = Message::assistant("done".into(), "model".into(), "x".repeat(8000), Vec::new());
        assert!(estimate_tokens(&[bulky.clone()]) as f64 >= 2048.0 * 0.75);

        let result = turn(
            provider,
            &ToolRegistry::default(),
            &config,
            vec![
                Message::user("old turn".into()),
                bulky,
                Message::user("commit".into()),
            ],
            2,
            1000,
            None,
            false,
            no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        assert!(result.compaction.is_none());
        assert_eq!(result.completed[0], Message::user("commit".into()));
        assert_eq!(result.completed[1].content(), "ok");
        while let Ok(event) = event_rx.try_recv() {
            assert!(!matches!(event, Event::CompactionStarted));
        }
    }

    #[tokio::test]
    async fn compaction_rejects_a_reasoning_only_response() {
        // A response that only thinks and never writes output text is not
        // a summary: the chain of thought must not be persisted as one.
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Reasoning("Let me think about what to preserve...".into()),
            ResponseDelta::Completed,
            ResponseDelta::Usage(Usage {
                prompt_tokens: 90,
                total_tokens: 100,
            }),
        ]]));
        let mut config = Config::default();
        config.models[0].reasoning_efforts = vec![ReasoningEffort::Low, ReasoningEffort::Medium];
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (internal_tx, mut internal_rx) = mpsc::channel(2);

        let error = summarize(
            provider.clone(),
            &config,
            &[Message::user("old turn".into())],
            &event_tx,
            &internal_tx,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("compaction produced no summary text")
        );
        assert_eq!(
            provider.requests()[0].reasoning_effort,
            Some(ReasoningEffort::Low)
        );
        assert!(matches!(
            internal_rx.recv().await,
            Some(InternalEvent::AuxiliaryUsage(Usage {
                total_tokens: 100,
                ..
            }))
        ));
    }

    #[tokio::test]
    async fn compaction_runs_without_reasoning_when_supported() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Text("dense summary".into()),
            ResponseDelta::Completed,
        ]]));
        let mut config = Config::default();
        config.models[0].reasoning_efforts = vec![
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
        ];
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (internal_tx, _internal_rx) = mpsc::channel(2);

        let summary = summarize(
            provider.clone(),
            &config,
            &[Message::user("old turn".into())],
            &event_tx,
            &internal_tx,
            None,
        )
        .await
        .unwrap();

        assert_eq!(summary.0, "dense summary");
        // Reasoning would only spend the shared output budget.
        assert_eq!(
            provider.requests()[0].reasoning_effort,
            Some(ReasoningEffort::None)
        );
    }

    #[tokio::test]
    async fn compaction_fails_rather_than_persisting_truncated_reasoning() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Reasoning("cut off mid-thought".into()),
            ResponseDelta::Truncated("max_output_tokens".into()),
        ]]));
        let config = Config::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (internal_tx, _internal_rx) = mpsc::channel(2);

        let error = summarize(
            provider,
            &config,
            &[Message::user("old turn".into())],
            &event_tx,
            &internal_tx,
            None,
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("was truncated (max_output_tokens) before writing any summary text")
        );
    }

    #[tokio::test]
    async fn compaction_fails_rather_than_persisting_a_dropped_reasoning_stream() {
        // The stream closed without any terminal event — e.g. the output
        // budget ran out inside the reasoning and the connection ended.
        // The cut-off monologue must not become the summary.
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Reasoning("cut off mid-thought".into()),
            ResponseDelta::Usage(Usage {
                prompt_tokens: 90,
                total_tokens: 4186,
            }),
        ]]));
        let config = Config::default();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (internal_tx, _internal_rx) = mpsc::channel(2);

        let error = summarize(
            provider,
            &config,
            &[Message::user("old turn".into())],
            &event_tx,
            &internal_tx,
            None,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("ended before finishing"));
    }

    #[tokio::test]
    async fn compaction_trim_leaves_room_for_the_scaled_budget() {
        let provider = Arc::new(MockProvider::new(vec![vec![ResponseDelta::Text(
            "dense summary".into(),
        )]]));
        let mut config = Config::default();
        config.models[0].max_context_tokens = 16_384;
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (internal_tx, _internal_rx) = mpsc::channel(2);

        // 20 messages of ~1,000 tokens: the input alone nearly fills the
        // context, so the trim must drop messages until the 4096-token
        // output floor fits alongside it.
        let messages = vec![Message::user("a".repeat(4_000)); 20];
        summarize(
            provider.clone(),
            &config,
            &messages,
            &event_tx,
            &internal_tx,
            None,
        )
        .await
        .unwrap();

        let request = &provider.requests()[0];
        let input_tokens = estimate_tokens(&request.messages);
        assert!(
            request.messages.len() < 21,
            "messages must be dropped to free room"
        );
        assert!(
            request.max_tokens.unwrap() as u64 >= 4_096,
            "a reasoning model needs the full floor, not the sliver the input left behind"
        );
        assert!(
            input_tokens + request.max_tokens.unwrap() as u64 <= 16_384,
            "input + output must fit the context"
        );
    }

    #[tokio::test]
    async fn compaction_output_budget_scales_with_the_conversation() {
        let provider = Arc::new(MockProvider::new(vec![vec![ResponseDelta::Text(
            "dense summary".into(),
        )]]));
        let mut config = Config::default();
        config.models[0].max_context_tokens = 1_048_576;
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (internal_tx, _internal_rx) = mpsc::channel(2);

        // ~50k tokens of conversation: the budget must grow well past the
        // old 4096 ceiling so a reasoning model can think first.
        let messages = vec![Message::user("a".repeat(200_000))];
        summarize(
            provider.clone(),
            &config,
            &messages,
            &event_tx,
            &internal_tx,
            None,
        )
        .await
        .unwrap();

        let request = &provider.requests()[0];
        let input_tokens = estimate_tokens(&request.messages);
        assert_eq!(
            request.max_tokens.unwrap() as u64,
            summary_output_budget(input_tokens),
            "the output budget scales with the conversation"
        );
        assert!(
            request.max_tokens.unwrap() as u64 > 4_096,
            "a large conversation needs a large budget"
        );
        assert!(
            input_tokens + request.max_tokens.unwrap() as u64 <= 1_048_576,
            "input + output must fit the context"
        );
    }

    #[tokio::test]
    async fn compaction_input_is_trimmed_when_it_does_not_fit() {
        let provider = Arc::new(MockProvider::new(vec![vec![ResponseDelta::Text(
            "dense summary".into(),
        )]]));
        let mut config = Config::default();
        config.models[0].max_context_tokens = 2_048;
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (internal_tx, _internal_rx) = mpsc::channel(2);

        // retain recent work and the final instruction in a small window
        let messages = vec![
            Message::user("a".repeat(7_200)),
            Message::assistant(
                "recent work ".repeat(300),
                "model".into(),
                String::new(),
                Vec::new(),
            ),
        ];
        summarize(
            provider.clone(),
            &config,
            &messages,
            &event_tx,
            &internal_tx,
            None,
        )
        .await
        .unwrap();

        let request = &provider.requests()[0];
        let input_tokens = estimate_tokens(&request.messages);
        assert_eq!(
            request.messages.len(),
            3,
            "retain both instructions and the recent work"
        );
        assert_eq!(request.messages[1], messages[1]);
        assert!(
            request
                .messages
                .last()
                .unwrap()
                .content()
                .starts_with("Write the continuation summary")
        );
        assert!(
            request.max_tokens.unwrap() as u64 >= 128,
            "the summary must stay usable"
        );
        assert!(
            input_tokens + request.max_tokens.unwrap() as u64 <= 2_048,
            "input + output must fit the context"
        );
    }

    #[tokio::test]
    async fn compaction_refuses_to_summarize_an_empty_trimmed_context() {
        let provider = Arc::new(MockProvider::new(Vec::new()));
        let mut config = Config::default();
        config.models[0].max_context_tokens = 2_048;
        let (events, _receiver) = mpsc::channel(8);
        let (internal, _receiver) = mpsc::channel(2);
        let error = summarize(
            provider.clone(),
            &config,
            &[Message::user("x".repeat(12_000))],
            &events,
            &internal,
            None,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("no conversation fits"));
        assert!(provider.requests().is_empty());
    }

    #[test]
    fn close_open_tool_calls_answers_only_the_calls_without_a_result() {
        // The calls of one message run at the same time, so an interrupted
        // turn can already hold the result of the last call while a middle
        // one is still open. Matching calls by position would duplicate a
        // result and leave one call unanswered for the provider.
        let mut tail = vec![
            Message::user("go".into()),
            Message::assistant(
                "working".into(),
                "model".into(),
                String::new(),
                vec![
                    ToolCall {
                        id: "a".into(),
                        name: "echo".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "echo".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "c".into(),
                        name: "echo".into(),
                        arguments: json!({}),
                    },
                ],
            ),
            Message::tool("a".into(), "first".into(), None, None),
            Message::tool("c".into(), "third".into(), None, None),
        ];

        close_open_tool_calls(&mut tail, CANCELLED_TOOL_OUTPUT);

        assert_eq!(
            tail.iter()
                .filter_map(|message| match message {
                    Message::Tool { call_id, .. } => Some(call_id.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            ["a", "c", "b"],
            "one result per call, the open last one closed after them"
        );
    }

    #[test]
    fn drop_oldest_message_removes_tool_results_with_their_call() {
        let call = ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: json!({}),
        };
        let mut request = vec![
            Message::system("prompt".into()),
            Message::user("old".into()),
            Message::assistant_response(
                String::new(),
                "model".into(),
                String::new(),
                vec![call],
                Vec::new(),
            ),
            Message::tool("call_1".into(), "result".into(), None, None),
            Message::user("new".into()),
        ];

        assert!(drop_oldest_message(&mut request));
        assert_eq!(request.len(), 4);
        // The assistant call and its result leave together, so the
        // request never starts on an orphaned tool result.
        assert!(drop_oldest_message(&mut request));
        assert_eq!(request.len(), 2);
        assert!(matches!(&request[1], Message::User { .. }));
    }

    #[test]
    fn image_tokens_follow_the_openai_auto_detail_rule() {
        fn image(width: u32, height: u32) -> ImageContent {
            ImageContent {
                mime_type: "image/png".into(),
                data: String::new(),
                path: None,
                width,
                height,
            }
        }
        // 1x1 is a single 512px tile.
        assert_eq!(image_tokens(&image(1, 1)), 85 + 170);
        // 1024x1024 is capped to 768x768: four tiles.
        assert_eq!(image_tokens(&image(1024, 1024)), 85 + 4 * 170);
        // 1920x1080 is capped to 1365x768: six tiles.
        assert_eq!(image_tokens(&image(1920, 1080)), 85 + 6 * 170);
        // 3072x1024 fits 2048x683: eight tiles, the auto maximum.
        assert_eq!(image_tokens(&image(3072, 1024)), 85 + 8 * 170);
        // Unknown dimensions reserve the per-image maximum.
        assert_eq!(image_tokens(&image(0, 0)), 85 + 8 * 170);
    }

    #[test]
    fn image_results_are_counted_in_the_context_budget() {
        let image = ImageContent {
            mime_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
            path: None,
            width: 1024,
            height: 1024,
        };
        let without = Message::tool("call_1".into(), "viewed image.png".into(), None, None);
        let with = Message::tool(
            "call_1".into(),
            "viewed image.png".into(),
            Some(image.clone()),
            None,
        );

        // At least the provider's image reservation, plus the serialized
        // image metadata field.
        assert!(
            estimate_tokens(std::slice::from_ref(&with))
                - estimate_tokens(std::slice::from_ref(&without))
                >= image_tokens(&image)
        );
    }

    #[tokio::test]
    async fn image_result_is_dropped_when_it_does_not_fit_the_budget() {
        struct BigImage;

        #[async_trait]
        impl Tool for BigImage {
            fn name(&self) -> &str {
                "view"
            }
            fn description(&self) -> &str {
                "show an image"
            }
            fn schema(&self) -> Value {
                json!({ "type": "object" })
            }
            async fn run(&self, _args: Value) -> Result<ToolResult> {
                Ok(ToolResult {
                    is_error: false,
                    output: "viewed big.png".into(),
                    image: Some(ImageContent {
                        mime_type: "image/png".into(),
                        data: "aW1hZ2U=".into(),
                        path: None,
                        width: 3072,
                        height: 1024, // 1445 tokens
                    }),
                    file: None,
                    diff: None,
                })
            }
        }

        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("view".into()),
                    arguments: "{}".into(),
                },
                ResponseDelta::Usage(Usage {
                    prompt_tokens: 0,
                    total_tokens: 152,
                }),
            ],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(BigImage, Approval::Allow);
        let mut config = Config::default();
        config.models[0].max_context_tokens = 200;
        config.compaction_threshold = 1.0;
        let (event_tx, event_rx) = mpsc::channel(16);
        // Never read: drop the receiver so event sends fail fast instead
        // of blocking once the buffer fills.
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(4);

        let (completed, _) = agent(
            provider.clone(),
            &tools,
            &config,
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &no_steers(),
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        // The 1445-token image cannot fit the 48-token message budget; a
        // note stays in the content, and the next model request fits the
        // context.
        assert!(matches!(
            &completed[2],
            Message::Tool {
                content,
                image: None,
                ..
            } if content.contains("[image omitted: no room left in the context]")
        ));
        let requests = provider.requests();
        assert!(
            estimate_tokens(&requests.last().unwrap().messages)
                <= config.active_model().max_context_tokens
        );
    }

    #[test]
    fn request_context_projects_the_summary_and_drops_markers() {
        let summary = "dense summary";
        let mut meta = SessionMeta::test();
        meta.compaction_summary = Some(summary.into());
        meta.compacted_through = 2;
        let messages = vec![
            Message::user("old turn".into()),
            Message::system(format!("{COMPACTION_MARKER}\n{summary}")),
            Message::user("continue".into()),
            Message::assistant("done".into(), "model".into(), String::new(), Vec::new()),
        ];

        let context = request_context(&messages, &meta);

        assert_eq!(context.len(), 3);
        assert!(matches!(
            &context[0],
            Message::System { content, .. }
                if content == "Conversation summary for continuation:\ndense summary"
        ));
        assert_eq!(context[1], Message::user("continue".into()));
        assert!(context.iter().all(|message| !is_compaction_marker(message)));
    }

    #[tokio::test]
    async fn steer_prompts_are_injected_into_the_next_model_request() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_1".into()),
                name: Some("echo".into()),
                arguments: r#"{"value":"done"}"#.into(),
            }],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let steers = no_steers();
        steers.lock().unwrap().push(
            UserPrompt {
                content: "focus on the tests".into(),
                images: Vec::new(),
            }
            .steer_message(),
        );
        let (event_tx, event_rx) = mpsc::channel(16);
        // Never read: drop the receiver so event sends fail fast instead
        // of blocking once the buffer fills.
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(2);

        let (completed, _) = agent(
            provider.clone(),
            &tools,
            &Config::default(),
            vec![Message::user("run it".into())],
            0,
            0,
            None,
            &steers,
            &fresh_progress(),
            &event_tx,
            &internal_tx,
            &assistant_agent(),
            "test",
            "turn",
            &delegation_port(),
            &test_catalog(),
        )
        .await
        .unwrap();

        // The steer is appended after everything already delivered and
        // reaches the very next model request.
        let first_request = &provider.requests()[0];
        assert!(matches!(
            first_request.messages.last().unwrap(),
            Message::Steer { content, .. } if content == "focus on the tests"
        ));
        assert!(
            completed
                .iter()
                .any(|message| matches!(message, Message::Steer { .. }))
        );
    }

    #[tokio::test]
    async fn steer_during_a_tool_run_reaches_the_following_request() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![ResponseDelta::ToolCall {
                index: 0,
                id: Some("call_1".into()),
                name: Some("slow_echo".into()),
                arguments: "{}".into(),
            }],
            vec![ResponseDelta::Text("finished".into())],
        ]));
        let mut tools = ToolRegistry::default();
        // 25 chunks at 2ms each: the tool runs long enough to steer it.
        tools.insert(SlowEcho(vec!["x".repeat(64); 25]), Approval::Allow);
        let steers = no_steers();
        let (event_tx, event_rx) = mpsc::channel(16);
        drop(event_rx);
        let (internal_tx, _internal_rx) = mpsc::channel(2);
        let agent_task = {
            let provider = provider.clone();
            let tools = tools.clone();
            let events = event_tx.clone();
            let internal = internal_tx.clone();
            let steers = steers.clone();
            tokio::spawn(async move {
                agent(
                    provider,
                    &tools,
                    &Config::default(),
                    vec![Message::user("run it".into())],
                    0,
                    0,
                    None,
                    &steers,
                    &fresh_progress(),
                    &events,
                    &internal,
                    &assistant_agent(),
                    "test",
                    "turn",
                    &delegation_port(),
                    &test_catalog(),
                )
                .await
            })
        };

        // Well past the first request, mid-tool-run: the steer must not
        // re-request what already went out, only the next delivery.
        tokio::time::sleep(Duration::from_millis(10)).await;
        steers.lock().unwrap().push(
            UserPrompt {
                content: "also fix the docs".into(),
                images: Vec::new(),
            }
            .steer_message(),
        );
        agent_task.await.unwrap().unwrap();

        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert!(
            !requests[0]
                .messages
                .iter()
                .any(|message| matches!(message, Message::Steer { .. }))
        );
        assert!(requests[1].messages.iter().any(|message| {
            matches!(message, Message::Steer { content, .. }
                if content == "also fix the docs")
        }));
    }

    /// The mock with one twist: the request that carries a tool result —
    /// a turn's final model request — is delayed, so a steer can land
    /// after the turn's last injection point.
    struct DelayedFinalProvider {
        mock: MockProvider,
    }

    #[async_trait]
    impl Provider for DelayedFinalProvider {
        async fn stream(&self, request: CompletionRequest) -> Result<ResponseStream> {
            if request
                .messages
                .iter()
                .any(|message| matches!(message, Message::Tool { .. }))
            {
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
            self.mock.stream(request).await
        }
    }

    #[tokio::test]
    async fn steers_queued_after_the_final_request_start_a_fresh_turn() {
        let provider = Arc::new(DelayedFinalProvider {
            mock: MockProvider::new(vec![
                vec![ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("echo".into()),
                    arguments: r#"{"value":"done"}"#.into(),
                }],
                vec![ResponseDelta::Text("final answer".into())],
                vec![ResponseDelta::Text("steered answer".into())],
            ]),
        });
        let root = std::env::temp_dir().join(format!(
            "rope-steer-resubmit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create_in(root.clone(), "steer".into())
            .await
            .unwrap();
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let run_task = tokio::spawn({
            let provider = provider.clone();
            async move {
                run(
                    Config::default(),
                    provider,
                    tools,
                    session,
                    Vec::new(),
                    ProjectState::new().await.unwrap(),
                    test_catalog(),
                    delegation_port(),
                    command_rx,
                    event_tx,
                )
                .await
            }
        });
        command_tx
            .send(Command::Submit(UserPrompt {
                content: "go".into(),
                images: Vec::new(),
            }))
            .await
            .unwrap();
        // By the time the tool result lands, the turn's final model
        // request is already built — and its response is delayed.
        while let Some(event) = event_rx.recv().await {
            if matches!(event, Event::ToolResult { .. }) {
                break;
            }
        }
        // Let the agent drain the (empty) steer queue for the final
        // request, then steer while the response streams: the prompt
        // misses every injection point of this turn.
        tokio::time::sleep(Duration::from_millis(20)).await;
        command_tx
            .send(Command::Steer(UserPrompt {
                content: "steered".into(),
                images: Vec::new(),
            }))
            .await
            .unwrap();
        let mut finished = 0;
        while let Some(event) = event_rx.recv().await {
            if matches!(event, Event::GenerationFinished { .. }) {
                finished += 1;
                if finished == 2 {
                    break;
                }
            }
        }
        let (reply, summary) = tokio::sync::oneshot::channel();
        command_tx.send(Command::Shutdown(reply)).await.unwrap();
        summary.await.unwrap();
        run_task.await.unwrap();
        let _ = tokio::fs::remove_dir_all(&root).await;

        let requests = provider.mock.requests();
        assert_eq!(requests.len(), 3);
        // The steer missed the turn's final request...
        assert!(
            !requests[1]
                .messages
                .iter()
                .any(|message| matches!(message, Message::Steer { .. }))
        );
        // ...so it was resubmitted: the fresh turn carries it as its
        // prompt, on top of the finished conversation, keeping its Steer
        // identity as the UI rendered it.
        let resubmitted = &requests[2].messages;
        assert!(resubmitted.last().is_some_and(|message| {
            matches!(message, Message::Steer { content, .. }
                if content == "steered")
        }));
    }

    #[tokio::test]
    async fn compact_command_summarizes_the_idle_conversation() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Text("dense summary".into()),
            ResponseDelta::Usage(Usage {
                prompt_tokens: 90,
                total_tokens: 120,
            }),
        ]]));
        let root = std::env::temp_dir().join(format!(
            "rope-compact-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create_in(root.clone(), "compact".into())
            .await
            .unwrap();
        let messages = vec![
            Message::user("build the feature".into()),
            Message::assistant(
                "done".to_string(),
                "model".into(),
                String::new(),
                Vec::new(),
            ),
        ];
        let tools = ToolRegistry::default();
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let run_task = tokio::spawn({
            let provider = provider.clone();
            async move {
                run(
                    Config::default(),
                    provider,
                    tools,
                    session,
                    messages.clone(),
                    ProjectState::new().await.unwrap(),
                    test_catalog(),
                    delegation_port(),
                    command_rx,
                    event_tx,
                )
                .await
            }
        });

        command_tx.send(Command::Compact).await.unwrap();
        let summary = loop {
            let event = event_rx.recv().await.unwrap();
            if let Event::ContextCompacted { summary } = event {
                break summary;
            }
        };
        assert_eq!(summary, "dense summary");

        let (reply, _summary) = tokio::sync::oneshot::channel();
        command_tx.send(Command::Shutdown(reply)).await.unwrap();
        run_task.await.unwrap();

        // One model request: the summarizer prompt, the idle
        // conversation, and the trailing summary instruction — no turn in
        // between.
        let requests = provider.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].messages.len(), 4);
        assert!(matches!(
            requests[0].messages.last(),
            Some(Message::User { content, .. })
                if content.starts_with("Write the continuation summary")
        ));

        // The session persists the summary, the boundary, and the
        // transcript marker the UI renders as a collapsed section.
        let (saved, saved_messages) = Session::resume_in(root.clone(), "compact").await.unwrap();
        assert_eq!(
            saved.meta.compaction_summary.as_deref(),
            Some("dense summary")
        );
        assert_eq!(saved.meta.compacted_through, 2);
        assert!(saved_messages.last().is_some_and(|message| {
            matches!(
                message,
                Message::System { content, .. }
                    if content.starts_with("Context compacted\ndense summary")
            )
        }));
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[tokio::test]
    async fn compact_command_refuses_an_empty_conversation() {
        let provider = Arc::new(MockProvider::new(Vec::new()));
        let root = std::env::temp_dir().join(format!(
            "rope-compact-empty-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create_in(root.clone(), "empty".into())
            .await
            .unwrap();
        let tools = ToolRegistry::default();
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let run_task = tokio::spawn({
            let provider = provider.clone();
            async move {
                run(
                    Config::default(),
                    provider,
                    tools,
                    session,
                    Vec::new(),
                    ProjectState::new().await.unwrap(),
                    test_catalog(),
                    delegation_port(),
                    command_rx,
                    event_tx,
                )
                .await
            }
        });

        command_tx.send(Command::Compact).await.unwrap();
        let error = loop {
            let event = event_rx.recv().await.unwrap();
            if let Event::Error(error) = event {
                break error;
            }
        };
        assert!(error.contains("nothing to compact yet"));

        let (reply, _summary) = tokio::sync::oneshot::channel();
        command_tx.send(Command::Shutdown(reply)).await.unwrap();
        run_task.await.unwrap();
        assert!(provider.requests().is_empty());
        let _ = tokio::fs::remove_dir_all(&root).await;
    }

    #[test]
    fn close_open_tool_calls_appends_a_cancellation_result_per_open_call() {
        // Two calls, no results yet: both close, in call order.
        let mut tail = vec![
            Message::user("go".into()),
            Message::assistant(
                "working".into(),
                "model".into(),
                String::new(),
                vec![
                    ToolCall {
                        id: "a".into(),
                        name: "echo".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "echo".into(),
                        arguments: json!({}),
                    },
                ],
            ),
        ];
        close_open_tool_calls(&mut tail, CANCELLED_TOOL_OUTPUT);
        assert_eq!(tail.len(), 4);
        assert!(matches!(
            &tail[2],
            Message::Tool { call_id, content, .. }
                if call_id == "a" && content == CANCELLED_TOOL_OUTPUT
        ));
        assert!(matches!(&tail[3], Message::Tool { call_id, .. } if call_id == "b"));

        // One call answered: only the rest closes.
        let mut tail = vec![
            Message::user("go".into()),
            Message::assistant(
                "working".into(),
                "model".into(),
                String::new(),
                vec![
                    ToolCall {
                        id: "a".into(),
                        name: "echo".into(),
                        arguments: json!({}),
                    },
                    ToolCall {
                        id: "b".into(),
                        name: "echo".into(),
                        arguments: json!({}),
                    },
                ],
            ),
            Message::tool("a".into(), "first".into(), None, None),
        ];
        close_open_tool_calls(&mut tail, CANCELLED_TOOL_OUTPUT);
        assert_eq!(tail.len(), 4);
        assert!(matches!(&tail[3], Message::Tool { call_id, .. } if call_id == "b"));

        // A completed tail is unchanged.
        let mut tail = vec![
            Message::user("go".into()),
            Message::assistant(
                "working".into(),
                "model".into(),
                String::new(),
                vec![ToolCall {
                    id: "a".into(),
                    name: "echo".into(),
                    arguments: json!({}),
                }],
            ),
            Message::tool("a".into(), "first".into(), None, None),
            Message::assistant("done".into(), "model".into(), String::new(), Vec::new()),
        ];
        close_open_tool_calls(&mut tail, CANCELLED_TOOL_OUTPUT);
        assert_eq!(tail.len(), 4);
    }

    #[tokio::test]
    async fn cancelled_turn_persists_its_completed_work() {
        // The second response is held behind an 80ms delay that is
        // interrupted before it is consumed, so the follow-up turn gets
        // the "continued answer".
        let provider = Arc::new(DelayedFinalProvider {
            mock: MockProvider::new(vec![
                vec![ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("echo".into()),
                    arguments: r#"{"value":"done"}"#.into(),
                }],
                vec![ResponseDelta::Text("continued answer".into())],
                vec![ResponseDelta::Text("never used".into())],
            ]),
        });
        let root = std::env::temp_dir().join(format!(
            "rope-cancel-salvage-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create_in(root.clone(), "cancel".into())
            .await
            .unwrap();
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let run_task = tokio::spawn({
            let provider = provider.clone();
            async move {
                run(
                    Config::default(),
                    provider,
                    tools,
                    session,
                    Vec::new(),
                    ProjectState::new().await.unwrap(),
                    test_catalog(),
                    delegation_port(),
                    command_rx,
                    event_tx,
                )
                .await
            }
        });
        command_tx
            .send(Command::Submit(UserPrompt {
                content: "go".into(),
                images: Vec::new(),
            }))
            .await
            .unwrap();
        // Once the tool result lands, the turn's final model request is
        // built and its response delayed: a cancel lands mid-turn.
        while let Some(event) = event_rx.recv().await {
            if matches!(event, Event::ToolResult { .. }) {
                break;
            }
        }
        // Let the agent drain the (empty) steer queue for the final
        // request, then steer while the response is delayed: the prompt
        // misses the turn's last injection point.
        tokio::time::sleep(Duration::from_millis(20)).await;
        command_tx
            .send(Command::Steer(UserPrompt {
                content: "keep going".into(),
                images: Vec::new(),
            }))
            .await
            .unwrap();
        command_tx.send(Command::Cancel).await.unwrap();
        while let Some(event) = event_rx.recv().await {
            if matches!(event, Event::GenerationCancelled) {
                break;
            }
        }
        // The next turn must see the cancelled turn's work and the marker.
        command_tx
            .send(Command::Submit(UserPrompt {
                content: "continue".into(),
                images: Vec::new(),
            }))
            .await
            .unwrap();
        while let Some(event) = event_rx.recv().await {
            if matches!(event, Event::GenerationFinished { .. }) {
                break;
            }
        }
        let (reply, _summary) = tokio::sync::oneshot::channel();
        command_tx.send(Command::Shutdown(reply)).await.unwrap();
        run_task.await.unwrap();

        // The persisted session keeps the interrupted turn's work, the
        // undelivered steer, and the cancellation marker — in order —
        // followed by the fresh turn's own user + assistant messages.
        let (_, messages) = Session::resume_in(root.clone(), "cancel").await.unwrap();
        assert_eq!(messages.len(), 7);
        assert!(
            matches!(&messages[0], Message::User { content, .. } if content.starts_with("go\n\n<runtime-context>"))
        );
        assert!(
            matches!(&messages[1], Message::Assistant { tool_calls, duration_ms, .. }
            if tool_calls.len() == 1 && tool_calls[0].id == "call_1" && duration_ms.is_none())
        );
        assert!(
            matches!(&messages[2], Message::Tool { call_id, content, .. }
            if call_id == "call_1" && content == "done")
        );
        assert!(matches!(&messages[3], Message::Steer { content, .. }
                if content == "keep going"));
        assert!(
            matches!(&messages[4], Message::System { content, .. } if content == CANCELLED_BY_USER)
        );
        assert!(matches!(&messages[5], Message::User { content, .. }
                if content.starts_with("continue\n\n<runtime-context>")));
        assert!(
            matches!(&messages[6], Message::Assistant { content, duration_ms, .. }
                if content == "continued answer" && duration_ms.is_some())
        );

        // The next turn's model request carries the salvaged work and the
        // marker before its own prompt, so the agent continues from where
        // the user stopped it instead of from before the cancelled turn.
        let requests = provider.mock.requests();
        assert_eq!(requests.len(), 2);
        let next = &requests[1].messages;
        let seen_tool = next
            .iter()
            .position(|m| matches!(m, Message::Tool { call_id, .. } if call_id == "call_1"))
            .expect("the cancelled turn's tool result left the next context");
        let seen_marker = next
            .iter()
            .position(
                |m| matches!(m, Message::System { content, .. } if content == CANCELLED_BY_USER),
            )
            .expect("the cancellation marker left the next context");
        let seen_continue = next
            .iter()
            .position(|m| {
                matches!(m, Message::User { content, .. }
                if content.starts_with("continue\n\n<runtime-context>"))
            })
            .expect("the fresh prompt is in the context");
        assert!(seen_tool < seen_marker);
        assert!(seen_marker < seen_continue);

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_turn_closes_the_tool_calls_it_left_open() {
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                ResponseDelta::ToolCall {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("slow_echo".into()),
                    arguments: r#"{"value":"x"}"#.into(),
                },
                ResponseDelta::ToolCall {
                    index: 1,
                    id: Some("call_2".into()),
                    name: Some("slow_echo".into()),
                    arguments: r#"{"value":"y"}"#.into(),
                },
            ],
            vec![ResponseDelta::Text("never reached".into())],
        ]));
        let root = std::env::temp_dir().join(format!(
            "rope-cancel-open-calls-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create_in(root.clone(), "open".into())
            .await
            .unwrap();
        // 40 chunks x 2ms keeps both tools running long enough to be
        // cancelled while they are in flight.
        let mut tools = ToolRegistry::default();
        tools.insert(SlowEcho(vec!["x".into(); 40]), Approval::Allow);
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let run_task = tokio::spawn({
            let provider = provider.clone();
            async move {
                run(
                    Config::default(),
                    provider,
                    tools,
                    session,
                    Vec::new(),
                    ProjectState::new().await.unwrap(),
                    test_catalog(),
                    delegation_port(),
                    command_rx,
                    event_tx,
                )
                .await
            }
        });
        command_tx
            .send(Command::Submit(UserPrompt {
                content: "go".into(),
                images: Vec::new(),
            }))
            .await
            .unwrap();
        while let Some(event) = event_rx.recv().await {
            if matches!(event, Event::ToolStarted { .. }) {
                break;
            }
        }
        // Both calls are in flight; the turn is stopped while they run.
        tokio::time::sleep(Duration::from_millis(20)).await;
        command_tx.send(Command::Cancel).await.unwrap();
        while let Some(event) = event_rx.recv().await {
            if matches!(event, Event::GenerationCancelled) {
                break;
            }
        }
        let (reply, _summary) = tokio::sync::oneshot::channel();
        command_tx.send(Command::Shutdown(reply)).await.unwrap();
        run_task.await.unwrap();

        // Both open calls received a cancellation result, so the
        // interrupted history is a valid message sequence.
        let (_, messages) = Session::resume_in(root.clone(), "open").await.unwrap();
        assert_eq!(messages.len(), 5);
        assert!(matches!(&messages[1], Message::Assistant { tool_calls, .. }
            if tool_calls.len() == 2 && tool_calls[0].id == "call_1" && tool_calls[1].id == "call_2"));
        assert!(
            matches!(&messages[2], Message::Tool { call_id, content, .. }
            if call_id == "call_1" && content == CANCELLED_TOOL_OUTPUT)
        );
        assert!(
            matches!(&messages[3], Message::Tool { call_id, content, .. }
            if call_id == "call_2" && content == CANCELLED_TOOL_OUTPUT)
        );
        assert!(
            matches!(&messages[4], Message::System { content, .. } if content == CANCELLED_BY_USER)
        );

        tokio::fs::remove_dir_all(&root).await.unwrap();
    }

    /// Runs a turn that asks for one call that answers at once and one that
    /// keeps streaming, and stops it after the quick call has answered and
    /// while the other is still running. `quick_first` is the order the
    /// model made the calls in; what the transcript keeps must not depend
    /// on it. Returns the messages the interrupted turn persisted.
    async fn messages_after_cancelling_a_half_finished_batch(quick_first: bool) -> Vec<Message> {
        let quick = || ResponseDelta::ToolCall {
            index: 0,
            id: Some("call_quick".into()),
            name: Some("echo".into()),
            arguments: r#"{"value":"written"}"#.into(),
        };
        let slow = || ResponseDelta::ToolCall {
            index: 1,
            id: Some("call_slow".into()),
            name: Some("slow_echo".into()),
            arguments: "{}".into(),
        };
        let provider = Arc::new(MockProvider::new(vec![
            vec![
                if quick_first { quick() } else { slow() },
                if quick_first { slow() } else { quick() },
            ],
            vec![ResponseDelta::Text("never reached".into())],
        ]));
        let root = std::env::temp_dir().join(format!(
            "rope-cancel-half-done-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = tokio::fs::remove_dir_all(&root).await;
        tokio::fs::create_dir_all(&root).await.unwrap();
        let session = Session::create_in(root.clone(), "half".to_string())
            .await
            .unwrap();
        // 200 chunks x 2ms keeps the streaming call in flight long after
        // the quick one has answered.
        let mut tools = ToolRegistry::default();
        tools.insert(Echo, Approval::Allow);
        tools.insert(SlowEcho(vec!["x".into(); 200]), Approval::Allow);
        let (command_tx, command_rx) = mpsc::channel(16);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let run_task = tokio::spawn({
            let provider = provider.clone();
            async move {
                run(
                    Config::default(),
                    provider,
                    tools,
                    session,
                    Vec::new(),
                    ProjectState::new().await.unwrap(),
                    test_catalog(),
                    delegation_port(),
                    command_rx,
                    event_tx,
                )
                .await
            }
        });
        command_tx
            .send(Command::Submit(UserPrompt {
                content: "go".into(),
                images: Vec::new(),
            }))
            .await
            .unwrap();
        loop {
            match event_rx.recv().await {
                Some(Event::ToolResult {
                    call_id: id,
                    output,
                    ..
                }) if id == "call_quick" && output == "written" => break,
                Some(_) => {}
                None => panic!("the turn ended before the quick call answered"),
            }
        }
        // The batch is still waiting for the call that streams; stop it now.
        tokio::time::sleep(Duration::from_millis(30)).await;
        command_tx.send(Command::Cancel).await.unwrap();
        while let Some(event) = event_rx.recv().await {
            if matches!(event, Event::GenerationCancelled) {
                break;
            }
        }
        let (reply, _summary) = tokio::sync::oneshot::channel();
        command_tx.send(Command::Shutdown(reply)).await.unwrap();
        run_task.await.unwrap();

        let (_, messages) = Session::resume_in(root.clone(), "half").await.unwrap();
        tokio::fs::remove_dir_all(&root).await.ok();
        messages
    }

    /// What an interrupted batch must leave behind: every call answered
    /// exactly once, the finished one by its real result, the running one
    /// as cancelled, and the marker last.
    fn assert_batch_interrupted_as(messages: &[Message]) {
        assert_eq!(messages.len(), 5);
        assert!(matches!(&messages[0], Message::User { content, .. }
            if content.starts_with("go")));
        assert!(matches!(&messages[1], Message::Assistant { tool_calls, .. }
            if tool_calls.len() == 2
                && tool_calls.iter().any(|call| call.id == "call_quick")
                && tool_calls.iter().any(|call| call.id == "call_slow")));
        let answered = messages
            .iter()
            .filter_map(|message| match message {
                Message::Tool {
                    call_id, content, ..
                } => Some((call_id.as_str(), content.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(answered.len(), 2, "every call of the batch is answered");
        assert!(
            answered.contains(&("call_quick", "written")),
            "work the turn already completed is not reported as cancelled: {answered:?}"
        );
        assert!(answered.contains(&("call_slow", CANCELLED_TOOL_OUTPUT)));
        assert!(matches!(&messages[4], Message::System { content, .. }
            if content == CANCELLED_BY_USER));
    }

    #[tokio::test]
    async fn a_cancelled_batch_keeps_the_results_that_finished() {
        let messages = tokio::time::timeout(
            Duration::from_secs(20),
            messages_after_cancelling_a_half_finished_batch(true),
        )
        .await
        .expect("the turn was stopped");
        assert_batch_interrupted_as(&messages);
    }

    /// The case the order of the calls should not change: the finished call
    /// is the second one, so its result is held back while the turn waits
    /// for the call before it — and the user stops the turn in that moment.
    #[tokio::test]
    async fn a_cancelled_batch_keeps_a_result_the_order_held_back() {
        let messages = tokio::time::timeout(
            Duration::from_secs(20),
            messages_after_cancelling_a_half_finished_batch(false),
        )
        .await
        .expect("the turn was stopped");
        assert_batch_interrupted_as(&messages);
    }
}
