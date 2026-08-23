mod message;

use std::{str::FromStr, sync::Arc};

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
    provider::{Provider, ResponseDelta},
    session::Session,
    tool::{Approval, ToolDefinition, ToolRegistry},
};
pub use message::{Message, ToolCall};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

impl FromStr for ReasoningEffort {
    type Err = anyhow::Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            _ => bail!("reasoning effort must be low, medium, or high"),
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        })
    }
}

#[derive(Clone, Debug)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub temperature: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub stream: bool,
    pub tools: Vec<ToolDefinition>,
}

pub enum Command {
    Submit(String),
    Cancel,
    Approve(bool),
    NewSession(Option<String>),
    Save,
    AddContext(String),
    DropContext(String),
    NextModel,
    NextReasoningEffort,
    RefreshProject,
    Shutdown,
}

pub enum Event {
    History(Vec<Message>),
    SessionChanged(String),
    UsageChanged(u64),
    SettingsChanged {
        model: String,
        reasoning_effort: Option<ReasoningEffort>,
    },
    ProjectChanged(ProjectState),
    GenerationStarted,
    ModelRequestStarted(String),
    ResponseStarted,
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
    },
    Retrying {
        seconds: u64,
    },
    GenerationFinished,
    Saved,
    Error(String),
}

enum InternalEvent {
    Finished(Vec<Message>),
    Failed(String),
    Usage(u64),
    Approval {
        call: ToolCall,
        reply: oneshot::Sender<bool>,
    },
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
    let mut pending_approval: Option<oneshot::Sender<bool>> = None;

    events.send(Event::History(messages.clone())).await.ok();
    events
        .send(Event::SessionChanged(session.meta.name.clone()))
        .await
        .ok();
    events
        .send(Event::UsageChanged(session.meta.total_tokens))
        .await
        .ok();
    send_settings(&events, &config).await;
    events
        .send(Event::ProjectChanged(project.clone()))
        .await
        .ok();

