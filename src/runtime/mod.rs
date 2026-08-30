mod message;

use std::{
    collections::HashSet,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

use crate::{
    config::{Config, Startup},
    project::ProjectState,
    provider::{Provider, ResponseDelta, Usage},
    session::{Session, SessionMeta},
    tool::{Approval, ExecutionPlan, ToolDefinition, ToolRegistry},
};
pub use message::{ImageContent, Message, ToolCall};

pub const CANCELLED_BY_USER: &str = "cancelled by user";
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

pub struct UserPrompt {
    pub content: String,
    pub images: Vec<ImageContent>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalDecision {
    AllowOnce,
    AllowSession,
    Deny,
}

pub enum Command {
    Submit(UserPrompt),
    Cancel,
    Approve(ApprovalDecision),
    NewSession(Option<String>),
    Save,
    SelectModel(String),
    NextReasoningEffort,
    RememberCommand(String),
    RefreshProject,
    GitDiff(Option<std::path::PathBuf>),
    Shutdown(oneshot::Sender<SessionSummary>),
}

pub struct SessionSummary {
    pub name: String,
    pub total_tokens: u64,
    pub total_cost: Option<f64>,
}

pub enum Event {
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
    },
    ProjectChanged(ProjectState),
    PlanChanged(Option<ExecutionPlan>),
    GenerationStarted,
    ModelRequestStarted(String),
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
    ApprovalRequested(ToolCall),
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
    CompactionStarted,
    ContextCompacted {
        summary: String,
    },
    GenerationFinished,
    GenerationCancelled,
    Saved,
    Error(String),
}

enum InternalEvent {
    Finished(TurnResult),
    Failed(String),
    Usage(Usage),
    AuxiliaryUsage(Usage),
    PlanUpdated(ExecutionPlan),
    Approval {
        call: ToolCall,
        reply: oneshot::Sender<ApprovalDecision>,
    },
    ProjectChanged(ProjectState),
    ProjectRefresh,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ProjectRequest {
    Refresh,
    Diff(Option<std::path::PathBuf>),
}

/// Serializes git-backed project work: at most one task runs at a time.
/// A request that arrives while a task is in flight replaces the stored
/// pending request, so bursts of requests coalesce into a single follow-up run.
#[derive(Default)]
struct ProjectRequests {
    in_flight: bool,
    pending: Option<ProjectRequest>,
}

impl ProjectRequests {
    /// Returns the request to run now, or stores it if one is already in flight.
    fn request(&mut self, request: ProjectRequest) -> Option<ProjectRequest> {
        if self.in_flight {
            self.pending = Some(request);
            None
        } else {
            self.in_flight = true;
            Some(request)
        }
    }

    /// Called after the in-flight task reports its result; returns the follow-up, if any.
    fn completed(&mut self) -> Option<ProjectRequest> {
        self.in_flight = false;
        self.pending.take()
    }
}

struct TurnResult {
    completed: Vec<Message>,
    compaction: Option<Compaction>,
    title: Option<String>,
}

struct Compaction {
    summary: String,
    through: usize,
}

struct PendingApproval {
    tool: String,
    reply: oneshot::Sender<ApprovalDecision>,
}

pub async fn spawn<P: Provider>(
    config: Config,
    startup: Startup,
    provider: P,
    tools: ToolRegistry,
) -> Result<(mpsc::Sender<Command>, mpsc::Receiver<Event>)> {
    let (session, messages) = Session::open(startup).await?;
    let project = ProjectState::new().await?;
    let (command_tx, command_rx) = mpsc::channel(16);
    let (event_tx, event_rx) = mpsc::channel(64);
    tokio::spawn(run(
        config,
        Arc::new(provider),
        tools,
        session,
        messages,
        project,
        command_rx,
        event_tx,
    ));
    Ok((command_tx, event_rx))
}

#[allow(clippy::too_many_arguments)]
async fn run<P: Provider>(
    mut config: Config,
    provider: Arc<P>,
    tools: ToolRegistry,
    mut session: Session,
    mut messages: Vec<Message>,
    mut project: ProjectState,
    mut commands: mpsc::Receiver<Command>,
    events: mpsc::Sender<Event>,
) {
    let (internal_tx, mut internal_rx) = mpsc::channel(8);
    let mut generation: Option<JoinHandle<()>> = None;
    let mut pending_approval: Option<PendingApproval> = None;
    let mut project_requests = ProjectRequests::default();

    events.send(Event::History(messages.clone())).await.ok();
    events
        .send(Event::SessionChanged(session.display_name().to_owned()))
        .await
        .ok();
    send_usage(&events, &session).await;
    send_settings(&events, &config).await;
    send_context(&events, &session, &config).await;
    events
        .send(Event::ProjectChanged(project.clone()))
        .await
        .ok();
    events
        .send(Event::PlanChanged(session.meta.plan.clone()))
        .await
        .ok();

    loop {
        tokio::select! {
            Some(command) = commands.recv() => match command {
                Command::Submit(prompt) if generation.is_none() => {
                    let persist_from = messages.len();
                    messages.push(Message::user_with_images(prompt.content, prompt.images));
                    let request_messages = request_context(&messages, &session.meta);
                    let context_tokens = session.meta.context_tokens;
                    let generate_title = session.needs_title();
                    let current_plan = session.meta.plan.clone();
                    let provider = provider.clone();
                    let tools = tools.clone();
                    let config = config.clone();
                    let project_prompt = project.prompt().await;
                    let events = events.clone();
                    let internal = internal_tx.clone();
                    events.send(Event::GenerationStarted).await.ok();
                    generation = Some(tokio::spawn(async move {
                        let result = match project_prompt {
                            Ok(prompt) => turn(provider, &tools, &config, request_messages, persist_from, context_tokens, prompt, generate_title, current_plan, &events, &internal).await,
                            Err(error) => Err(error),
                        };
                        let event = match result {
                            Ok(result) => InternalEvent::Finished(result),
                            Err(error) => InternalEvent::Failed(format!("{error:#}")),
                        };
                        internal.send(event).await.ok();
                    }));
                }
                Command::Submit(_) => {}
                Command::Cancel => {
                    if let Some(task) = generation.take() {
                        task.abort();
                        // Let the aborted generation fully unwind before
                        // cancelling tools, so a shell job cannot be
                        // registered after cancellation began.
                        task.await.ok();
                        tools.cancel_active().await;
                        pending_approval = None;
                        let pending_from = messages.len().saturating_sub(1);
                        messages.push(Message::system(CANCELLED_BY_USER.into()));
                        let saved = async {
                            session.append(&messages[pending_from..]).await?;
                            session.save().await
                        }.await;
                        events.send(Event::GenerationCancelled).await.ok();
                        if let Err(error) = saved {
                            events.send(Event::Error(format!("save cancelled turn: {error:#}"))).await.ok();
                        }
                        request_project(&mut project_requests, &project, ProjectRequest::Refresh, &internal_tx);
                    }
                }
                Command::Approve(decision) => {
                    if let Some(pending) = pending_approval.take() {
                        if decision == ApprovalDecision::AllowSession
                            && !session.meta.approved_tools.contains(&pending.tool)
                        {
                            session.meta.approved_tools.push(pending.tool.clone());
                            if let Err(error) = session.save().await {
                                events.send(Event::Error(format!("save session approval: {error:#}"))).await.ok();
                            }
                        }
                        pending.reply.send(decision).ok();
                    }
                }
                Command::NewSession(name) if generation.is_none() => match Session::new_named(name).await {
                    Ok(new_session) => {
                        session = new_session; messages.clear();
                        events.send(Event::History(Vec::new())).await.ok();
                        events.send(Event::SessionChanged(session.display_name().to_owned())).await.ok();
                        send_usage(&events, &session).await;
                        events.send(Event::PlanChanged(None)).await.ok();
                        send_context(&events, &session, &config).await;
                    }
                    Err(error) => { events.send(Event::Error(format!("{error:#}"))).await.ok(); }
                }
                Command::Save => match session.save().await {
                    Ok(()) => { events.send(Event::Saved).await.ok(); }
                    Err(error) => { events.send(Event::Error(format!("{error:#}"))).await.ok(); }
                }
                Command::SelectModel(model) if generation.is_none() => match config.set_model(&model) {
                    Ok(()) => {
                        send_settings(&events, &config).await;
                        send_context(&events, &session, &config).await;
                    },
                    Err(error) => { events.send(Event::Error(format!("save model setting: {error:#}"))).await.ok(); }
                },
                Command::NextReasoningEffort if generation.is_none() => match config.next_reasoning_effort() {
                    Ok(()) => send_settings(&events, &config).await,
                    Err(error) => { events.send(Event::Error(format!("save reasoning setting: {error:#}"))).await.ok(); }
                },
                Command::RememberCommand(command) => {
                    if let Err(error) = config.remember_command(&command) {
                        events.send(Event::Error(format!("save command history: {error:#}"))).await.ok();
                    }
                }
                Command::RefreshProject => {
                    request_project(&mut project_requests, &project, ProjectRequest::Refresh, &internal_tx);
                }
                Command::GitDiff(path) => {
                    request_project(&mut project_requests, &project, ProjectRequest::Diff(path), &internal_tx);
                }
                Command::Shutdown(reply) => {
                    if let Some(task) = generation.take() {
                        task.abort();
                        // Same ordering as Cancel: let the generation unwind
                        // before shell-job cleanup starts.
                        task.await.ok();
                    }
                    tools.shutdown().await;
                    session.save().await.ok();
                    reply.send(SessionSummary {
                        name: session.meta.name.clone(),
                        total_tokens: session.meta.total_tokens,
                        total_cost: session.total_cost(),
                    }).ok();
                    break;
                }
                Command::NewSession(_)
                | Command::SelectModel(_)
                | Command::NextReasoningEffort => {}
            },
            Some(event) = internal_rx.recv() => match event {
                InternalEvent::Finished(result) if generation.is_some() => {
                    generation = None; pending_approval = None;
                    // The agent already reaps its jobs on a clean return;
                    // this guarantees no job outlives the turn on any path.
                    tools.cancel_active().await;
                    messages.truncate(messages.len().saturating_sub(1));
                    let TurnResult { completed, compaction, title } = result;
                    if let Some(title) = title {
                        session.set_title(title);
                        events.send(Event::SessionChanged(session.display_name().to_owned())).await.ok();
                    }
                    let mut persisted = Vec::new();
                    if let Some(compaction) = compaction {
                        let marker =
                            Message::system(format!("{COMPACTION_MARKER}\n{}", compaction.summary));
                        session.meta.compaction_summary = Some(compaction.summary);
                        session.meta.compacted_through = compaction.through;
                        messages.push(marker.clone());
                        persisted.push(marker);
                    }
                    messages.extend(completed.clone());
                    persisted.extend(completed);
                    let projected = request_context(&messages, &session.meta);
                    if projected.iter().any(is_ejected_web_result) {
                        session.meta.context_tokens = estimate_tokens(&projected);
                        send_context(&events, &session, &config).await;
                    }
                    let saved = async {
                        session.append(&persisted).await?;
                        session.save().await
                    }.await;
                    if let Err(error) = saved {
                        events.send(Event::Error(format!("save session: {error:#}"))).await.ok();
                    } else {
                        events.send(Event::GenerationFinished).await.ok();
                    }
                    request_project(&mut project_requests, &project, ProjectRequest::Refresh, &internal_tx);
                }
                InternalEvent::Failed(error) if generation.is_some() => {
                    generation = None; pending_approval = None; messages.pop();
                    // A failed turn may leave jobs behind (the agent's clean
                    // return does not run on error); reap them here.
                    tools.cancel_active().await;
                    session.save().await.ok();
                    events.send(Event::Error(error)).await.ok();
                    request_project(&mut project_requests, &project, ProjectRequest::Refresh, &internal_tx);
                }
                InternalEvent::Usage(usage) if generation.is_some() => {
                    session.record_usage(usage.total_tokens, config.active_model().price_per_token);
                    session.meta.context_tokens = usage.total_tokens;
                    send_usage(&events, &session).await;
                    send_context(&events, &session, &config).await;
                }
                InternalEvent::AuxiliaryUsage(usage) if generation.is_some() => {
                    session.record_usage(usage.total_tokens, config.active_model().price_per_token);
                    send_usage(&events, &session).await;
                }
                InternalEvent::PlanUpdated(plan) if generation.is_some() => {
                    session.meta.plan = Some(plan.clone());
                    session.save().await.ok();
                    events.send(Event::PlanChanged(Some(plan))).await.ok();
                }
                InternalEvent::Approval { call, reply } => {
                    if generation.is_none() || pending_approval.is_some() { reply.send(ApprovalDecision::Deny).ok(); }
                    else if session.meta.approved_tools.contains(&call.name) {
                        reply.send(ApprovalDecision::AllowSession).ok();
                    } else {
                        pending_approval = Some(PendingApproval { tool: call.name.clone(), reply });
                        events.send(Event::ApprovalRequested(call)).await.ok();
                    }
                }
                InternalEvent::ProjectChanged(changed) => {
                    project = changed;
                    events.send(Event::ProjectChanged(project.clone())).await.ok();
                    if let Some(next) = project_requests.completed() {
                        spawn_project_task(&project, next, &internal_tx);
                    }
                }
                InternalEvent::ProjectRefresh => {
                    request_project(&mut project_requests, &project, ProjectRequest::Refresh, &internal_tx);
                }
                InternalEvent::Finished(_)
                | InternalEvent::Failed(_)
                | InternalEvent::Usage(_)
                | InternalEvent::AuxiliaryUsage(_)
                | InternalEvent::PlanUpdated(_) => {}
            },
            else => break,
        }
    }
    tools.shutdown().await;
}

fn spawn_project_task(
    project: &ProjectState,
    request: ProjectRequest,
    internal: &mpsc::Sender<InternalEvent>,
) {
    let mut snapshot = project.clone();
    let internal = internal.clone();
    tokio::spawn(async move {
        match request {
            ProjectRequest::Refresh => snapshot.refresh().await,
            ProjectRequest::Diff(path) => snapshot.load_diff(path).await,
        }
        internal
            .send(InternalEvent::ProjectChanged(snapshot))
            .await
            .ok();
    });
}

fn request_project(
    requests: &mut ProjectRequests,
    project: &ProjectState,
    request: ProjectRequest,
    internal: &mpsc::Sender<InternalEvent>,
) {
    if let Some(request) = requests.request(request) {
        spawn_project_task(project, request, internal);
    }
}

async fn send_settings(events: &mpsc::Sender<Event>, config: &Config) {
    events
        .send(Event::SettingsChanged {
            model: config.model_name().to_owned(),
            reasoning_effort: config.effective_reasoning_effort(),
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
    compact_plan_history(&mut context);
    strip_tool_diffs(&mut context);
    context
}

const PLAN_CONTEXT_PREFIX: &str = "Current execution plan (keep it updated with update_plan):\n";

fn apply_plan_context(messages: &mut Vec<Message>, plan: Option<&ExecutionPlan>) {
    compact_plan_history(messages);
    if let Some(plan) = plan {
        messages.insert(
            0,
            Message::system(format!(
                "{PLAN_CONTEXT_PREFIX}{}",
                serde_json::to_string_pretty(plan).unwrap()
            )),
        );
    }
}

fn compact_plan_history(messages: &mut [Message]) -> usize {
    let mut call_ids = HashSet::new();
    let mut compacted = 0;
    for message in messages {
        match message {
            Message::Assistant { tool_calls, .. } => {
                for call in tool_calls
                    .iter_mut()
                    .filter(|call| call.name == "update_plan")
                {
                    call_ids.insert(call.id.clone());
                    call.arguments = serde_json::json!({ "stored": true });
                }
            }
            Message::Tool {
                call_id, content, ..
            } if call_ids.remove(call_id) => {
                *content = r#"{"stored":true,"note":"Latest plan is provided separately."}"#.into();
                compacted += 1;
            }
            _ => {}
        }
    }
    compacted
}

fn strip_tool_diffs(messages: &mut [Message]) -> usize {
    messages
        .iter_mut()
        .filter_map(|message| match message {
            Message::Tool { diff, .. } => diff.take(),
            _ => None,
        })
        .count()
}

fn is_compaction_marker(message: &Message) -> bool {
    matches!(message, Message::System { content } if content.starts_with(COMPACTION_MARKER))
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
async fn turn<P: Provider>(
    provider: Arc<P>,
    tools: &ToolRegistry,
    config: &Config,
    mut messages: Vec<Message>,
    visible_through: usize,
    context_tokens: u64,
    project_prompt: Option<String>,
    generate_title: bool,
    current_plan: Option<ExecutionPlan>,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> Result<TurnResult> {
    let mut compaction = None;
    let estimated = estimate_tokens(&messages).max(context_tokens);
    let max_tokens = config.active_model().max_context_tokens;
    if estimated as f64 >= max_tokens as f64 * config.compaction_threshold as f64 {
        let user = messages.pop().context("missing user message")?;
        let summary = summarize(provider.clone(), config, &messages, events, internal).await?;
        messages = vec![
            Message::system(format!("Conversation summary for continuation:\n{summary}")),
            user,
        ];
        compaction = Some(Compaction {
            summary,
            through: visible_through,
        });
    }
    let persist_from = messages.len().saturating_sub(1);
    let (completed, mid_turn_compaction) = agent(
        provider.clone(),
        tools,
        config,
        messages,
        persist_from,
        visible_through,
        project_prompt,
        current_plan,
        events,
        internal,
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
async fn summarize<P: Provider>(
    provider: Arc<P>,
    config: &Config,
    messages: &[Message],
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> Result<String> {
    events.send(Event::CompactionStarted).await.ok();
    events
        .send(Event::ModelRequestStarted(config.model_id().to_owned()))
        .await
        .ok();
    let mut request_messages = vec![Message::system(
        "Summarize this conversation for seamless continuation. Preserve requirements, decisions, files, commands, errors, results, and unresolved work. Be dense and factual. Return only the summary."
            .into(),
    )];
    request_messages.extend(messages.iter().cloned());
    let request = CompletionRequest {
        provider: config.provider_name().to_owned(),
        model: config.model_id().to_owned(),
        messages: request_messages,
        temperature: config.effective_temperature(),
        reasoning_effort: config.light_reasoning_effort(),
        max_tokens: Some(
            (config.active_model().max_context_tokens / 8)
                .clamp(512, 4096)
                .try_into()
                .unwrap(),
        ),
        stream: true,
        tools: Vec::new(),
    };
    let mut stream = stream_with_retry(&provider, request, events).await?;
    let mut summary = String::new();
    let mut reasoning = String::new();
    let mut started = false;
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        if !started {
            events.send(Event::ResponseStarted).await.ok();
            started = true;
        }
        match delta {
            ResponseDelta::Text(text) => summary.push_str(&text),
            ResponseDelta::Reasoning(text) => reasoning.push_str(&text),
            ResponseDelta::Usage(usage) => {
                internal.send(InternalEvent::AuxiliaryUsage(usage)).await?;
            }
            ResponseDelta::ToolCall { .. } | ResponseDelta::OutputItem(_) => {}
        }
    }
    let summary = if summary.trim().is_empty() {
        reasoning.trim()
    } else {
        summary.trim()
    };
    if summary.is_empty() {
        bail!("compaction returned an empty summary");
    }
    let summary = summary.to_owned();
    events
        .send(Event::ContextCompacted {
            summary: summary.clone(),
        })
        .await
        .ok();
    Ok(summary)
}

async fn generate_session_title<P: Provider>(
    provider: Arc<P>,
    config: &Config,
    messages: &[Message],
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> String {
    let fallback = fallback_session_title(messages);
    let Some(user) = messages.iter().find_map(|message| match message {
        Message::User { content, .. } => Some(content.as_str()),
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
        let mut stream = stream_with_retry(&provider, request, events).await?;
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
        .and_then(clean_session_title)
        .unwrap_or_else(|| "New Conversation".into())
}

fn estimate_tokens(messages: &[Message]) -> u64 {
    let bytes = messages
        .iter()
        .map(|message| serde_json::to_string(message).map_or(0, |value| value.len()))
        .sum::<usize>();
    (bytes.div_ceil(4) + messages.len() * 4) as u64
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
async fn agent<P: Provider>(
    provider: Arc<P>,
    tools: &ToolRegistry,
    config: &Config,
    mut messages: Vec<Message>,
    mut persist_from: usize,
    user_full_index: usize,
    project_prompt: Option<String>,
    mut current_plan: Option<ExecutionPlan>,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> Result<(Vec<Message>, Option<Compaction>)> {
    let mut compaction = None;
    // One mid-turn compaction per turn: if it cannot free room, the turn
    // fails instead of overflowing the context or looping.
    let mut compacted_mid_turn = false;
    loop {
        events
            .send(Event::ModelRequestStarted(config.model_id().to_owned()))
            .await
            .ok();
        let mut request_messages = messages.clone();
        strip_tool_diffs(&mut request_messages);
        apply_plan_context(&mut request_messages, current_plan.as_ref());
        if let Some(prompt) = &project_prompt {
            request_messages.insert(0, Message::system(prompt.clone()));
        }
        let request = CompletionRequest {
            provider: config.provider_name().to_owned(),
            model: config.model_id().to_owned(),
            messages: request_messages,
            temperature: config.effective_temperature(),
            reasoning_effort: config.effective_reasoning_effort(),
            max_tokens: None,
            stream: true,
            tools: tools.definitions(config.active_model().vision),
        };
        let stream = stream_with_retry(&provider, request, events).await?;
        let (reasoning, text, mut calls, usage, response_items) =
            collect(stream, events, internal).await?;
        // The tool call cap applies between assistant messages, not per turn.
        calls.truncate(MAX_TOOL_CALLS_PER_MESSAGE);
        messages.push(Message::assistant_response(
            text,
            config.model_id().to_owned(),
            reasoning,
            calls.clone(),
            response_items,
        ));
        let mut used_context_tokens = usage.map_or_else(
            || {
                config
                    .active_model()
                    .max_context_tokens
                    .saturating_sub(available_context_tokens(
                        &messages,
                        config,
                        project_prompt.as_deref(),
                        current_plan.as_ref(),
                    ))
            },
            |usage| usage.total_tokens,
        );
        if calls.is_empty() {
            // The turn is done: no job outlives it, so a command the model
            // stopped polling is killed instead of running unattended.
            tools.cancel_active().await;
            return Ok((messages[persist_from..].to_vec(), compaction));
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

        for call in calls {
            let entry = tools.get(&call.name)?;
            // The result is one Tool message: reserve its framing (role,
            // call id) and per-message overhead up front, so the budget
            // bounds the next model request, not just the content.
            let overhead = tool_message_overhead(&call);
            let max_tokens = config.active_model().max_context_tokens;
            let mut remaining = max_tokens.saturating_sub(used_context_tokens);
            if remaining.saturating_sub(overhead) < MIN_TOOL_OUTPUT_TOKENS {
                // Not even the control envelope fits. Compact the
                // conversation up to this turn's user message — the user,
                // the assistant calls, and the results delivered so far
                // all stay, so results still match their calls — and
                // measure the freed context.
                let boundary = persist_from;
                if boundary == 0 || compacted_mid_turn {
                    bail!("context exhausted: no room left for a tool result");
                }
                let summary = summarize(
                    provider.clone(),
                    config,
                    &messages[..boundary],
                    events,
                    internal,
                )
                .await?;
                messages.splice(
                    0..boundary,
                    [Message::system(format!(
                        "Conversation summary for continuation:\n{summary}"
                    ))],
                );
                persist_from = 1;
                compacted_mid_turn = true;
                compaction = Some(Compaction {
                    summary,
                    through: user_full_index,
                });
                used_context_tokens = max_tokens.saturating_sub(available_context_tokens(
                    &messages,
                    config,
                    project_prompt.as_deref(),
                    current_plan.as_ref(),
                ));
                remaining = max_tokens.saturating_sub(used_context_tokens);
                if remaining.saturating_sub(overhead) < MIN_TOOL_OUTPUT_TOKENS {
                    bail!("context exhausted: compaction could not free room for a tool result");
                }
            }
            let approved = match entry.approval {
                Approval::Allow => true,
                Approval::Deny => false,
                Approval::Ask => {
                    let (reply, decision) = oneshot::channel();
                    internal
                        .send(InternalEvent::Approval {
                            call: call.clone(),
                            reply,
                        })
                        .await?;
                    decision.await.unwrap_or(ApprovalDecision::Deny) != ApprovalDecision::Deny
                }
            };
            // The streaming cap mirrors the final truncation, so the chat
            // never shows output the model context will keep beyond it.
            // The floor keeps the control envelope intact near the limit;
            // after the framing reservation, the whole result — content,
            // role, and call id — fits the remaining context.
            let available = remaining.saturating_sub(overhead);
            let output_tokens = (available / 5).max(MIN_TOOL_OUTPUT_TOKENS).min(available);
            let max_streamed_bytes =
                output_tokens.saturating_mul(4).min(usize::MAX as u64) as usize;
            let result = if approved {
                events
                    .send(Event::ToolStarted {
                        call_id: call.id.clone(),
                    })
                    .await
                    .ok();
                let (delta_tx, delta_rx) = mpsc::unbounded_channel();
                let forward = tokio::spawn(forward_tool_output_deltas(
                    call.id.clone(),
                    delta_rx,
                    max_streamed_bytes,
                    events.clone(),
                ));
                let result = entry
                    .tool
                    .run_streamed(call.arguments.clone(), Some(delta_tx), max_streamed_bytes)
                    .await;
                // Let in-flight deltas land before the final result.
                forward.await.ok();
                result
            } else {
                bail_tool_denied(&call.name)
            };
            let (output, image, diff, success) = match result {
                Ok(result) => (result.output, result.image, result.diff, true),
                Err(error) => (format!("Error: {error:#}"), None, None, false),
            };
            let output = truncate_tool_output(output, output_tokens);
            events
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
                internal.send(InternalEvent::ProjectRefresh).await.ok();
            }
            if success && call.name == "update_plan" {
                let plan: ExecutionPlan =
                    serde_json::from_str(&output).context("decode normalized execution plan")?;
                current_plan = Some(plan.clone());
                internal.send(InternalEvent::PlanUpdated(plan)).await?;
            }
            let message = Message::tool(call.id, output, image, diff);
            used_context_tokens =
                used_context_tokens.saturating_add(estimate_tokens(std::slice::from_ref(&message)));
            messages.push(message);
        }
    }
}

fn available_context_tokens(
    messages: &[Message],
    config: &Config,
    project_prompt: Option<&str>,
    current_plan: Option<&ExecutionPlan>,
) -> u64 {
    let mut context = messages.to_vec();
    strip_tool_diffs(&mut context);
    apply_plan_context(&mut context, current_plan);
    if let Some(prompt) = project_prompt {
        context.insert(0, Message::system(prompt.into()));
    }
    config
        .active_model()
        .max_context_tokens
        .saturating_sub(estimate_tokens(&context))
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

fn truncate_tool_output(mut output: String, max_tokens: u64) -> String {
    let max_bytes = max_tokens.saturating_mul(4).min(usize::MAX as u64) as usize;
    if output.len() <= max_bytes {
        return output;
    }
    // The cap is a hard budget: when even the marker no longer fits, keep
    // the head up to the cap instead of overshooting it.
    if max_bytes <= TOOL_OUTPUT_TRUNCATED.len() {
        output.truncate(floor_char_boundary(&output, max_bytes));
        return output;
    }
    output.truncate(floor_char_boundary(
        &output,
        max_bytes - TOOL_OUTPUT_TRUNCATED.len(),
    ));
    output.push_str(TOOL_OUTPUT_TRUNCATED);
    output
}

async fn stream_with_retry<P: Provider>(
    provider: &Arc<P>,
    request: CompletionRequest,
    events: &mpsc::Sender<Event>,
) -> Result<crate::provider::ResponseStream> {
    let mut attempt = 0;
    loop {
        match provider.stream(request.clone()).await {
            Ok(stream) => {
                events.send(Event::ResponseHeadersReceived).await.ok();
                return Ok(stream);
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
        "connection",
        "timed out",
        "timeout",
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
)> {
    let mut reasoning = String::new();
    let mut text = String::new();
    let mut calls: Vec<ToolDraft> = Vec::new();
    let mut usage = None;
    let mut response_items = Vec::new();
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
    Ok((reasoning, text, calls, usage, response_items))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ResponseDelta, mock::MockProvider};
    use crate::tool::{
        Approval, ShellCancelTool, ShellJobManager, ShellPollTool, ShellTool, Tool, ToolResult,
    };
    use async_trait::async_trait;
    use serde_json::{Value, json};

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
                output: args["value"].as_str().unwrap().to_owned(),
                image: None,
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
                output,
                image: None,
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
            None,
            &event_tx,
            &internal_tx,
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
            None,
            &event_tx,
            &internal_tx,
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
            None,
            &event_tx,
            &internal_tx,
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
            None,
            &event_tx,
            &internal_tx,
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
            None,
            &event_tx,
            &internal_tx,
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
            None,
            &event_tx,
            &internal_tx,
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
            None,
            &event_tx,
            &internal_tx,
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
            None,
            &event_tx,
            &internal_tx,
        )
        .await
        .unwrap();

        // user, 2 * cap assistant/tool message pairs, final assistant
        assert_eq!(completed.len(), 1 + 2 * MAX_TOOL_CALLS_PER_MESSAGE * 2 + 1);
        assert_eq!(completed.last().unwrap().content(), "finished");
    }

    #[test]
    fn tool_output_is_truncated_on_a_character_boundary() {
        let output = truncate_tool_output("é".repeat(100), 10);

        assert!(output.ends_with("[tool output truncated]"));
        assert!(output.len() <= 40);
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
            None,
            &event_tx,
            &internal_tx,
        )
        .await
        .unwrap();
        internal_rx.recv().await;

        assert!(matches!(
            &completed[2],
            // 100 - 50 = 50 remaining tokens, minus the 16-token Tool
            // message framing: 34, which floors to the 32-token control
            // minimum: 128 bytes. 50 + 16 + 32 = 98 stays under the
            // 100-token limit, where the framing-less old budget (50 +
            // 128) would have overflowed it.
            Message::Tool { content, .. }
                if content.ends_with("[tool output truncated]") && content.len() <= 128
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
            None,
            &event_tx,
            &internal_tx,
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
                    total_tokens: 190,
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
        config.models[0].max_context_tokens = 200;
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
            None,
            &event_tx,
            &internal_tx,
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
            None,
            &event_tx,
            &internal_tx,
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
                    None,
                    &events,
                    &internal,
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
                    if let InternalEvent::Approval { call, reply } = event {
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
            None,
            &event_tx,
            &internal_tx,
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

        assert_eq!(strip_tool_diffs(&mut context), 1);
        assert!(matches!(
            &transcript[0],
            Message::Tool { diff: Some(diff), .. } if diff.starts_with("large diff")
        ));
        assert!(matches!(&context[0], Message::Tool { diff: None, .. }));
        assert!(estimate_tokens(&context) < estimate_tokens(&transcript) / 10);
    }

    #[test]
    fn model_context_contains_only_the_latest_full_plan() {
        let old_plan = serde_json::json!({
            "plan": [{ "step": "obsolete step", "status": "in_progress" }]
        });
        let mut messages = vec![
            Message::assistant(
                String::new(),
                "model".into(),
                String::new(),
                vec![ToolCall {
                    id: "plan-1".into(),
                    name: "update_plan".into(),
                    arguments: old_plan.clone(),
                }],
            ),
            Message::tool("plan-1".into(), old_plan.to_string(), None, None),
        ];
        let current = ExecutionPlan {
            explanation: Some("revised".into()),
            plan: vec![crate::tool::PlanStep {
                step: "current step".into(),
                status: crate::tool::PlanStatus::InProgress,
            }],
        };

        apply_plan_context(&mut messages, Some(&current));
        let encoded = serde_json::to_string(&messages).unwrap();

        assert!(!encoded.contains("obsolete step"));
        assert_eq!(encoded.matches("current step").count(), 1);
        assert_eq!(
            messages
                .iter()
                .filter(|message| matches!(message, Message::System { content } if content.starts_with(PLAN_CONTEXT_PREFIX)))
                .count(),
            1
        );
        assert!(encoded.contains("Latest plan is provided separately"));
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
        assert!(!is_retryable(&anyhow::anyhow!(
            "server returned 400: invalid request"
        )));
        assert_eq!(
            (0..6).map(retry_delay).collect::<Vec<_>>(),
            [2, 5, 10, 30, 30, 30]
        );
    }

    #[test]
    fn project_requests_serialize_and_coalesce() {
        let mut requests = ProjectRequests::default();
        assert_eq!(
            requests.request(ProjectRequest::Refresh),
            Some(ProjectRequest::Refresh)
        );
        // Requests arriving while one is in flight are coalesced, latest wins.
        assert_eq!(requests.request(ProjectRequest::Refresh), None);
        assert_eq!(
            requests.request(ProjectRequest::Diff(Some(std::path::PathBuf::from("a.rs")))),
            None
        );
        assert_eq!(
            requests.request(ProjectRequest::Diff(Some(std::path::PathBuf::from("b.rs")))),
            None
        );
        assert_eq!(
            requests.completed(),
            Some(ProjectRequest::Diff(Some(std::path::PathBuf::from("b.rs"))))
        );
        // No pending work means no follow-up.
        assert_eq!(requests.completed(), None);
        assert_eq!(
            requests.request(ProjectRequest::Refresh),
            Some(ProjectRequest::Refresh)
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
        config.models[0].max_context_tokens = 100;
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
            80,
            None,
            false,
            None,
            &event_tx,
            &internal_tx,
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
    async fn compaction_accepts_a_reasoning_only_summary() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Reasoning("Dense continuation summary".into()),
            ResponseDelta::Usage(Usage {
                prompt_tokens: 90,
                total_tokens: 100,
            }),
        ]]));
        let mut config = Config::default();
        config.models[0].reasoning_efforts = vec![
            ReasoningEffort::None,
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
        ];
        let (event_tx, _event_rx) = mpsc::channel(8);
        let (internal_tx, mut internal_rx) = mpsc::channel(2);

        let summary = summarize(
            provider.clone(),
            &config,
            &[Message::user("old turn".into())],
            &event_tx,
            &internal_tx,
        )
        .await
        .unwrap();

        assert_eq!(summary, "Dense continuation summary");
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

    #[test]
    fn request_context_projects_the_summary_and_drops_markers() {
        let summary = "dense summary";
        let meta = SessionMeta {
            name: "test".into(),
            title: None,
            plan: None,
            created_at: 0,
            total_tokens: 0,
            total_cost: 0.0,
            cost_complete: true,
            context_tokens: 0,
            compaction_summary: Some(summary.into()),
            compacted_through: 2,
            approved_tools: Vec::new(),
        };
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
            Message::System { content }
                if content == "Conversation summary for continuation:\ndense summary"
        ));
        assert_eq!(context[1], Message::user("continue".into()));
        assert!(context.iter().all(|message| !is_compaction_marker(message)));
    }
}
