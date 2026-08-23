use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crate::{
    project::ProjectState,
    runtime::{Event, Message, ToolCall},
};

#[derive(Clone, Copy)]
pub enum MessageKind {
    User,
    Assistant,
    System,
}

#[derive(Clone, Copy)]
pub enum ToolStatus {
    Streaming,
    Pending,
    WaitingApproval,
    Running,
    Done,
    Failed,
}

#[derive(Default)]
pub struct ToolCounter {
    chars: usize,
    line_breaks: usize,
    multiline: bool,
    trailing_backslashes: usize,
}

#[derive(Default)]
pub struct Elapsed {
    started: Option<Instant>,
    duration: Option<Duration>,
}

impl Elapsed {
    fn started() -> Self {
        Self {
            started: Some(Instant::now()),
            duration: Some(Duration::ZERO),
        }
    }

    fn finish(&mut self) {
        if let Some(started) = self.started.take() {
            *self.duration.get_or_insert(Duration::ZERO) += started.elapsed();
        }
    }

    pub fn value(&self) -> Option<Duration> {
        self.duration.map(|duration| {
            duration
                + self
                    .started
                    .map_or(Duration::ZERO, |started| started.elapsed())
        })
    }
}

impl ToolCounter {
    fn push(&mut self, text: &str) {
        self.chars += text.chars().count();
        let mut line_breaks = 0;
        for character in text.chars() {
            match character {
                '\\' => self.trailing_backslashes += 1,
                'n' if self.trailing_backslashes % 2 == 1 => {
                    line_breaks += 1;
                    self.trailing_backslashes = 0;
                }
                '\n' => {
                    line_breaks += 1;
                    self.trailing_backslashes = 0;
                }
                _ => self.trailing_backslashes = 0,
            }
        }
        self.line_breaks += line_breaks;
        self.multiline |= line_breaks > 0;
    }

    pub fn label(&self) -> String {
        if self.multiline {
            format!("{} lines", self.line_breaks + 1)
        } else {
            format!("{} chars", self.chars)
        }
    }
}

pub enum ChatBlock {
    Message {
        label: String,
        content: String,
        model: String,
        kind: MessageKind,
        expanded: bool,
    },
    Thinking {
        content: String,
        expanded: bool,
        elapsed: Elapsed,
    },
    Tool {
        call_id: Option<String>,
        name: String,
        arguments: String,
        output: Option<String>,
        status: ToolStatus,
        expanded: bool,
        counter: ToolCounter,
        elapsed: Elapsed,
    },
}

pub struct UiState {
    pub input: String,
    pub blocks: Vec<ChatBlock>,
    pub generating: bool,
    pub connecting: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub scroll: u16,
    pub session: String,
    pub total_tokens: u64,
    pub context_tokens: u64,
    pub max_context_tokens: u64,
    pub model: String,
    pub reasoning_effort: Option<crate::runtime::ReasoningEffort>,
    pub project: ProjectState,
    pub approval: Option<ToolCall>,
    pub palette_selected: usize,
    pub thinking_expanded: bool,
    pub tools_expanded: bool,
    pub git_panel: bool,
    pub git_diff_mode: bool,
    assistant: Option<usize>,
    reasoning: Option<usize>,
    response_model: String,
    tool_drafts: BTreeMap<usize, usize>,
    tool_calls: BTreeMap<String, usize>,
    selected: Option<usize>,
    input_cursor: usize,
    pasted: Vec<PastedRange>,
}