    loop {
        tokio::select! {
            Some(command) = commands.recv() => match command {
                Command::Submit(prompt) if generation.is_none() => {
                    let persist_from = messages.len();
                    messages.push(Message::user(prompt));
                    let request_messages = messages.clone();
                    let provider = provider.clone();
                    let tools = tools.clone();
                    let config = config.clone();
                    let project_prompt = project.prompt().await;
                    let events = events.clone();
                    let internal = internal_tx.clone();
                    events.send(Event::GenerationStarted).await.ok();
                    generation = Some(tokio::spawn(async move {
                        let result = match project_prompt {
                            Ok(prompt) => agent(provider, &tools, &config, request_messages, persist_from, prompt, &events, &internal).await,
                            Err(error) => Err(error),
                        };
                        let event = match result {
                            Ok(messages) => InternalEvent::Finished(messages),
                            Err(error) => InternalEvent::Failed(format!("{error:#}")),
                        };
                        internal.send(event).await.ok();
                    }));
                }
                Command::Submit(_) => {}
                Command::Cancel => {
                    if let Some(task) = generation.take() {
                        task.abort(); messages.pop(); pending_approval = None;
                        session.save().await.ok();
                        events.send(Event::History(messages.clone())).await.ok();
                        events.send(Event::GenerationFinished).await.ok();
                    }
                }
                Command::Approve(approved) => {
                    if let Some(reply) = pending_approval.take() { reply.send(approved).ok(); }
                }
                Command::NewSession(name) if generation.is_none() => match Session::new_named(name).await {
                    Ok(new_session) => {
                        session = new_session; messages.clear();
                        events.send(Event::History(Vec::new())).await.ok();
                        events.send(Event::SessionChanged(session.meta.name.clone())).await.ok();
                        events.send(Event::UsageChanged(session.meta.total_tokens)).await.ok();
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
                Command::NextModel if generation.is_none() => match config.next_model() {
                    Ok(()) => send_settings(&events, &config).await,
                    Err(error) => { events.send(Event::Error(format!("save model setting: {error:#}"))).await.ok(); }
                },
                Command::NextReasoningEffort if generation.is_none() => match config.next_reasoning_effort() {
                    Ok(()) => send_settings(&events, &config).await,
                    Err(error) => { events.send(Event::Error(format!("save reasoning setting: {error:#}"))).await.ok(); }
                },
                Command::RefreshProject => {
                    project.refresh().await;
                    events.send(Event::ProjectChanged(project.clone())).await.ok();
                }
                Command::Shutdown => {
                    if let Some(task) = generation.take() { task.abort(); }
                    session.save().await.ok();
                    break;
                }
                Command::NewSession(_) | Command::NextModel | Command::NextReasoningEffort => {}
            },
            Some(event) = internal_rx.recv() => match event {
                InternalEvent::Finished(completed) if generation.is_some() => {
                    generation = None; pending_approval = None;
                    messages.truncate(messages.len().saturating_sub(1));
                    messages.extend(completed.clone());
                    let saved = async {
                        session.append(&completed).await?;
                        session.save().await
                    }.await;
                    if let Err(error) = saved {
                        events.send(Event::Error(format!("save session: {error:#}"))).await.ok();
                    } else {
                        project.refresh().await;
                        events.send(Event::ProjectChanged(project.clone())).await.ok();
                    }
                    events.send(Event::GenerationFinished).await.ok();
                }
                InternalEvent::Failed(error) if generation.is_some() => {
                    generation = None; pending_approval = None; messages.pop();
                    session.save().await.ok();
                    events.send(Event::History(messages.clone())).await.ok();
                    events.send(Event::Error(error)).await.ok();
                }
                InternalEvent::Usage(tokens) if generation.is_some() => {
                    session.meta.total_tokens += tokens;
                    events.send(Event::UsageChanged(session.meta.total_tokens)).await.ok();
                }
                InternalEvent::Approval { call, reply } => {
                    if generation.is_none() || pending_approval.is_some() { reply.send(false).ok(); }
                    else { pending_approval = Some(reply); events.send(Event::ApprovalRequested(call)).await.ok(); }
                }
                InternalEvent::Finished(_) | InternalEvent::Failed(_) | InternalEvent::Usage(_) => {}
            },
            else => break,
        }
    }
}

async fn send_settings(events: &mpsc::Sender<Event>, config: &Config) {
    events
        .send(Event::SettingsChanged {
            model: config.model_name().to_owned(),
            reasoning_effort: config.reasoning_effort,
        })
        .await
        .ok();
}

#[allow(clippy::too_many_arguments)]
async fn agent<P: Provider>(
    provider: Arc<P>,
    tools: &ToolRegistry,
    config: &Config,
    mut messages: Vec<Message>,
    persist_from: usize,
    project_prompt: Option<String>,
    events: &mpsc::Sender<Event>,
    internal: &mpsc::Sender<InternalEvent>,
) -> Result<Vec<Message>> {
    for _ in 0..32 {
        events
            .send(Event::ModelRequestStarted(config.model_id().to_owned()))
            .await
            .ok();
        let mut request_messages = messages.clone();
        if let Some(prompt) = &project_prompt {
            request_messages.insert(0, Message::system(prompt.clone()));
        }
        let request = CompletionRequest {
            model: config.model_id().to_owned(),
            messages: request_messages,
            temperature: config.effective_temperature(),
            reasoning_effort: config.reasoning_effort,
            stream: true,
            tools: tools.definitions(),
        };
        let stream = stream_with_retry(&provider, request, events).await?;
        let (reasoning, text, calls) = collect(stream, events, internal).await?;
        messages.push(Message::assistant(
            text,
            config.model_id().to_owned(),
            reasoning,
            calls.clone(),
        ));
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
                    decision.await.unwrap_or(false)
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
            let (output, success) = match result {
                Ok(result) => (result.output, true),
                Err(error) => (format!("Error: {error:#}"), false),
            };
            events
                .send(Event::ToolResult {
                    call_id: call.id.clone(),
                    output: output.clone(),
                    success,
                })
                .await
                .ok();
            messages.push(Message::tool(call.id, output));
        }
    }
    bail!("tool loop exceeded 32 model turns")
}

async fn stream_with_retry<P: Provider>(
    provider: &Arc<P>,
    request: CompletionRequest,
    events: &mpsc::Sender<Event>,
) -> Result<crate::provider::ResponseStream> {
    let mut attempt = 0;
    loop {
        match provider.stream(request.clone()).await {
            Ok(stream) => return Ok(stream),
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
) -> Result<(String, String, Vec<ToolCall>)> {
    let mut reasoning = String::new();
    let mut text = String::new();
    let mut calls: Vec<ToolDraft> = Vec::new();
    let mut usage = None;
    let mut started = false;
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        if !started {
            events.send(Event::ResponseStarted).await.ok();
            started = true;
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
    Ok((reasoning, text, calls))
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
            })
        }
    }

    #[tokio::test]
    async fn runtime_streams_mock_response_without_a_terminal() {
        let provider = Arc::new(MockProvider::new(vec![vec![
            ResponseDelta::Reasoning("thinking".into()),
            ResponseDelta::Text("hel".into()),
            ResponseDelta::Text("lo".into()),
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
            event_rx.recv().await,
            Some(Event::ModelRequestStarted(_))
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
            ResponseDelta::Usage(321),
        ]]));
        let (event_tx, _event_rx) = mpsc::channel(16);
        let (internal_tx, mut internal_rx) = mpsc::channel(2);

        agent(
            provider,
            &ToolRegistry::default(),
            &Config::default(),
            vec![Message::user("hi".into())],
            0,
            None,
            &event_tx,
            &internal_tx,
        )
        .await
        .unwrap();

        assert!(matches!(
            internal_rx.recv().await,
            Some(InternalEvent::Usage(321))
        ));
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
}
