use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{
    project::ProjectState,
    runtime::{
        ApprovalDecision, CANCELLED_BY_USER, COMPACTION_MARKER, Event, FileContent, ImageContent,
        Message, ReasoningEffort, ToolCall,
    },
    tool::ExecutionPlan,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(from = "TimerValue", into = "TimerValue")]
pub struct Timer {
    elapsed: Duration,
    started: Option<Instant>,
}

#[derive(Deserialize, Serialize)]
struct TimerValue {
    elapsed_ms: u64,
    running: bool,
}

impl From<TimerValue> for Timer {
    fn from(value: TimerValue) -> Self {
        Self {
            elapsed: Duration::from_millis(value.elapsed_ms),
            started: value.running.then(Instant::now),
        }
    }
}

impl From<Timer> for TimerValue {
    fn from(timer: Timer) -> Self {
        Self {
            elapsed_ms: timer.value().as_millis() as u64,
            running: timer.started.is_some(),
        }
    }
}

impl Timer {
    pub fn value(&self) -> Duration {
        self.elapsed + self.started.map_or(Duration::ZERO, |start| start.elapsed())
    }
    pub fn running(&self) -> bool {
        self.started.is_some()
    }
    fn resume(&mut self) {
        if self.started.is_none() {
            self.started = Some(Instant::now());
        }
    }
    fn pause(&mut self) {
        self.elapsed = self.value();
        self.started = None;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    User,
    Steer,
    Assistant,
    Status,
    System,
    Error,
    Thinking,
    Tool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Streaming,
    Pending,
    WaitingApproval,
    Running,
    Done,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolView {
    pub call_id: Option<String>,
    pub name: String,
    pub arguments: String,
    pub output: Option<String>,
    pub diff: Option<String>,
    pub status: ToolStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Block {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_request: Option<String>,
    pub id: String,
    pub kind: BlockKind,
    pub content: String,
    pub model: String,
    pub images: Vec<ImageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<FileContent>,
    pub summary: Option<String>,
    pub tool: Option<ToolView>,
    pub timer: Timer,
    pub queued: bool,
    /// The agent that produced this assistant block; `None` for the
    /// built-in assistant and for history written before agents existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Total time the turn took, on the turn's final assistant block.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Approval {
    pub id: String,
    pub call: ToolCall,
}

/// One child session linked to a `subagent` invocation, as shown in the
/// parent's snapshot. `status` is `running` while the invocation is active,
/// then the terminal state from the call's structured result.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct ChildLink {
    pub session: String,
    pub agent: String,
    #[serde(default)]
    pub prompt: String,
    pub tool_call_id: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SessionState {
    pub title: String,
    pub turn_id: Option<String>,
    pub compacting: bool,
    pub phase: String,
    pub model: String,
    pub response_model: String,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub settings_revision: u64,
    pub total_tokens: u64,
    pub total_cost: Option<f64>,
    pub context_tokens: u64,
    pub max_context_tokens: u64,
    pub approval: Option<Approval>,
    pub queued_steers: usize,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub output_tokens: u64,
    pub generation_ms: u64,
    /// The selected agent ID; `None` is the built-in assistant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The child session the active `subagent` call is waiting on, if any.
    /// While set, a user message sent from this session steers that child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<String>,
    /// Every child session this session created, with terminal state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ChildLink>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Snapshot {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<crate::session::ParentRef>,
    pub seq: u64,
    pub blocks: Vec<Block>,
    pub state: SessionState,
    pub plan: Option<ExecutionPlan>,
    pub project: ProjectState,
    pub assistant_id: Option<String>,
    pub reasoning_id: Option<String>,
    pub drafts: HashMap<usize, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Change {
    Insert {
        before: Option<String>,
        block: Block,
    },
    Replace {
        block: Block,
    },
    Append {
        block_id: String,
        field: String,
        text: String,
    },
    State {
        state: SessionState,
    },
    Plan {
        plan: Option<ExecutionPlan>,
    },
    Project {
        project: ProjectState,
    },
}

pub struct Projection {
    pub snapshot: Snapshot,
    assistant: Option<usize>,
    reasoning: Option<usize>,
    drafts: HashMap<usize, usize>,
    calls: HashMap<String, usize>,
    next_id: u64,
    raw_request: Option<String>,
    pending_raw: Vec<usize>,
}

impl Projection {
    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot.assistant_id = self.assistant.map(|i| snapshot.blocks[i].id.clone());
        snapshot.reasoning_id = self.reasoning.map(|i| snapshot.blocks[i].id.clone());
        snapshot.drafts = self
            .drafts
            .iter()
            .map(|(draft, i)| (*draft, snapshot.blocks[*i].id.clone()))
            .collect();
        snapshot
    }
    pub fn new(session_id: String) -> Self {
        let mut snapshot = Snapshot {
            session_id: session_id.clone(),
            ..Snapshot::default()
        };
        snapshot.state.title = session_id;
        snapshot.state.phase = "idle".into();
        Self {
            snapshot,
            assistant: None,
            reasoning: None,
            drafts: HashMap::new(),
            calls: HashMap::new(),
            next_id: 0,
            raw_request: None,
            pending_raw: Vec::new(),
        }
    }

    fn block(&mut self, kind: BlockKind, content: String) -> Block {
        self.next_id += 1;
        Block {
            raw_request: if matches!(kind, BlockKind::User | BlockKind::Steer) {
                None
            } else {
                self.raw_request.clone()
            },
            id: self.next_id.to_string(),
            kind,
            content,
            model: String::new(),
            images: Vec::new(),
            file: None,
            summary: None,
            tool: None,
            timer: Timer::default(),
            queued: false,
            agent: None,
            duration_ms: None,
        }
    }

    fn push(&mut self, block: Block, changes: &mut Vec<Change>) -> usize {
        changes.push(Change::Insert {
            before: None,
            block: block.clone(),
        });
        if matches!(block.kind, BlockKind::User | BlockKind::Steer) {
            self.pending_raw.push(self.snapshot.blocks.len());
        }
        self.snapshot.blocks.push(block);
        self.snapshot.blocks.len() - 1
    }

    fn replace(&self, index: usize, changes: &mut Vec<Change>) {
        changes.push(Change::Replace {
            block: self.snapshot.blocks[index].clone(),
        });
    }

    fn pause_reasoning(&mut self, changes: &mut Vec<Change>) {
        if let Some(index) = self.reasoning.take() {
            self.snapshot.blocks[index].timer.pause();
            self.replace(index, changes);
        }
    }

    fn delivered(&mut self, count: usize, changes: &mut Vec<Change>) {
        let mut left = count;
        for index in 0..self.snapshot.blocks.len() {
            if left == 0 {
                break;
            }
            if self.snapshot.blocks[index].queued {
                self.snapshot.blocks[index].queued = false;
                self.replace(index, changes);
                left -= 1;
            }
        }
        self.snapshot.state.queued_steers = self.snapshot.state.queued_steers.saturating_sub(count);
    }

    fn history(&mut self, messages: &[Message], changes: &mut Vec<Change>) {
        for message in messages {
            if let Message::Assistant { raw_request, .. } = message {
                self.set_raw_request(raw_request.clone(), changes);
                self.pending_raw.clear();
            }
            match message {
                Message::User { content, images } | Message::Steer { content, images } => {
                    let kind = if matches!(message, Message::Steer { .. }) {
                        BlockKind::Steer
                    } else {
                        BlockKind::User
                    };
                    // The model-facing runtime context pinned to the message
                    // never renders in the chat.
                    let content = crate::runtime::strip_runtime_context(content).to_owned();
                    let mut block = self.block(kind, content);
                    block.images = images.clone();
                    self.push(block, changes);
                }
                Message::System {
                    content,
                    raw_request,
                } => {
                    if raw_request.is_some() {
                        self.set_raw_request(raw_request.clone(), changes);
                    }
                    if !content.starts_with(COMPACTION_MARKER) {
                        self.pending_raw.clear();
                    }
                    let mut block = self.block(BlockKind::System, content.clone());
                    if let Some(summary) = content.strip_prefix(COMPACTION_MARKER) {
                        block.content = "context compacted".into();
                        block.summary = Some(summary.trim().into());
                    }
                    self.push(block, changes);
                }
                Message::Assistant {
                    content,
                    reasoning,
                    model,
                    tool_calls,
                    duration_ms,
                    agent,
                    ..
                } => {
                    if !reasoning.is_empty() {
                        let block = self.block(BlockKind::Thinking, reasoning.clone());
                        self.push(block, changes);
                    }
                    if !content.is_empty() {
                        let kind = if tool_calls.is_empty() {
                            BlockKind::Assistant
                        } else {
                            BlockKind::Status
                        };
                        let mut block = self.block(kind, content.clone());
                        block.model = model.clone();
                        block.duration_ms = *duration_ms;
                        block.agent = agent.clone();
                        self.push(block, changes);
                    }
                    for call in tool_calls {
                        let mut block = self.block(BlockKind::Tool, String::new());
                        block.tool = Some(ToolView {
                            call_id: Some(call.id.clone()),
                            name: call.name.clone(),
                            arguments: serde_json::to_string_pretty(&call.arguments).unwrap(),
                            output: None,
                            diff: None,
                            status: ToolStatus::Pending,
                        });
                        let index = self.push(block, changes);
                        self.calls.insert(call.id.clone(), index);
                    }
                }
                Message::Tool {
                    call_id,
                    content,
                    image,
                    file,
                    diff,
                } => {
                    if let Some(&index) = self.calls.get(call_id) {
                        let block = &mut self.snapshot.blocks[index];
                        if let Some(image) = image {
                            block.images.push(image.clone());
                        }
                        if file.is_some() {
                            block.file = file.clone();
                        }
                        let tool = block.tool.as_mut().unwrap();
                        tool.output = Some(content.clone());
                        tool.diff = diff.clone();
                        tool.status = if content.starts_with("Error:") {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Done
                        };
                        self.replace(index, changes);
                    }
                }
            }
        }
    }

    fn set_raw_request(&mut self, id: Option<String>, changes: &mut Vec<Change>) {
        self.raw_request = id.clone();
        for index in self.pending_raw.clone() {
            let block = &mut self.snapshot.blocks[index];
            if !block.queued {
                block.raw_request = id.clone();
                self.replace(index, changes);
            }
        }
    }

    pub fn apply(&mut self, event: &Event) -> Vec<Change> {
        let mut changes = Vec::new();
        let mut state_changed = true;
        match event {
            Event::RawRequest(id) => self.set_raw_request(id.clone(), &mut changes),
            Event::History(messages) => self.history(messages, &mut changes),
            Event::MessageAccepted(message) => {
                self.history(std::slice::from_ref(message), &mut changes);
                if matches!(message, Message::Steer { .. }) {
                    self.snapshot.state.queued_steers += 1;
                    let index = self.snapshot.blocks.len() - 1;
                    self.snapshot.blocks[index].queued = true;
                    self.replace(index, &mut changes);
                }
            }
            Event::OperationStarted { id, compacting } => {
                self.snapshot.state.turn_id = Some(id.clone());
                self.snapshot.state.compacting = *compacting;
                self.snapshot.state.phase = if *compacting {
                    "compacting"
                } else {
                    "connecting"
                }
                .into();
                self.snapshot.state.error = None;
            }
            Event::SessionChanged(title) => self.snapshot.state.title = title.clone(),
            Event::UsageChanged {
                total_tokens,
                total_cost,
            } => {
                self.snapshot.state.total_tokens = *total_tokens;
                self.snapshot.state.total_cost = *total_cost;
            }
            Event::ContextChanged { tokens, max_tokens } => {
                self.snapshot.state.context_tokens = *tokens;
                self.snapshot.state.max_context_tokens = *max_tokens;
            }
            Event::SettingsChanged {
                model,
                reasoning_effort,
                agent,
            } => {
                self.snapshot.state.model = model.clone();
                self.snapshot.state.reasoning_effort = *reasoning_effort;
                self.snapshot.state.agent = agent.clone();
            }
            Event::DelegationChanged { child, children } => {
                self.snapshot.state.delegation = child.clone();
                self.snapshot.state.children = children.clone();
            }
            Event::SteerReceipt { to, agent } => {
                let mut block = self.block(BlockKind::Status, format!("steered to {agent} ({to})"));
                block.queued = false;
                self.push(block, &mut changes);
            }
            Event::SettingsRevision(revision) => self.snapshot.state.settings_revision = *revision,
            Event::ProjectChanged(project) => {
                self.snapshot.project = project.clone();
                self.snapshot.project.git_diff.clear();
                self.snapshot.project.git_diff_path = None;
                changes.push(Change::Project {
                    project: self.snapshot.project.clone(),
                });
                state_changed = false;
            }
            Event::PlanChanged(plan) => {
                self.snapshot.plan = plan.clone();
                changes.push(Change::Plan { plan: plan.clone() });
                state_changed = false;
            }
            Event::GenerationStarted => {
                self.snapshot.state.output_tokens = 0;
                self.snapshot.state.generation_ms = 0;
                self.snapshot.state.error = None;
                self.snapshot.state.notice = None;
            }
            Event::ModelRequestStarted(model) => {
                self.pause_reasoning(&mut changes);
                self.assistant = None;
                self.drafts.clear();
                self.snapshot.state.response_model = model.clone();
                self.snapshot.state.phase = "connecting".into();
            }
            Event::ResponseHeadersReceived => {
                self.snapshot.state.phase = "waiting".into();
                self.snapshot.state.notice = None;
            }
            Event::ResponseStarted => self.snapshot.state.phase = "generating".into(),
            Event::ModelResponseFinished {
                output_tokens,
                duration,
            } => {
                self.snapshot.state.output_tokens += output_tokens;
                self.snapshot.state.generation_ms += duration.as_millis() as u64;
            }
            Event::TextDelta(delta) | Event::ReasoningDelta(delta) => {
                self.pending_raw.retain(|&i| self.snapshot.blocks[i].queued);
                let thinking = matches!(event, Event::ReasoningDelta(_));
                if !thinking {
                    self.pause_reasoning(&mut changes);
                }
                let current = if thinking {
                    self.reasoning
                } else {
                    self.assistant
                };
                let index = match current {
                    Some(index) => index,
                    None => {
                        let mut block = self.block(
                            if thinking {
                                BlockKind::Thinking
                            } else {
                                BlockKind::Assistant
                            },
                            String::new(),
                        );
                        block.model = self.snapshot.state.response_model.clone();
                        block.agent = self.snapshot.state.agent.clone();
                        if thinking {
                            block.timer.resume();
                        }
                        let index = self.push(block, &mut changes);
                        if thinking {
                            self.reasoning = Some(index);
                        } else {
                            self.assistant = Some(index);
                        }
                        index
                    }
                };
                let block = &mut self.snapshot.blocks[index];
                block.content.push_str(delta);
                changes.push(Change::Append {
                    block_id: block.id.clone(),
                    field: "content".into(),
                    text: delta.clone(),
                });
                state_changed = false;
            }
            Event::ToolCallDelta {
                index,
                name,
                arguments,
            } => {
                self.pending_raw.retain(|&i| self.snapshot.blocks[i].queued);
                self.pause_reasoning(&mut changes);
                if let Some(index) = self.assistant
                    && self.snapshot.blocks[index].kind != BlockKind::Status
                {
                    self.snapshot.blocks[index].kind = BlockKind::Status;
                    self.replace(index, &mut changes);
                }
                let block_index = match self.drafts.get(index) {
                    Some(index) => *index,
                    None => {
                        let mut block = self.block(BlockKind::Tool, String::new());
                        block.timer.resume();
                        block.tool = Some(ToolView {
                            call_id: None,
                            name: name.clone().unwrap_or_default(),
                            arguments: String::new(),
                            output: None,
                            diff: None,
                            status: ToolStatus::Streaming,
                        });
                        let block_index = self.push(block, &mut changes);
                        self.drafts.insert(*index, block_index);
                        block_index
                    }
                };
                if let Some(name) = name {
                    self.snapshot.blocks[block_index]
                        .tool
                        .as_mut()
                        .unwrap()
                        .name = name.clone();
                    self.replace(block_index, &mut changes);
                }
                let block = &mut self.snapshot.blocks[block_index];
                block.tool.as_mut().unwrap().arguments.push_str(arguments);
                changes.push(Change::Append {
                    block_id: block.id.clone(),
                    field: "arguments".into(),
                    text: arguments.clone(),
                });
                state_changed = false;
            }
            Event::ToolCallFinished { index, call } => {
                if let Some(index) = self.drafts.remove(index) {
                    let tool = self.snapshot.blocks[index].tool.as_mut().unwrap();
                    tool.call_id = Some(call.id.clone());
                    tool.name = call.name.clone();
                    tool.arguments = serde_json::to_string_pretty(&call.arguments).unwrap();
                    tool.status = ToolStatus::Pending;
                    self.calls.insert(call.id.clone(), index);
                    self.replace(index, &mut changes);
                }
                state_changed = false;
            }
            Event::ApprovalRequested { approval_id, call } => {
                for index in 0..self.snapshot.blocks.len() {
                    let block = &mut self.snapshot.blocks[index];
                    if block.tool.as_ref().is_some_and(|tool| {
                        matches!(tool.status, ToolStatus::Streaming | ToolStatus::Pending)
                    }) {
                        block.timer.pause();
                        self.replace(index, &mut changes);
                    }
                }
                if let Some(&index) = self.calls.get(&call.id) {
                    self.snapshot.blocks[index].tool.as_mut().unwrap().status =
                        ToolStatus::WaitingApproval;
                    self.replace(index, &mut changes);
                }
                self.snapshot.state.approval = Some(Approval {
                    id: approval_id.clone(),
                    call: call.clone(),
                });
                self.snapshot.state.phase = "approval".into();
            }
            Event::ApprovalResolved { tool, decision, .. } => {
                self.snapshot.state.approval = None;
                let text = match decision {
                    ApprovalDecision::AllowOnce => format!("Tool approved: {tool} · once"),
                    ApprovalDecision::AllowSession => format!("Tool approved: {tool} · session"),
                    ApprovalDecision::Deny => format!("Tool denied: {tool}"),
                };
                let block = self.block(BlockKind::System, text);
                self.push(block, &mut changes);
            }
            Event::ToolStarted { call_id } => {
                self.snapshot.state.phase = "tool".into();
                if let Some(&index) = self.calls.get(call_id) {
                    let block = &mut self.snapshot.blocks[index];
                    block.timer.resume();
                    block.tool.as_mut().unwrap().status = ToolStatus::Running;
                    self.replace(index, &mut changes);
                }
            }
            Event::ToolOutputDelta { call_id, delta } => {
                if let Some(&index) = self.calls.get(call_id) {
                    let block = &mut self.snapshot.blocks[index];
                    let tool = block.tool.as_mut().unwrap();
                    if tool.status == ToolStatus::Running {
                        tool.output.get_or_insert_with(String::new).push_str(delta);
                        changes.push(Change::Append {
                            block_id: block.id.clone(),
                            field: "output".into(),
                            text: delta.clone(),
                        });
                    }
                }
                state_changed = false;
            }
            Event::ToolResult {
                call_id,
                output,
                success,
                diff,
            } => {
                if let Some(child) = self
                    .snapshot
                    .state
                    .children
                    .iter_mut()
                    .find(|child| child.tool_call_id == *call_id)
                {
                    if let Ok(value) = serde_json::from_str::<serde_json::Value>(output)
                        && let Some(status) =
                            value.get("status").and_then(serde_json::Value::as_str)
                    {
                        child.status = status.to_owned();
                    }
                }
                if let Some(&index) = self.calls.get(call_id) {
                    let block = &mut self.snapshot.blocks[index];
                    block.timer.pause();
                    let tool = block.tool.as_mut().unwrap();
                    tool.output = Some(output.clone());
                    tool.diff = diff.clone();
                    tool.status = if *success {
                        ToolStatus::Done
                    } else {
                        ToolStatus::Failed
                    };
                    self.replace(index, &mut changes);
                }
            }
            Event::ToolImage { call_id, image } => {
                if let Some(&index) = self.calls.get(call_id) {
                    self.snapshot.blocks[index].images.push(image.clone());
                    self.replace(index, &mut changes);
                }
                state_changed = false;
            }
            Event::ToolFile { call_id, file } => {
                if let Some(&index) = self.calls.get(call_id) {
                    self.snapshot.blocks[index].file = Some(file.clone());
                    self.replace(index, &mut changes);
                }
                state_changed = false;
            }
            Event::SteersDelivered(count) => self.delivered(*count, &mut changes),
            Event::Retrying { seconds } => {
                self.snapshot.state.phase = "retrying".into();
                self.snapshot.state.notice = Some(format!("retrying in {seconds}s"));
            }
            Event::CompactionStarted => {
                self.snapshot.state.phase = "compacting".into();
                self.snapshot.state.notice = Some("compacting context".into());
            }
            Event::ContextCompacted { summary } => {
                let index = if self.snapshot.state.turn_id.is_some()
                    && !self.snapshot.state.compacting
                {
                    self.snapshot
                        .blocks
                        .iter()
                        .rposition(|block| matches!(block.kind, BlockKind::User | BlockKind::Steer))
                        .unwrap_or(self.snapshot.blocks.len())
                } else {
                    self.snapshot.blocks.len()
                };
                let mut block = self.block(BlockKind::System, "context compacted".into());
                block.summary = Some(summary.clone());
                changes.push(Change::Insert {
                    before: self
                        .snapshot
                        .blocks
                        .get(index)
                        .map(|block| block.id.clone()),
                    block: block.clone(),
                });
                self.snapshot.blocks.insert(index, block);
                for pointer in &mut self.pending_raw {
                    if *pointer >= index {
                        *pointer += 1;
                    }
                }
                for pointer in self
                    .assistant
                    .iter_mut()
                    .chain(self.reasoning.iter_mut())
                    .chain(self.calls.values_mut())
                    .chain(self.drafts.values_mut())
                {
                    if *pointer >= index {
                        *pointer += 1;
                    }
                }
                self.snapshot.state.notice = None;
            }
            Event::GenerationFinished { .. } | Event::GenerationCancelled | Event::Error(_) => {
                if let Event::GenerationFinished {
                    duration: Some(duration),
                } = event
                    && let Some(index) = self
                        .snapshot
                        .blocks
                        .iter()
                        .rposition(|block| block.kind == BlockKind::Assistant)
                {
                    self.snapshot.blocks[index].duration_ms = Some(duration.as_millis() as u64);
                    self.replace(index, &mut changes);
                }
                if matches!(event, Event::GenerationFinished { .. }) {
                    self.pending_raw.retain(|&i| self.snapshot.blocks[i].queued);
                } else {
                    self.pending_raw.clear();
                }
                self.pause_reasoning(&mut changes);
                self.assistant = None;
                self.drafts.clear();
                for index in 0..self.snapshot.blocks.len() {
                    let block = &mut self.snapshot.blocks[index];
                    if let Some(tool) = &mut block.tool
                        && !matches!(tool.status, ToolStatus::Done | ToolStatus::Failed)
                    {
                        tool.status = ToolStatus::Failed;
                        tool.output.get_or_insert_with(|| match event {
                            Event::Error(error) => format!("Error: {error}"),
                            _ => CANCELLED_BY_USER.into(),
                        });
                        block.timer.pause();
                        self.replace(index, &mut changes);
                    }
                }
                if !matches!(event, Event::GenerationFinished { .. }) {
                    self.delivered(usize::MAX, &mut changes);
                    let (kind, text) = match event {
                        Event::Error(error) => (BlockKind::Error, error.clone()),
                        _ => (BlockKind::System, CANCELLED_BY_USER.into()),
                    };
                    let block = self.block(kind, text);
                    self.push(block, &mut changes);
                }
                self.snapshot.state.phase = if matches!(event, Event::Error(_)) {
                    "error"
                } else {
                    "idle"
                }
                .into();
                if let Event::Error(error) = event {
                    self.snapshot.state.error = Some(error.clone());
                }
                self.snapshot.state.turn_id = None;
                self.snapshot.state.compacting = false;
                self.snapshot.state.approval = None;
                self.snapshot.state.notice = None;
            }
            Event::Notice(notice) => self.snapshot.state.notice = Some(notice.clone()),
            Event::Ready
            | Event::Barrier(_)
            | Event::RefreshProject
            | Event::Update(_)
            | Event::PromptRejected(..)
            | Event::Snapshot(_)
            | Event::Catalog(_)
            | Event::RawData { .. }
            | Event::Diff { .. } => state_changed = false,
        }
        if state_changed {
            changes.push(Change::State {
                state: self.snapshot.state.clone(),
            });
        }
        self.snapshot.seq += 1;
        changes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str) -> FileContent {
        FileContent {
            path: path.into(),
            name: "notes.md".into(),
            size: 42,
            mime_type: "text/markdown".into(),
        }
    }

    #[test]
    fn resubmitted_steer_links_to_its_own_request() {
        let mut projection = Projection::new("session".into());
        projection.apply(&Event::MessageAccepted(Message::user("first".into())));
        projection.apply(&Event::RawRequest(Some("first-request".into())));
        projection.apply(&Event::TextDelta("answer".into()));
        projection.apply(&Event::MessageAccepted(Message::steer(
            "next".into(),
            Vec::new(),
        )));
        projection.apply(&Event::GenerationFinished { duration: None });
        projection.apply(&Event::SteersDelivered(1));
        projection.apply(&Event::ModelRequestStarted("model".into()));
        projection.apply(&Event::RawRequest(Some("next-request".into())));
        assert_eq!(
            projection.snapshot.blocks[0].raw_request.as_deref(),
            Some("first-request")
        );
        assert_eq!(
            projection.snapshot.blocks[2].raw_request.as_deref(),
            Some("next-request")
        );
    }

    #[test]
    fn finished_turn_stamps_its_total_duration_on_the_final_answer() {
        let mut projection = Projection::new("session".into());
        projection.apply(&Event::MessageAccepted(Message::user("work".into())));
        projection.apply(&Event::TextDelta("answer".into()));
        projection.apply(&Event::GenerationFinished {
            duration: Some(Duration::from_millis(1500)),
        });
        assert_eq!(projection.snapshot.blocks[1].duration_ms, Some(1500));

        // An operation without a user-facing answer stamps nothing.
        projection.apply(&Event::MessageAccepted(Message::user("again".into())));
        projection.apply(&Event::TextDelta("second".into()));
        projection.apply(&Event::GenerationFinished { duration: None });
        assert_eq!(projection.snapshot.blocks[3].duration_ms, None);
    }

    #[test]
    fn tool_file_event_lands_on_the_active_tool_block() {
        let mut projection = Projection::new("s1".into());
        projection.apply(&Event::ToolCallDelta {
            index: 0,
            name: Some("send_file".into()),
            arguments: r#"{"path":"notes.md"}"#.into(),
        });
        projection.apply(&Event::ToolCallFinished {
            index: 0,
            call: ToolCall {
                id: "c1".into(),
                name: "send_file".into(),
                arguments: serde_json::json!({ "path": "notes.md" }),
            },
        });
        let changes = projection.apply(&Event::ToolFile {
            call_id: "c1".into(),
            file: file("/tmp/notes.md"),
        });
        let snapshot = projection.snapshot();
        let block = snapshot.blocks.last().unwrap();
        assert_eq!(
            block.file.as_ref().map(|f| f.path.as_str()),
            Some("/tmp/notes.md")
        );
        assert!(
            matches!(changes.last(), Some(Change::Replace { block }) if block.file.is_some()),
            "ToolFile must replace the block so late clients see the file"
        );
    }

    #[test]
    fn history_tool_results_restore_their_file() {
        let mut projection = Projection::new("s1".into());
        projection.apply(&Event::History(vec![
            Message::assistant_response(
                String::new(),
                "model".into(),
                String::new(),
                vec![ToolCall {
                    id: "c1".into(),
                    name: "send_file".into(),
                    arguments: serde_json::json!({ "path": "notes.md" }),
                }],
                Vec::new(),
            ),
            Message::tool_file(
                "c1".into(),
                "sent /tmp/notes.md (42 bytes)".into(),
                file("/tmp/notes.md"),
                None,
            ),
        ]));
        let snapshot = projection.snapshot();
        let block = snapshot.blocks.last().unwrap();
        assert_eq!(block.file.as_ref().map(|f| f.size), Some(42));
    }
}