#[derive(Clone)]
struct PastedRange {
    start: usize,
    end: usize,
    chars: usize,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            blocks: Vec::new(),
            generating: false,
            connecting: false,
            error: None,
            notice: None,
            scroll: 0,
            session: String::new(),
            total_tokens: 0,
            context_tokens: 0,
            max_context_tokens: 0,
            model: String::new(),
            reasoning_effort: None,
            project: ProjectState::default(),
            approval: None,
            palette_selected: 0,
            thinking_expanded: false,
            tools_expanded: false,
            git_panel: true,
            git_diff_mode: false,
            assistant: None,
            reasoning: None,
            response_model: String::new(),
            tool_drafts: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
            selected: None,
            input_cursor: 0,
            pasted: Vec::new(),
        }
    }

    pub fn take_input(&mut self) -> Option<String> {
        let input = self.input.trim().to_owned();
        if input.is_empty() || self.generating {
            return None;
        }
        self.input.clear();
        self.input_cursor = 0;
        self.pasted.clear();
        self.error = None;
        self.notice = None;
        self.scroll = 0;
        Some(input)
    }

    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.input_cursor = self.input.len();
        self.pasted.clear();
        self.palette_selected = 0;
    }

    pub fn clear_input(&mut self) {
        self.set_input(String::new());
    }

    pub fn insert_char(&mut self, character: char) {
        self.shift_pastes(self.input_cursor, character.len_utf8() as isize);
        self.input.insert(self.input_cursor, character);
        self.input_cursor += character.len_utf8();
    }

    pub fn insert_paste(&mut self, text: &str, collapse_at: usize) {
        let chars = text.chars().count();
        let start = self.input_cursor;
        self.shift_pastes(start, text.len() as isize);
        self.input.insert_str(start, text);
        self.input_cursor += text.len();
        if chars > collapse_at {
            self.pasted.push(PastedRange {
                start,
                end: self.input_cursor,
                chars,
            });
            self.pasted.sort_by_key(|range| range.start);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(index) = self
            .pasted
            .iter()
            .position(|range| range.end == self.input_cursor)
        {
            let range = self.pasted.remove(index);
            self.input.replace_range(range.start..range.end, "");
            self.input_cursor = range.start;
            self.shift_pastes(range.end, -((range.end - range.start) as isize));
            return;
        }
        let Some((start, _)) = self.input[..self.input_cursor].char_indices().next_back() else {
            return;
        };
        self.input.replace_range(start..self.input_cursor, "");
        let removed = self.input_cursor - start;
        self.input_cursor = start;
        self.shift_pastes(start + removed, -(removed as isize));
    }

    pub fn delete(&mut self) {
        if let Some(index) = self
            .pasted
            .iter()
            .position(|range| range.start == self.input_cursor)
        {
            let range = self.pasted.remove(index);
            self.input.replace_range(range.start..range.end, "");
            self.shift_pastes(range.end, -((range.end - range.start) as isize));
            return;
        }
        let Some(character) = self.input[self.input_cursor..].chars().next() else {
            return;
        };
        let end = self.input_cursor + character.len_utf8();
        self.input.replace_range(self.input_cursor..end, "");
        self.shift_pastes(end, -(character.len_utf8() as isize));
    }

    pub fn move_input_left(&mut self) {
        if let Some(range) = self
            .pasted
            .iter()
            .find(|range| range.end == self.input_cursor)
        {
            self.input_cursor = range.start;
        } else if let Some((start, _)) = self.input[..self.input_cursor].char_indices().next_back()
        {
            self.input_cursor = start;
        }
    }

    pub fn move_input_right(&mut self) {
        if let Some(range) = self
            .pasted
            .iter()
            .find(|range| range.start == self.input_cursor)
        {
            self.input_cursor = range.end;
        } else if let Some(character) = self.input[self.input_cursor..].chars().next() {
            self.input_cursor += character.len_utf8();
        }
    }

    pub fn input_lines(&self) -> Vec<ratatui::text::Line<'static>> {
        use ratatui::{
            style::{Color, Style},
            text::{Line, Span},
        };

        let mut lines = vec![Line::from(Span::raw(" "))];
        let mut offset = 0;
        for range in &self.pasted {
            push_input_text(&mut lines, &self.input[offset..range.start]);
            lines.last_mut().unwrap().spans.push(Span::styled(
                format!(" Pasted {} chars ", range.chars),
                Style::default().fg(Color::Black).bg(Color::Magenta),
            ));
            offset = range.end;
        }
        push_input_text(&mut lines, &self.input[offset..]);
        lines
    }

    pub fn input_cursor(&self, width: u16) -> (u16, u16) {
        let mut row = 0u16;
        let mut column = 1u16;
        let mut offset = 0;
        for range in &self.pasted {
            if range.start >= self.input_cursor {
                break;
            }
            advance_cursor(
                &self.input[offset..range.start],
                width,
                &mut row,
                &mut column,
            );
            let label = format!(" Pasted {} chars ", range.chars);
            advance_cursor(&label, width, &mut row, &mut column);
            offset = range.end;
        }
        advance_cursor(
            &self.input[offset..self.input_cursor],
            width,
            &mut row,
            &mut column,
        );
        (row, column)
    }

    fn shift_pastes(&mut self, from: usize, delta: isize) {
        for range in self.pasted.iter_mut().filter(|range| range.start >= from) {
            range.start = range.start.checked_add_signed(delta).unwrap();
            range.end = range.end.checked_add_signed(delta).unwrap();
        }
    }

    pub fn push_user(&mut self, content: String) {
        self.blocks.push(ChatBlock::Message {
            label: "You".into(),
            content,
            model: String::new(),
            kind: MessageKind::User,
            expanded: true,
        });
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        self.error = Some(error.into());
    }

    pub fn toggle_thinking_default(&mut self) {
        self.thinking_expanded = !self.thinking_expanded;
        self.notice = Some(format!(
            "thinking blocks default to {}",
            if self.thinking_expanded {
                "expanded"
            } else {
                "collapsed"
            }
        ));
    }

    pub fn toggle_tools_default(&mut self) {
        self.tools_expanded = !self.tools_expanded;
        self.notice = Some(format!(
            "tool blocks default to {}",
            if self.tools_expanded {
                "expanded"
            } else {
                "collapsed"
            }
        ));
    }

    pub fn open_git_diff(&mut self) {
        self.git_panel = true;
        self.git_diff_mode = true;
    }

    pub fn conversation_focused(&self) -> bool {
        self.selected.is_some()
    }
    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn focus_next(&mut self) {
        let sections = self.sections();
        self.selected = match self
            .selected
            .and_then(|selected| sections.iter().position(|index| *index == selected))
        {
            Some(position) if position + 1 < sections.len() => Some(sections[position + 1]),
            Some(_) => None,
            None => sections.last().copied(),
        };
    }

    pub fn focus_previous(&mut self) {
        let sections = self.sections();
        self.selected = match self
            .selected
            .and_then(|selected| sections.iter().position(|index| *index == selected))
        {
            Some(position) if position > 0 => Some(sections[position - 1]),
            Some(_) => None,
            None => sections.last().copied(),
        };
    }

    pub fn select_previous(&mut self) {
        let sections = self.sections();
        if let Some(position) = self
            .selected
            .and_then(|selected| sections.iter().position(|index| *index == selected))
        {
            self.selected = Some(sections[position.saturating_sub(1)]);
        }
    }

    pub fn select_next(&mut self) {
        let sections = self.sections();
        if let Some(position) = self
            .selected
            .and_then(|selected| sections.iter().position(|index| *index == selected))
        {
            self.selected = Some(sections[(position + 1).min(sections.len() - 1)]);
        }
    }

    pub fn focus_input(&mut self) {
        self.selected = None;
    }

    pub fn select(&mut self, index: usize) {
        if self.blocks.get(index).is_some_and(collapsible) {
            self.selected = Some(index);
        }
    }

    pub fn toggle_selected(&mut self) {
        if let Some(index) = self.selected {
            self.toggle(index);
        }
    }

    pub fn toggle(&mut self, index: usize) {
        if let Some(
            ChatBlock::Message {
                kind: MessageKind::User | MessageKind::Assistant,
                expanded,
                ..
            }
            | ChatBlock::Thinking { expanded, .. }
            | ChatBlock::Tool { expanded, .. },
        ) = self.blocks.get_mut(index)
        {
            *expanded = !*expanded;
        }
    }

    fn sections(&self) -> Vec<usize> {
        self.blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| collapsible(block).then_some(index))
            .collect()
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::History(messages) => self.set_history(messages),
            Event::SessionChanged(session) => self.session = session,
            Event::UsageChanged(tokens) => self.total_tokens = tokens,
            Event::ContextChanged { tokens, max_tokens } => {
                self.context_tokens = tokens;
                self.max_context_tokens = max_tokens;
            }
            Event::SettingsChanged {
                model,
                reasoning_effort,
            } => {
                self.model = model;
                self.reasoning_effort = reasoning_effort;
            }
            Event::ProjectChanged(project) => self.project = project,
            Event::GenerationStarted => {
                self.generating = true;
                self.connecting = true;
                self.error = None;
            }
            Event::ModelRequestStarted(model) => {
                self.finish_reasoning();
                self.connecting = true;
                self.response_model = model;
                self.tool_drafts.clear();
                self.assistant = None;
                self.reasoning = None;
            }
            Event::ResponseStarted => {
                self.connecting = false;
                if self
                    .notice
                    .as_ref()
                    .is_some_and(|notice| notice.starts_with("retrying in "))
                {
                    self.notice = None;
                }
            }
            Event::ReasoningDelta(delta) => {
                let block = match self.reasoning {
                    Some(block) => block,
                    None => {
                        self.blocks.push(ChatBlock::Thinking {
                            content: String::new(),
                            expanded: self.thinking_expanded,
                            elapsed: Elapsed::started(),
                        });
                        let block = self.blocks.len() - 1;
                        self.reasoning = Some(block);
                        block
                    }
                };
                if let ChatBlock::Thinking { content, .. } = &mut self.blocks[block] {
                    content.push_str(&delta);
                }
            }
            Event::TextDelta(delta) => {
                self.finish_reasoning();
                let block = match self.assistant {
                    Some(block) => block,
                    None => {
                        self.blocks.push(ChatBlock::Message {
                            label: "Assistant".into(),
                            content: String::new(),
                            model: self.response_model.clone(),
                            kind: MessageKind::Assistant,
                            expanded: true,
                        });
                        let block = self.blocks.len() - 1;
                        self.assistant = Some(block);
                        block
                    }
                };
                if let ChatBlock::Message { content, .. } = &mut self.blocks[block] {
                    content.push_str(&delta);
                }
            }
            Event::ToolCallDelta {
                index,
                name,
                arguments,
            } => {
                self.finish_reasoning();
                let block = match self.tool_drafts.get(&index).copied() {
                    Some(block) => block,
                    None => {
                        self.blocks.push(ChatBlock::Tool {
                            call_id: None,
                            name: String::new(),
                            arguments: String::new(),
                            output: None,
                            status: ToolStatus::Streaming,
                            expanded: self.tools_expanded,
                            counter: ToolCounter::default(),
                            elapsed: Elapsed::started(),
                        });
                        let block = self.blocks.len() - 1;
                        self.tool_drafts.insert(index, block);
                        block
                    }
                };
                if let ChatBlock::Tool {
                    name: block_name,
                    arguments: block_arguments,
                    counter,
                    ..
                } = &mut self.blocks[block]
                {
                    if let Some(name) = name {
                        *block_name = name;
                    }
                    counter.push(&arguments);
                    block_arguments.push_str(&arguments);
                }
            }
            Event::ToolCallFinished { index, call } => {
                if let Some(block) = self.tool_drafts.remove(&index) {
                    if let ChatBlock::Tool {
                        call_id,
                        name,
                        arguments,
                        status,
                        ..
                    } = &mut self.blocks[block]
                    {
                        *call_id = Some(call.id.clone());
                        *name = call.name;
                        *arguments =
                            serde_json::to_string_pretty(&call.arguments).unwrap_or_default();
                        *status = ToolStatus::Pending;
                    }
                    self.tool_calls.insert(call.id, block);
                }
            }
            Event::ApprovalRequested(call) => {
                self.set_tool_status(&call.id, ToolStatus::WaitingApproval);
                self.approval = Some(call);
            }
            Event::ToolStarted { call_id } => self.set_tool_status(&call_id, ToolStatus::Running),
            Event::ToolResult {
                call_id,
                output,
                success,
            } => {
                if let Some(block) = self.tool_calls.get(&call_id).copied()
                    && let ChatBlock::Tool {
                        output: block_output,
                        status,
                        counter,
                        elapsed,
                        ..
                    } = &mut self.blocks[block]
                {
                    counter.push(&output);
                    elapsed.finish();
                    *block_output = Some(output);
                    *status = if success {
                        ToolStatus::Done
                    } else {
                        ToolStatus::Failed
                    };
                }
            }
            Event::Retrying { seconds } => {
                self.connecting = true;
                self.notice = Some(format!("retrying in {seconds}s · Esc to cancel"));
            }
            Event::CompactionStarted => {
                self.notice = Some("compacting context".into());
            }
            Event::ContextCompacted => {
                let block = ChatBlock::Message {
                    label: "System".into(),
                    content: "Context compacted".into(),
                    model: String::new(),
                    kind: MessageKind::System,
                    expanded: true,
                };
                let before_user = self.blocks.iter().rposition(|block| {
                    matches!(
                        block,
                        ChatBlock::Message {
                            kind: MessageKind::User,
                            ..
                        }
                    )
                });
                if let Some(index) = before_user {
                    self.blocks.insert(index, block);
                } else {
                    self.blocks.push(block);
                }
                self.notice = Some("context compacted".into());
            }
            Event::GenerationFinished => {
                self.finish_reasoning();
                self.generating = false;
                self.connecting = false;
                self.approval = None;
            }
            Event::Saved => self.notice = Some("session saved".into()),
            Event::Error(error) => {
                self.finish_reasoning();
                self.generating = false;
                self.connecting = false;
                self.approval = None;
                self.error = Some(error);
            }
        }
    }

    fn set_tool_status(&mut self, call_id: &str, value: ToolStatus) {
        if let Some(block) = self.tool_calls.get(call_id).copied()
            && let ChatBlock::Tool { status, .. } = &mut self.blocks[block]
        {
            *status = value;
        }
    }

    fn finish_reasoning(&mut self) {
        if let Some(block) = self.reasoning.take()
            && let ChatBlock::Thinking { elapsed, .. } = &mut self.blocks[block]
        {
            elapsed.finish();
        }
    }

    fn set_history(&mut self, messages: Vec<Message>) {
        self.blocks.clear();
        self.tool_calls.clear();
        for message in messages {
            match message {
                Message::System { content } => self.blocks.push(ChatBlock::Message {
                    label: "System".into(),
                    content,
                    model: String::new(),
                    kind: MessageKind::System,
                    expanded: true,
                }),
                Message::User { content } => self.blocks.push(ChatBlock::Message {
                    label: "You".into(),
                    content,
                    model: String::new(),
                    kind: MessageKind::User,
                    expanded: true,
                }),
                Message::Assistant {
                    content,
                    model,
                    reasoning,
                    tool_calls,
                } => {
                    if !reasoning.is_empty() {
                        self.blocks.push(ChatBlock::Thinking {
                            content: reasoning,
                            expanded: self.thinking_expanded,
                            elapsed: Elapsed::default(),
                        });
                    }
                    if !content.is_empty() {
                        self.blocks.push(ChatBlock::Message {
                            label: "Assistant".into(),
                            content,
                            model,
                            kind: MessageKind::Assistant,
                            expanded: true,
                        });
                    }
                    for call in tool_calls {
                        let mut counter = ToolCounter::default();
                        counter.push(&call.arguments.to_string());
                        self.blocks.push(ChatBlock::Tool {
                            call_id: Some(call.id.clone()),
                            name: call.name,
                            arguments: serde_json::to_string_pretty(&call.arguments)
                                .unwrap_or_default(),
                            output: None,
                            status: ToolStatus::Pending,
                            expanded: self.tools_expanded,
                            counter,
                            elapsed: Elapsed::default(),
                        });
                        self.tool_calls.insert(call.id, self.blocks.len() - 1);
                    }
                }
                Message::Tool {
                    call_id, content, ..
                } => {
                    if let Some(block) = self.tool_calls.get(&call_id).copied()
                        && let ChatBlock::Tool {
                            output,
                            status,
                            counter,
                            ..
                        } = &mut self.blocks[block]
                    {
                        *status = if content.starts_with("Error:") {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Done
                        };
                        counter.push(&content);
                        *output = Some(content);
                    }
                }
            }
        }
        self.scroll = 0;
        self.error = None;
        self.notice = None;
        self.assistant = None;
        self.reasoning = None;
        self.tool_drafts.clear();
        self.selected = None;
    }
}

