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
    session::Session,
    tool::{Approval, ExecutionPlan, ToolDefinition, ToolRegistry},
};
pub use message::{ImageContent, Message, ToolCall};

pub const CANCELLED_BY_USER: &str = "cancelled by user";
const TOOL_OUTPUT_TRUNCATED: &str = "\n[tool output truncated]";

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
    AddContext(String),
    DropContext(String),
    SelectModel(String),
    NextReasoningEffort,
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
    ContextCompacted,
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
                    let request_messages = request_context(&messages, &session);
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
                        task.abort(); pending_approval = None;
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
                        refresh_project(project.clone(), internal_tx.clone());
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
                Command::AddContext(path) => match project.add(&path).await {
                    Ok(()) => { events.send(Event::ProjectChanged(project.clone())).await.ok(); }
                    Err(error) => { events.send(Event::Error(format!("{error:#}"))).await.ok(); }
                }
                Command::DropContext(path) => match project.drop(&path) {
                    Ok(()) => { events.send(Event::ProjectChanged(project.clone())).await.ok(); }
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
                Command::RefreshProject => {
                    refresh_project(project.clone(), internal_tx.clone());
                }
                Command::GitDiff(path) => {
                    let mut changed = project.clone();
                    let internal = internal_tx.clone();
                    tokio::spawn(async move {
                        changed.load_diff(path).await;
                        internal.send(InternalEvent::ProjectChanged(changed)).await.ok();
                    });
                }
                Command::Shutdown(reply) => {
                    if let Some(task) = generation.take() { task.abort(); }
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
                    messages.truncate(messages.len().saturating_sub(1));
                    let TurnResult { completed, compaction, title } = result;
                    if let Some(title) = title {
                        session.set_title(title);
                        events.send(Event::SessionChanged(session.display_name().to_owned())).await.ok();
                    }
                    let mut persisted = Vec::new();
                    if let Some(compaction) = compaction {
                        session.meta.compaction_summary = Some(compaction.summary);
                        session.meta.compacted_through = compaction.through;
                        let marker = Message::system("Context compacted".into());
                        messages.push(marker.clone());
                        persisted.push(marker);
                    }
                    messages.extend(completed.clone());
                    persisted.extend(completed);
                    let projected = request_context(&messages, &session);
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
                    refresh_project(project.clone(), internal_tx.clone());
                }
                InternalEvent::Failed(error) if generation.is_some() => {
                    generation = None; pending_approval = None; messages.pop();
                    session.save().await.ok();
                    events.send(Event::Error(error)).await.ok();
                    refresh_project(project.clone(), internal_tx.clone());
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

fn refresh_project(mut project: ProjectState, internal: mpsc::Sender<InternalEvent>) {
    tokio::spawn(async move {
        project.refresh().await;
        internal
            .send(InternalEvent::ProjectChanged(project))
            .await
            .ok();
    });
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

fn request_context(messages: &[Message], session: &Session) -> Vec<Message> {
    let mut context = if let Some(summary) = &session.meta.compaction_summary {
        let mut context = vec![Message::system(format!(
            "Conversation summary for continuation:\n{summary}"
        ))];
        context.extend(
            messages[session.meta.compacted_through.min(messages.len())..]
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
    matches!(message, Message::System { content } if content == "Context compacted")
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
        let summary = compact(provider.clone(), config, messages, events, internal).await?;
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
    let completed = agent(
        provider.clone(),
        tools,
        config,
        messages,
        persist_from,
        project_prompt,
        current_plan,
        events,
        internal,
    )
    .await?;
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

async fn compact<P: Provider>(
    provider: Arc<P>,
    config: &Config,
    messages: Vec<Message>,
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
    request_messages.extend(messages);
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
    let mut started = false;
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        if !started {
            events.send(Event::ResponseStarted).await.ok();
            started = true;
        }
        match delta {
            ResponseDelta::Text(text) => summary.push_str(&text),
            ResponseDelta::Usage(usage) => {
                internal.send(InternalEvent::Usage(usage)).await?;
            }
            ResponseDelta::Reasoning(_)
            | ResponseDelta::ToolCall { .. }
            | ResponseDelta::OutputItem(_) => {}
        }
    }
    if summary.trim().is_empty() {
        bail!("compaction returned an empty summary");
    }
    events.send(Event::ContextCompacted).await.ok();
    Ok(summary.trim().to_owned())
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

#[allow(clippy::too_many_arguments)]
async fn agent<P: Provider>(
    provider: Arc<P>,
    tools: &ToolRegistry,
    config: &Config,
    mut messages: Vec<Message>,
    persist_from: usize,
    project_prompt: Option<String>,
    mut current_plan: Option<ExecutionPlan>,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> Result<Vec<Message>> {
    for _ in 0..64 {
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
        let (reasoning, text, calls, usage, response_items) =
            collect(stream, events, internal).await?;
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
            return Ok(messages[persist_from..].to_vec());
        }

        for (index, call) in calls.into_iter().enumerate() {
            events
                .send(Event::ToolCallFinished {
                    index,
                    call: call.clone(),
                })
                .await
                .ok();
            let entry = tools.get(&call.name)?;
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
            let result = if approved {
                events
                    .send(Event::ToolStarted {
                        call_id: call.id.clone(),
                    })
                    .await
                    .ok();
                entry.tool.run(call.arguments.clone()).await
            } else {
                bail_tool_denied(&call.name)
            };
            let (output, image, diff, success) = match result {
                Ok(result) => (result.output, result.image, result.diff, true),
                Err(error) => (format!("Error: {error:#}"), None, None, false),
            };
            let output_tokens = config
                .active_model()
                .max_context_tokens
                .saturating_sub(used_context_tokens)
                / 5;
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
    bail!("tool loop exceeded 64 model turns")
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

fn truncate_tool_output(mut output: String, max_tokens: u64) -> String {
    let max_bytes = max_tokens.saturating_mul(4).min(usize::MAX as u64) as usize;
    if output.len() <= max_bytes {
        return output;
    }
    let mut end = max_bytes.saturating_sub(TOOL_OUTPUT_TRUNCATED.len());
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    output.truncate(end);
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
    use crate::tool::{Approval, Tool, ToolResult};
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
        let completed = agent(
            provider,
            &ToolRegistry::default(),
            &Config::default(),
            vec![Message::user("hi".into())],
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
        let completed = agent(
            provider,
            &tools,
            &Config::default(),
            vec![Message::user("run it".into())],
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

        let completed = agent(
            provider,
            &tools,
            &config,
            vec![Message::user("run it".into())],
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
            Message::Tool { content, .. }
                if content.ends_with("[tool output truncated]") && content.len() <= 40
        ));
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
        assert!(matches!(
            internal_rx.recv().await,
            Some(InternalEvent::Usage(Usage {
                total_tokens: 100,
                ..
            }))
        ));
    }
}