fn push_input_text(lines: &mut Vec<ratatui::text::Line<'static>>, text: &str) {
    use ratatui::text::{Line, Span};

    let mut parts = text.split('\n').peekable();
    while let Some(part) = parts.next() {
        if !part.is_empty() {
            lines
                .last_mut()
                .unwrap()
                .spans
                .push(Span::raw(part.to_owned()));
        }
        if parts.peek().is_some() {
            lines.push(Line::from(Span::raw(" ")));
        }
    }
}

fn advance_cursor(text: &str, width: u16, row: &mut u16, column: &mut u16) {
    for character in text.chars() {
        if character == '\n' {
            *row += 1;
            *column = 1;
        } else {
            *column += 1;
            if *column >= width {
                *row += 1;
                *column = 0;
            }
        }
    }
}

fn collapsible(block: &ChatBlock) -> bool {
    matches!(
        block,
        ChatBlock::Message {
            kind: MessageKind::User | MessageKind::Assistant,
            ..
        } | ChatBlock::Thinking { .. }
            | ChatBlock::Tool { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use serde_json::json;

    #[test]
    fn thinking_is_collapsed_and_keyboard_toggleable() {
        let mut state = UiState::new();
        state.apply(Event::ResponseStarted);
        state.apply(Event::ReasoningDelta("working it out".into()));

        assert!(matches!(
            state.blocks[0],
            ChatBlock::Thinking {
                expanded: false,
                ..
            }
        ));
        state.focus_next();
        assert_eq!(state.selected(), Some(0));
        state.toggle_selected();
        assert!(matches!(
            state.blocks[0],
            ChatBlock::Thinking { expanded: true, .. }
        ));
    }

    #[test]
    fn generation_is_connecting_until_the_response_starts() {
        let mut state = UiState::new();
        state.apply(Event::GenerationStarted);
        assert!(state.generating);
        assert!(state.connecting);

        state.apply(Event::ModelRequestStarted("test-model".into()));
        assert!(state.connecting);

        state.apply(Event::ResponseStarted);
        assert!(state.generating);
        assert!(!state.connecting);

        state.apply(Event::GenerationFinished);
        assert!(!state.generating);
        assert!(!state.connecting);
    }

    #[test]
    fn retry_status_is_visible_until_response_arrives() {
        let mut state = UiState::new();
        state.apply(Event::GenerationStarted);
        state.apply(Event::Retrying { seconds: 5 });
        assert_eq!(
            state.notice.as_deref(),
            Some("retrying in 5s · Esc to cancel")
        );
        assert!(state.connecting);

        state.apply(Event::ResponseStarted);
        assert!(state.notice.is_none());
        assert!(!state.connecting);
    }

    #[test]
    fn context_compaction_marker_precedes_pending_user_message() {
        let mut state = UiState::new();
        state.push_user("continue".into());
        state.apply(Event::ContextChanged {
            tokens: 750,
            max_tokens: 1000,
        });
        state.apply(Event::ContextCompacted);

        assert_eq!(state.context_tokens, 750);
        assert!(matches!(
            &state.blocks[0],
            ChatBlock::Message {
                kind: MessageKind::System,
                content,
                ..
            } if content == "Context compacted"
        ));
        assert!(matches!(
            state.blocks[1],
            ChatBlock::Message {
                kind: MessageKind::User,
                ..
            }
        ));
    }

    #[test]
    fn user_and_assistant_messages_are_collapsible() {
        let mut state = UiState::new();
        state.push_user("hello".into());
        state.apply(Event::TextDelta("hi".into()));

        state.focus_next();
        assert_eq!(state.selected(), Some(1));
        state.toggle_selected();
        assert!(matches!(
            state.blocks[1],
            ChatBlock::Message {
                expanded: false,
                ..
            }
        ));

        state.select_previous();
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn usage_updates_the_session_total() {
        let mut state = UiState::new();
        state.apply(Event::UsageChanged(12_345));
        assert_eq!(state.total_tokens, 12_345);
    }

    #[test]
    fn tool_call_and_result_share_one_block() {
        let mut state = UiState::new();
        let arguments = r#"{"path":"src/main.rs"}"#;
        state.apply(Event::ToolCallDelta {
            index: 0,
            name: Some("read".into()),
            arguments: arguments.into(),
        });
        state.apply(Event::ToolCallFinished {
            index: 0,
            call: ToolCall {
                id: "call_1".into(),
                name: "read".into(),
                arguments: json!({ "path": "src/main.rs" }),
            },
        });
        assert!(matches!(
            &state.blocks[0],
            ChatBlock::Tool { counter, .. }
                if counter.label() == format!("{} chars", arguments.chars().count())
        ));
        state.apply(Event::ToolStarted {
            call_id: "call_1".into(),
        });
        state.apply(Event::ToolResult {
            call_id: "call_1".into(),
            output: "contents".into(),
            success: true,
        });

        assert_eq!(state.blocks.len(), 1);
        assert!(matches!(
            &state.blocks[0],
            ChatBlock::Tool {
                output: Some(output),
                status: ToolStatus::Done,
                expanded: false,
                ..
            } if output == "contents"
        ));
    }

    #[test]
    fn tool_counter_switches_permanently_from_chars_to_lines() {
        let mut counter = ToolCounter::default();
        counter.push("abcé");
        assert_eq!(counter.label(), "4 chars");

        counter.push("\nnext");
        assert_eq!(counter.label(), "2 lines");

        counter.push(" text\n");
        assert_eq!(counter.label(), "3 lines");
    }

    #[test]
    fn tool_counter_handles_streamed_newline_escapes() {
        let mut counter = ToolCounter::default();
        counter.push(r"first\");
        assert_eq!(counter.label(), "6 chars");

        counter.push("nsecond");
        assert_eq!(counter.label(), "2 lines");

        counter.push(r"\\nthird\nfourth");
        assert_eq!(counter.label(), "3 lines");
    }

    #[test]
    fn visibility_commands_change_new_block_defaults() {
        let mut state = UiState::new();
        state.toggle_thinking_default();
        state.toggle_tools_default();
        state.apply(Event::ReasoningDelta("visible".into()));
        state.apply(Event::ToolCallDelta {
            index: 0,
            name: Some("read".into()),
            arguments: "{}".into(),
        });

        assert!(matches!(
            state.blocks[0],
            ChatBlock::Thinking { expanded: true, .. }
        ));
        assert!(matches!(
            state.blocks[1],
            ChatBlock::Tool { expanded: true, .. }
        ));
    }

    #[test]
    fn large_paste_moves_and_deletes_as_one_input_item() {
        let mut state = UiState::new();
        state.insert_char('a');
        state.insert_paste("long paste", 4);
        state.insert_char('z');

        assert_eq!(state.input, "along pastez");
        assert!(state.input_lines()[0].spans.iter().any(|span| {
            span.content == " Pasted 10 chars " && span.style.bg == Some(Color::Magenta)
        }));

        state.move_input_left();
        state.move_input_left();
        state.delete();
        assert_eq!(state.input, "az");
    }

    #[test]
    fn short_multiline_paste_remains_visible_text() {
        let mut state = UiState::new();
        state.insert_paste("one\ntwo", 200);
        let lines = state.input_lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].spans[1].content, "one");
        assert_eq!(lines[1].spans[1].content, "two");
    }
}
