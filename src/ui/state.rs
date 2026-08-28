use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

use crate::{
    project::ProjectState,
    runtime::{CANCELLED_BY_USER, Event, ImageContent, Message, ToolCall, UserPrompt},
    tool::ExecutionPlan,
};

const IMAGE_SENTINEL: char = '\u{fffc}';

#[derive(Clone, Copy)]
pub enum MessageKind {
    User,
    Assistant,
    System,
    Error,
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

    fn resume(&mut self) {
        if self.started.is_none() {
            self.started = Some(Instant::now());
            self.duration.get_or_insert(Duration::ZERO);
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
        images: Vec<ImageContent>,
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
        diff: Option<String>,
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
    pub waiting: bool,
    pub tool_running: bool,
    pub error: Option<String>,
    pub notice: Option<String>,
    pub scroll: u16,
    pub chat_scroll_max: u16,
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
    pub git_panel_width: Option<u16>,
    pub git_split_dragging: bool,
    pub git_diff_mode: bool,
    pub git_fullscreen_diff: bool,
    pub fullscreen_tool_diff: Option<String>,
    pub git_status_scroll: u16,
    pub git_panel_diff_scroll: u16,
    pub git_diff_scroll: u16,
    pub plan: Option<ExecutionPlan>,
    pub plan_panel: bool,
    pub plan_panel_height: Option<u16>,
    pub plan_split_dragging: bool,
    pub plan_scroll: u16,
    pub search: Option<ChatSearch>,
    pub model_picker: Option<ModelPicker>,
    pub recent_models: Vec<String>,
    pub image_cell_size: Option<(u16, u16)>,
    generation_started: Option<Instant>,
    generated_bytes: usize,
    reported_generation_duration: Duration,
    reported_output_tokens: u64,
    average_generation_speed: Option<f64>,
    assistant: Option<usize>,
    reasoning: Option<usize>,
    response_model: String,
    tool_drafts: BTreeMap<usize, usize>,
    tool_calls: BTreeMap<String, usize>,
    selected: Option<usize>,
    input_cursor: usize,
    pasted: Vec<PastedRange>,
    input_images: Vec<InputImage>,
    next_input_image_id: u64,
    pub text_selection: Option<TextSelection>,
    pub selection_anchor: Option<TextPoint>,
    toast: Option<Toast>,
}

pub struct ChatSearch {
    pub query: String,
    pub cursor: usize,
    pub current: usize,
    pub total: usize,
}

#[derive(Default)]
pub struct ModelPicker {
    pub query: String,
    pub cursor: usize,
    pub selected: usize,
}

impl ChatSearch {
    pub fn insert_char(&mut self, character: char) {
        self.query.insert(self.cursor, character);
        self.cursor += character.len_utf8();
    }

    pub fn backspace(&mut self) {
        let Some((start, _)) = self.query[..self.cursor].char_indices().next_back() else {
            return;
        };
        self.query.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn move_left(&mut self) {
        if let Some((start, _)) = self.query[..self.cursor].char_indices().next_back() {
            self.cursor = start;
        }
    }

    pub fn move_right(&mut self) {
        if let Some(character) = self.query[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    pub fn move_word_left(&mut self) {
        self.cursor = previous_word_start(&self.query, self.cursor);
    }

    pub fn move_word_right(&mut self) {
        self.cursor = next_word_start(&self.query, self.cursor);
    }

    pub fn delete_word_back(&mut self) {
        let start = previous_word_start(&self.query, self.cursor);
        self.query.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextPoint {
    pub row: u16,
    pub column: u16,
}

#[derive(Clone, Copy, Debug)]
pub struct TextSelection {
    pub start: TextPoint,
    pub end: TextPoint,
}

struct Toast {
    message: String,
    expires: Instant,
}

#[derive(Clone)]
struct PastedRange {
    start: usize,
    end: usize,
    chars: usize,
}

#[derive(Clone)]
struct InputImage {
    id: u64,
    start: usize,
    end: usize,
    label: String,
    started: Instant,
    image: Option<ImageContent>,
}

impl UiState {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            blocks: Vec::new(),
            generating: false,
            connecting: false,
            waiting: false,
            tool_running: false,
            error: None,
            notice: None,
            scroll: 0,
            chat_scroll_max: 0,
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
            git_panel_width: None,
            git_split_dragging: false,
            git_diff_mode: false,
            git_fullscreen_diff: false,
            fullscreen_tool_diff: None,
            git_status_scroll: 0,
            git_panel_diff_scroll: 0,
            git_diff_scroll: 0,
            plan: None,
            plan_panel: false,
            plan_panel_height: None,
            plan_split_dragging: false,
            plan_scroll: 0,
            search: None,
            model_picker: None,
            recent_models: Vec::new(),
            image_cell_size: None,
            generation_started: None,
            generated_bytes: 0,
            reported_generation_duration: Duration::ZERO,
            reported_output_tokens: 0,
            average_generation_speed: None,
            assistant: None,
            reasoning: None,
            response_model: String::new(),
            tool_drafts: BTreeMap::new(),
            tool_calls: BTreeMap::new(),
            selected: None,
            input_cursor: 0,
            pasted: Vec::new(),
            input_images: Vec::new(),
            next_input_image_id: 0,
            text_selection: None,
            selection_anchor: None,
            toast: None,
        }
    }

    pub fn take_input(&mut self) -> Option<UserPrompt> {
        let content = self.input.replace(IMAGE_SENTINEL, "").trim().to_owned();
        if (content.is_empty() && self.input_images.is_empty())
            || self.generating
            || self.image_loading()
        {
            return None;
        }
        let images = std::mem::take(&mut self.input_images)
            .into_iter()
            .map(|item| item.image.unwrap())
            .collect();
        self.input.clear();
        self.input_cursor = 0;
        self.pasted.clear();
        self.error = None;
        self.notice = None;
        self.scroll = 0;
        Some(UserPrompt { content, images })
    }

    pub fn set_input(&mut self, input: String) {
        self.input = input;
        self.input_cursor = self.input.len();
        self.pasted.clear();
        self.input_images.clear();
        self.palette_selected = 0;
    }

    pub fn clear_input(&mut self) {
        self.set_input(String::new());
    }

    pub fn insert_char(&mut self, character: char) {
        self.shift_input_items(self.input_cursor, character.len_utf8() as isize);
        self.input.insert(self.input_cursor, character);
        self.input_cursor += character.len_utf8();
    }

    pub fn insert_paste(&mut self, text: &str, collapse_at: usize) {
        let chars = text.chars().count();
        let start = self.input_cursor;
        self.shift_input_items(start, text.len() as isize);
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

    #[cfg(test)]
    pub fn insert_image(&mut self, image: ImageContent) {
        let id = self.begin_image_load("Processing image");
        self.finish_image_load(id, image);
    }

    pub fn begin_image_load(&mut self, label: impl Into<String>) -> u64 {
        let id = self.next_input_image_id;
        self.next_input_image_id += 1;
        let start = self.input_cursor;
        let width = IMAGE_SENTINEL.len_utf8();
        self.shift_input_items(start, width as isize);
        self.input.insert(start, IMAGE_SENTINEL);
        self.input_cursor += width;
        self.input_images.push(InputImage {
            id,
            start,
            end: self.input_cursor,
            label: label.into(),
            started: Instant::now(),
            image: None,
        });
        self.input_images.sort_by_key(|item| item.start);
        id
    }

    pub fn finish_image_load(&mut self, id: u64, image: ImageContent) -> bool {
        let Some(item) = self.input_images.iter_mut().find(|item| item.id == id) else {
            return false;
        };
        item.image = Some(image);
        true
    }

    pub fn fail_image_load(&mut self, id: u64) -> bool {
        let Some(index) = self
            .input_images
            .iter()
            .position(|item| item.id == id && item.image.is_none())
        else {
            return false;
        };
        self.remove_input_image(index);
        true
    }

    pub fn replace_image_load_with_paste(
        &mut self,
        id: u64,
        text: &str,
        collapse_at: usize,
    ) -> bool {
        let Some(index) = self
            .input_images
            .iter()
            .position(|item| item.id == id && item.image.is_none())
        else {
            return false;
        };
        let cursor_after = self.input_cursor > self.input_images[index].start;
        let start = self.remove_input_image(index);
        self.shift_input_items(start, text.len() as isize);
        self.input.insert_str(start, text);
        if cursor_after {
            self.input_cursor += text.len();
        }
        let chars = text.chars().count();
        if chars > collapse_at {
            self.pasted.push(PastedRange {
                start,
                end: start + text.len(),
                chars,
            });
            self.pasted.sort_by_key(|range| range.start);
        }
        true
    }

    pub fn has_input_images(&self) -> bool {
        !self.input_images.is_empty()
    }

    pub fn image_loading(&self) -> bool {
        self.input_images.iter().any(|item| item.image.is_none())
    }

    pub fn backspace(&mut self) {
        if let Some(index) = self
            .input_images
            .iter()
            .position(|item| item.end == self.input_cursor)
        {
            self.remove_input_image(index);
            return;
        }
        if let Some(index) = self
            .pasted
            .iter()
            .position(|range| range.end == self.input_cursor)
        {
            let range = self.pasted.remove(index);
            self.input.replace_range(range.start..range.end, "");
            self.input_cursor = range.start;
            self.shift_input_items(range.end, -((range.end - range.start) as isize));
            return;
        }
        let Some((start, _)) = self.input[..self.input_cursor].char_indices().next_back() else {
            return;
        };
        self.input.replace_range(start..self.input_cursor, "");
        let removed = self.input_cursor - start;
        self.input_cursor = start;
        self.shift_input_items(start + removed, -(removed as isize));
    }

    pub fn delete(&mut self) {
        if let Some(index) = self
            .input_images
            .iter()
            .position(|item| item.start == self.input_cursor)
        {
            self.remove_input_image(index);
            return;
        }
        if let Some(index) = self
            .pasted
            .iter()
            .position(|range| range.start == self.input_cursor)
        {
            let range = self.pasted.remove(index);
            self.input.replace_range(range.start..range.end, "");
            self.shift_input_items(range.end, -((range.end - range.start) as isize));
            return;
        }
        let Some(character) = self.input[self.input_cursor..].chars().next() else {
            return;
        };
        let end = self.input_cursor + character.len_utf8();
        self.input.replace_range(self.input_cursor..end, "");
        self.shift_input_items(end, -(character.len_utf8() as isize));
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

    pub fn move_input_word_left(&mut self) {
        if let Some((start, _)) = self
            .input_items()
            .into_iter()
            .find(|(_, end)| *end == self.input_cursor)
        {
            self.input_cursor = start;
            return;
        }
        let target = previous_word_start(&self.input, self.input_cursor);
        self.input_cursor = self
            .input_items()
            .into_iter()
            .filter(|(start, end)| *start < self.input_cursor && *end > target)
            .map(|(_, end)| end)
            .max()
            .unwrap_or(target);
    }

    pub fn move_input_word_right(&mut self) {
        if let Some((_, end)) = self
            .input_items()
            .into_iter()
            .find(|(start, _)| *start == self.input_cursor)
        {
            self.input_cursor = end;
            return;
        }
        let target = next_word_start(&self.input, self.input_cursor);
        self.input_cursor = self
            .input_items()
            .into_iter()
            .filter(|(start, end)| *start < target && *end > self.input_cursor)
            .map(|(start, _)| start)
            .min()
            .unwrap_or(target);
    }

    pub fn delete_input_word_back(&mut self) {
        let end = self.input_cursor;
        self.move_input_word_left();
        let start = self.input_cursor;
        if start == end {
            return;
        }
        self.pasted
            .retain(|range| range.start < start || range.end > end);
        self.input_images
            .retain(|item| item.start < start || item.end > end);
        self.input.replace_range(start..end, "");
        self.shift_input_items(end, -((end - start) as isize));
    }

    pub fn move_input_home(&mut self) {
        self.input_cursor = 0;
    }

    pub fn move_input_end(&mut self) {
        self.input_cursor = self.input.len();
    }

    pub fn input_lines(&self) -> Vec<ratatui::text::Line<'static>> {
        use ratatui::{
            style::{Color, Style},
            text::{Line, Span},
        };

        let mut lines = vec![Line::from(Span::raw(" "))];
        let mut offset = 0;
        for (start, end, label, color, gap) in self.input_plates() {
            push_input_text(&mut lines, &self.input[offset..start]);
            lines.last_mut().unwrap().spans.push(Span::styled(
                label,
                Style::default().fg(Color::Black).bg(color),
            ));
            if gap && end < self.input.len() {
                lines.last_mut().unwrap().spans.push(Span::raw(" "));
            }
            offset = end;
        }
        push_input_text(&mut lines, &self.input[offset..]);
        lines
    }

    pub fn input_cursor(&self, width: u16) -> (u16, u16) {
        let mut row = 0u16;
        let mut column = 1u16;
        let mut offset = 0;
        for (start, end, label, _, gap) in self.input_plates() {
            if start >= self.input_cursor {
                break;
            }
            advance_cursor(&self.input[offset..start], width, &mut row, &mut column);
            advance_cursor(&label, width, &mut row, &mut column);
            if gap && end < self.input.len() {
                advance_cursor(" ", width, &mut row, &mut column);
            }
            offset = end;
        }
        advance_cursor(
            &self.input[offset..self.input_cursor],
            width,
            &mut row,
            &mut column,
        );
        (row, column)
    }

    fn input_plates(&self) -> Vec<(usize, usize, String, ratatui::style::Color, bool)> {
        use ratatui::style::Color;

        let mut plates = self
            .pasted
            .iter()
            .map(|range| {
                (
                    range.start,
                    range.end,
                    format!(" Pasted {} chars ", range.chars),
                    Color::Magenta,
                    false,
                )
            })
            .chain(self.input_images.iter().map(|item| {
                let (label, color) = if let Some(image) = &item.image {
                    let dimensions = if image.width == 0 || image.height == 0 {
                        String::new()
                    } else {
                        format!(" · {}×{}", image.width, image.height)
                    };
                    (format!(" Image{dimensions} "), Color::Cyan)
                } else {
                    (
                        format!(
                            " {} · {:.1}s ",
                            item.label,
                            item.started.elapsed().as_secs_f32()
                        ),
                        Color::Yellow,
                    )
                };
                (item.start, item.end, label, color, true)
            }))
            .collect::<Vec<_>>();
        plates.sort_by_key(|plate| plate.0);
        plates
    }

    fn input_items(&self) -> Vec<(usize, usize)> {
        self.pasted
            .iter()
            .map(|range| (range.start, range.end))
            .chain(self.input_images.iter().map(|item| (item.start, item.end)))
            .collect()
    }

    fn remove_input_image(&mut self, index: usize) -> usize {
        let item = self.input_images.remove(index);
        self.input.replace_range(item.start..item.end, "");
        if self.input_cursor >= item.end {
            self.input_cursor -= item.end - item.start;
        } else if self.input_cursor > item.start {
            self.input_cursor = item.start;
        }
        self.shift_input_items(item.end, -((item.end - item.start) as isize));
        item.start
    }

    fn shift_input_items(&mut self, from: usize, delta: isize) {
        for range in self.pasted.iter_mut().filter(|range| range.start >= from) {
            range.start = range.start.checked_add_signed(delta).unwrap();
            range.end = range.end.checked_add_signed(delta).unwrap();
        }
        for item in self
            .input_images
            .iter_mut()
            .filter(|item| item.start >= from)
        {
            item.start = item.start.checked_add_signed(delta).unwrap();
            item.end = item.end.checked_add_signed(delta).unwrap();
        }
    }

    #[cfg(test)]
    pub fn push_user(&mut self, content: String) {
        self.push_user_with_images(content, Vec::new());
    }

    pub fn push_user_with_images(&mut self, content: String, images: Vec<ImageContent>) {
        self.blocks.push(ChatBlock::Message {
            label: "You".into(),
            content,
            images,
            model: String::new(),
            kind: MessageKind::User,
            expanded: true,
        });
    }

    pub fn set_error(&mut self, error: impl Into<String>) {
        let error = error.into();
        self.error = Some(error.clone());
        self.blocks.push(ChatBlock::Message {
            label: "Error".into(),
            content: error,
            images: Vec::new(),
            model: String::new(),
            kind: MessageKind::Error,
            expanded: true,
        });
    }

    pub fn toggle_thinking_default(&mut self) {
        self.thinking_expanded = !self.thinking_expanded;
        for block in &mut self.blocks {
            if let ChatBlock::Thinking { expanded, .. } = block {
                *expanded = self.thinking_expanded;
            }
        }
    }

    pub fn toggle_tools_default(&mut self) {
        self.tools_expanded = !self.tools_expanded;
        for block in &mut self.blocks {
            if let ChatBlock::Tool { expanded, .. } = block {
                *expanded = self.tools_expanded;
            }
        }
    }

    pub fn toggle_plan_panel(&mut self) {
        self.plan_panel = !self.plan_panel;
        self.plan_split_dragging = false;
    }

    pub fn open_fullscreen_git_diff(&mut self) {
        self.git_fullscreen_diff = true;
        self.git_diff_scroll = 0;
        self.fullscreen_tool_diff = None;
        self.project.git_diff_path = None;
        self.project.git_diff.clear();
    }

    pub fn open_fullscreen_tool_diff(&mut self, diff: String) {
        self.git_fullscreen_diff = true;
        self.git_diff_scroll = 0;
        self.fullscreen_tool_diff = Some(diff);
    }

    pub fn close_fullscreen_git_diff(&mut self) {
        self.git_fullscreen_diff = false;
        self.git_diff_scroll = 0;
        self.fullscreen_tool_diff = None;
    }

    pub fn fullscreen_diff(&self) -> &str {
        self.fullscreen_tool_diff
            .as_deref()
            .unwrap_or(&self.project.git_diff)
    }

    pub fn show_toast(&mut self, message: impl Into<String>) {
        self.toast = Some(Toast {
            message: message.into(),
            expires: Instant::now() + Duration::from_secs(3),
        });
    }

    pub fn expire_toast(&mut self) {
        if self
            .toast
            .as_ref()
            .is_some_and(|toast| Instant::now() >= toast.expires)
        {
            self.toast = None;
        }
    }

    pub fn toast(&self) -> Option<&str> {
        self.toast.as_ref().map(|toast| toast.message.as_str())
    }

    pub fn conversation_focused(&self) -> bool {
        self.selected.is_some()
    }
    pub fn selected(&self) -> Option<usize> {
        self.selected
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

    pub fn start_search(&mut self) {
        self.focus_input();
        self.search = Some(ChatSearch {
            query: String::new(),
            cursor: 0,
            current: 0,
            total: 0,
        });
    }

    pub fn has_chat_images(&self) -> bool {
        self.blocks.iter().any(|block| {
            matches!(
                block,
                ChatBlock::Message { images, .. } if !images.is_empty()
            )
        })
    }

    #[cfg(test)]
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

    pub fn collapse(&mut self, index: usize) {
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
            *expanded = false;
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
            Event::PlanChanged(plan) => {
                if plan.is_some() {
                    self.plan_panel = true;
                }
                self.plan = plan;
                self.plan_scroll = 0;
            }
            Event::GenerationStarted => {
                self.generating = true;
                self.connecting = true;
                self.waiting = false;
                self.tool_running = false;
                self.error = None;
                self.generation_started = None;
                self.generated_bytes = 0;
                self.reported_generation_duration = Duration::ZERO;
                self.reported_output_tokens = 0;
                self.average_generation_speed = None;
            }
            Event::ModelRequestStarted(model) => {
                self.finish_reasoning();
                self.connecting = true;
                self.waiting = false;
                self.tool_running = false;
                self.response_model = model;
                self.tool_drafts.clear();
                self.assistant = None;
                self.reasoning = None;
                self.generation_started = None;
                self.generated_bytes = 0;
            }
            Event::ResponseHeadersReceived => {
                self.connecting = false;
                self.waiting = true;
                if self
                    .notice
                    .as_ref()
                    .is_some_and(|notice| notice.starts_with("retrying in "))
                {
                    self.notice = None;
                }
            }
            Event::ResponseStarted => {
                self.connecting = false;
                self.waiting = false;
                self.generation_started = Some(Instant::now());
                if self
                    .notice
                    .as_ref()
                    .is_some_and(|notice| notice.starts_with("retrying in "))
                {
                    self.notice = None;
                }
            }
            Event::ModelResponseFinished {
                output_tokens,
                duration,
            } => {
                self.reported_output_tokens += output_tokens;
                self.reported_generation_duration += duration;
                self.generation_started = None;
                self.generated_bytes = 0;
            }
            Event::ReasoningDelta(delta) => {
                self.generated_bytes += delta.len();
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
                self.generated_bytes += delta.len();
                self.finish_reasoning();
                let block = match self.assistant {
                    Some(block) => block,
                    None => {
                        self.blocks.push(ChatBlock::Message {
                            label: "Assistant".into(),
                            content: String::new(),
                            images: Vec::new(),
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
                self.generated_bytes += arguments.len();
                self.finish_reasoning();
                let block = match self.tool_drafts.get(&index).copied() {
                    Some(block) => block,
                    None => {
                        self.blocks.push(ChatBlock::Tool {
                            call_id: None,
                            name: String::new(),
                            arguments: String::new(),
                            output: None,
                            diff: None,
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
            Event::ToolStarted { call_id } => {
                self.tool_running = true;
                self.set_tool_status(&call_id, ToolStatus::Running);
            }
            Event::ToolResult {
                call_id,
                output,
                success,
                diff,
            } => {
                self.tool_running = false;
                if let Some(block) = self.tool_calls.get(&call_id).copied()
                    && let ChatBlock::Tool {
                        output: block_output,
                        diff: block_diff,
                        status,
                        counter,
                        elapsed,
                        ..
                    } = &mut self.blocks[block]
                {
                    counter.push(&output);
                    elapsed.finish();
                    *block_output = Some(output);
                    *block_diff = diff;
                    *status = if success {
                        ToolStatus::Done
                    } else {
                        ToolStatus::Failed
                    };
                }
            }
            Event::Retrying { seconds } => {
                self.connecting = true;
                self.waiting = false;
                self.notice = Some(format!("retrying in {seconds}s · Esc to cancel"));
            }
            Event::CompactionStarted => {
                self.notice = Some("compacting context".into());
            }
            Event::ContextCompacted => {
                let block = ChatBlock::Message {
                    label: "System".into(),
                    content: "Context compacted".into(),
                    images: Vec::new(),
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
                if !self.reported_generation_duration.is_zero() {
                    self.average_generation_speed = Some(
                        self.reported_output_tokens as f64
                            / self.reported_generation_duration.as_secs_f64(),
                    );
                }
                self.generating = false;
                self.connecting = false;
                self.waiting = false;
                self.tool_running = false;
                self.approval = None;
                self.generation_started = None;
                self.generated_bytes = 0;
                self.reported_generation_duration = Duration::ZERO;
                self.reported_output_tokens = 0;
            }
            Event::GenerationCancelled => {
                self.finish_reasoning();
                for block in &mut self.blocks {
                    if let ChatBlock::Tool {
                        output,
                        status,
                        counter,
                        elapsed,
                        ..
                    } = block
                        && matches!(
                            status,
                            ToolStatus::Streaming
                                | ToolStatus::Pending
                                | ToolStatus::WaitingApproval
                                | ToolStatus::Running
                        )
                    {
                        *status = ToolStatus::Failed;
                        counter.push(CANCELLED_BY_USER);
                        *output = Some(CANCELLED_BY_USER.into());
                        elapsed.finish();
                    }
                }
                self.blocks.push(ChatBlock::Message {
                    label: "System".into(),
                    content: CANCELLED_BY_USER.into(),
                    images: Vec::new(),
                    model: String::new(),
                    kind: MessageKind::System,
                    expanded: true,
                });
                self.generating = false;
                self.connecting = false;
                self.waiting = false;
                self.tool_running = false;
                self.approval = None;
                self.notice = None;
                self.assistant = None;
                self.reasoning = None;
                self.tool_drafts.clear();
                self.generation_started = None;
                self.generated_bytes = 0;
                self.reported_generation_duration = Duration::ZERO;
                self.reported_output_tokens = 0;
            }
            Event::Saved => self.notice = Some("session saved".into()),
            Event::Error(error) => {
                self.finish_reasoning();
                self.generating = false;
                self.connecting = false;
                self.waiting = false;
                self.tool_running = false;
                self.approval = None;
                self.generation_started = None;
                self.generated_bytes = 0;
                self.reported_generation_duration = Duration::ZERO;
                self.reported_output_tokens = 0;
                self.set_error(error);
            }
        }
    }

    pub fn generation_speed(&self) -> Option<f64> {
        let elapsed = self.generation_started?.elapsed().as_secs_f64();
        let tokens = self.generated_bytes.div_ceil(4);
        Some(tokens as f64 / elapsed)
    }

    pub fn average_generation_speed(&self) -> Option<f64> {
        self.average_generation_speed
    }

    fn set_tool_status(&mut self, call_id: &str, value: ToolStatus) {
        if let Some(block) = self.tool_calls.get(call_id).copied()
            && let ChatBlock::Tool {
                status, elapsed, ..
            } = &mut self.blocks[block]
        {
            *status = value;
            match value {
                ToolStatus::WaitingApproval => elapsed.finish(),
                ToolStatus::Running => elapsed.resume(),
                _ => {}
            }
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
                    images: Vec::new(),
                    model: String::new(),
                    kind: MessageKind::System,
                    expanded: true,
                }),
                Message::User { content, images } => self.blocks.push(ChatBlock::Message {
                    label: "You".into(),
                    content,
                    images,
                    model: String::new(),
                    kind: MessageKind::User,
                    expanded: true,
                }),
                Message::Assistant {
                    content,
                    model,
                    reasoning,
                    tool_calls,
                    response_items: _,
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
                            images: Vec::new(),
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
                            diff: None,
                            status: ToolStatus::Pending,
                            expanded: self.tools_expanded,
                            counter,
                            elapsed: Elapsed::default(),
                        });
                        self.tool_calls.insert(call.id, self.blocks.len() - 1);
                    }
                }
                Message::Tool {
                    call_id,
                    content,
                    diff,
                    ..
                } => {
                    if let Some(block) = self.tool_calls.get(&call_id).copied()
                        && let ChatBlock::Tool {
                            output,
                            diff: block_diff,
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
                        *block_diff = diff;
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

fn previous_word_start(text: &str, cursor: usize) -> usize {
    let mut start = cursor;
    let mut characters = text[..cursor].char_indices().rev().peekable();
    while let Some(&(index, character)) = characters.peek() {
        if character.is_alphanumeric() {
            break;
        }
        start = index;
        characters.next();
    }
    while let Some(&(index, character)) = characters.peek() {
        if !character.is_alphanumeric() {
            break;
        }
        start = index;
        characters.next();
    }
    start
}

fn next_word_start(text: &str, cursor: usize) -> usize {
    let mut end = cursor;
    let mut characters = text[cursor..].char_indices().peekable();
    while let Some(&(index, character)) = characters.peek() {
        if !character.is_alphanumeric() {
            break;
        }
        end = cursor + index + character.len_utf8();
        characters.next();
    }
    while let Some(&(index, character)) = characters.peek() {
        if character.is_alphanumeric() {
            break;
        }
        end = cursor + index + character.len_utf8();
        characters.next();
    }
    end
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
        state.select(0);
        assert_eq!(state.selected(), Some(0));
        state.toggle_selected();
        assert!(matches!(
            state.blocks[0],
            ChatBlock::Thinking { expanded: true, .. }
        ));
    }

    #[test]
    fn generation_moves_from_connecting_to_waiting_to_streaming() {
        let mut state = UiState::new();
        state.apply(Event::GenerationStarted);
        assert!(state.generating);
        assert!(state.connecting);
        assert!(!state.waiting);

        state.apply(Event::ModelRequestStarted("test-model".into()));
        assert!(state.connecting);

        state.apply(Event::ResponseHeadersReceived);
        assert!(!state.connecting);
        assert!(state.waiting);

        state.apply(Event::ResponseStarted);
        assert!(state.generating);
        assert!(!state.connecting);
        assert!(!state.waiting);

        state.apply(Event::GenerationFinished);
        assert!(!state.generating);
        assert!(!state.connecting);
        assert!(!state.waiting);
    }

    #[test]
    fn cancellation_preserves_partial_blocks_and_fails_active_tools() {
        let mut state = UiState::new();
        state.apply(Event::GenerationStarted);
        state.apply(Event::ResponseStarted);
        state.apply(Event::ReasoningDelta("partial thought".into()));
        state.apply(Event::ToolCallDelta {
            index: 0,
            name: Some("read".into()),
            arguments: r#"{"path":"src/main.rs"}"#.into(),
        });

        state.apply(Event::GenerationCancelled);

        assert!(!state.generating);
        assert!(matches!(
            &state.blocks[0],
            ChatBlock::Thinking { content, .. } if content == "partial thought"
        ));
        assert!(matches!(
            &state.blocks[1],
            ChatBlock::Tool {
                status: ToolStatus::Failed,
                output: Some(output),
                ..
            } if output == CANCELLED_BY_USER
        ));
        assert!(matches!(
            state.blocks.last(),
            Some(ChatBlock::Message {
                content,
                kind: MessageKind::System,
                ..
            }) if content == CANCELLED_BY_USER
        ));

        state.apply(Event::GenerationStarted);
        state.apply(Event::ModelRequestStarted("test-model".into()));
        state.apply(Event::ResponseStarted);
        state.apply(Event::TextDelta("continued normally".into()));
        assert!(matches!(
            state.blocks.last(),
            Some(ChatBlock::Message {
                content,
                kind: MessageKind::Assistant,
                ..
            }) if content == "continued normally"
        ));
    }

    #[test]
    fn retry_status_is_visible_until_response_headers_arrive() {
        let mut state = UiState::new();
        state.apply(Event::GenerationStarted);
        state.apply(Event::Retrying { seconds: 5 });
        assert_eq!(
            state.notice.as_deref(),
            Some("retrying in 5s · Esc to cancel")
        );
        assert!(state.connecting);

        state.apply(Event::ResponseHeadersReceived);
        assert!(state.notice.is_none());
        assert!(!state.connecting);
        assert!(state.waiting);
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

        state.select(1);
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
    fn errors_append_without_erasing_the_transcript() {
        let mut state = UiState::new();
        state.push_user("keep this".into());
        state.apply(Event::TextDelta("partial response".into()));
        state.apply(Event::Error("model turn limit reached".into()));

        assert_eq!(state.blocks.len(), 3);
        assert!(matches!(
            &state.blocks[0],
            ChatBlock::Message {
                content,
                kind: MessageKind::User,
                ..
            } if content == "keep this"
        ));
        assert!(matches!(
            state.blocks.last(),
            Some(ChatBlock::Message {
                content,
                kind: MessageKind::Error,
                ..
            }) if content == "model turn limit reached"
        ));
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
        state.apply(Event::ApprovalRequested(ToolCall {
            id: "call_1".into(),
            name: "read".into(),
            arguments: json!({ "path": "src/main.rs" }),
        }));
        assert!(matches!(
            &state.blocks[0],
            ChatBlock::Tool {
                status: ToolStatus::WaitingApproval,
                elapsed,
                ..
            } if elapsed.started.is_none()
        ));
        state.apply(Event::ToolStarted {
            call_id: "call_1".into(),
        });
        assert!(matches!(
            &state.blocks[0],
            ChatBlock::Tool {
                status: ToolStatus::Running,
                elapsed,
                ..
            } if elapsed.started.is_some()
        ));
        state.apply(Event::ToolResult {
            call_id: "call_1".into(),
            output: "contents".into(),
            success: true,
            diff: None,
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
    fn visibility_commands_change_existing_and_new_blocks() {
        let mut state = UiState::new();
        state.apply(Event::ReasoningDelta("existing".into()));
        state.apply(Event::ToolCallDelta {
            index: 0,
            name: Some("read".into()),
            arguments: "{}".into(),
        });
        state.toggle_thinking_default();
        state.toggle_tools_default();
        assert!(state.notice.is_none());

        assert!(matches!(
            state.blocks[0],
            ChatBlock::Thinking { expanded: true, .. }
        ));
        assert!(matches!(
            state.blocks[1],
            ChatBlock::Tool { expanded: true, .. }
        ));

        state.apply(Event::ReasoningDelta("visible".into()));
        state.apply(Event::ToolCallDelta {
            index: 1,
            name: Some("read".into()),
            arguments: "{}".into(),
        });

        assert!(matches!(
            state.blocks[2],
            ChatBlock::Thinking { expanded: true, .. }
        ));
        assert!(matches!(
            state.blocks[3],
            ChatBlock::Tool { expanded: true, .. }
        ));

        state.toggle_thinking_default();
        state.toggle_tools_default();
        assert!(state.blocks.iter().all(|block| !matches!(
            block,
            ChatBlock::Thinking { expanded: true, .. } | ChatBlock::Tool { expanded: true, .. }
        )));
    }

    #[test]
    fn plan_updates_open_the_pane_and_plan_tools_stay_in_chat() {
        let mut state = UiState::new();
        state.apply(Event::PlanChanged(Some(ExecutionPlan {
            explanation: None,
            plan: vec![crate::tool::PlanStep {
                step: "implement pane".into(),
                status: crate::tool::PlanStatus::InProgress,
            }],
        })));
        assert!(state.plan_panel);
        assert_eq!(state.plan.as_ref().unwrap().plan[0].step, "implement pane");

        state.toggle_plan_panel();
        assert!(!state.plan_panel);
        state.apply(Event::ToolCallDelta {
            index: 0,
            name: Some("update_plan".into()),
            arguments: "{}".into(),
        });
        assert!(matches!(
            state.blocks[0],
            ChatBlock::Tool { ref name, .. } if name == "update_plan"
        ));
    }

    #[test]
    fn fullscreen_diff_has_independent_open_and_scroll_state() {
        let mut state = UiState::new();
        state.project.git_diff = "old file diff".into();
        state.project.git_diff_path = Some("old.rs".into());
        state.git_diff_scroll = 10;

        state.open_fullscreen_git_diff();
        assert!(state.git_fullscreen_diff);
        assert_eq!(state.git_diff_scroll, 0);
        assert!(state.project.git_diff.is_empty());
        assert!(state.project.git_diff_path.is_none());

        state.close_fullscreen_git_diff();
        assert!(!state.git_fullscreen_diff);
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
    fn home_and_end_move_around_collapsed_pastes() {
        let mut state = UiState::new();
        state.insert_char('a');
        state.insert_paste("long paste", 4);
        state.insert_char('z');

        state.move_input_home();
        state.insert_char('>');
        state.move_input_end();
        state.insert_char('<');

        assert_eq!(state.input, ">along pastez<");
        assert!(
            state.input_lines()[0]
                .spans
                .iter()
                .any(|span| span.content == " Pasted 10 chars ")
        );
    }

    #[test]
    fn word_navigation_uses_non_alphanumeric_boundaries() {
        let mut state = UiState::new();
        state.set_input("alpha-beta gamma".into());

        state.move_input_word_left();
        state.insert_char('|');
        assert_eq!(state.input, "alpha-beta |gamma");

        state.move_input_word_left();
        state.insert_char('|');
        assert_eq!(state.input, "alpha-|beta |gamma");

        state.move_input_word_right();
        state.insert_char('|');
        assert_eq!(state.input, "alpha-|beta ||gamma");
    }

    #[test]
    fn word_backspace_deletes_words_but_keeps_punctuation_boundaries() {
        let mut state = UiState::new();
        state.set_input("alpha-beta gamma".into());

        state.delete_input_word_back();
        assert_eq!(state.input, "alpha-beta ");
        state.delete_input_word_back();
        assert_eq!(state.input, "alpha-");
    }

    #[test]
    fn word_navigation_keeps_collapsed_pastes_atomic() {
        let mut state = UiState::new();
        state.insert_char('a');
        state.insert_paste("long paste", 4);
        state.insert_char('z');

        state.move_input_word_left();
        assert_eq!(state.input_cursor, "along paste".len());
        state.move_input_word_left();
        assert_eq!(state.input_cursor, 1);
    }

    #[test]
    fn search_word_editing_uses_the_same_boundaries() {
        let mut search = ChatSearch {
            query: "alpha-beta gamma".into(),
            cursor: "alpha-beta gamma".len(),
            current: 0,
            total: 0,
        };

        search.move_word_left();
        assert_eq!(search.cursor, "alpha-beta ".len());
        search.delete_word_back();
        assert_eq!(search.query, "alpha-gamma");
        assert_eq!(search.cursor, "alpha-".len());
    }

    #[test]
    fn image_plates_are_atomic_input_items() {
        let mut state = UiState::new();
        state.insert_char('a');
        state.insert_image(ImageContent {
            mime_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
            path: None,
            width: 10,
            height: 20,
        });
        state.insert_char('z');

        assert!(
            state.input_lines()[0]
                .spans
                .iter()
                .any(|span| span.content == " Image · 10×20 ")
        );
        let plate = state.input_lines();
        let image = plate[0]
            .spans
            .iter()
            .position(|span| span.content == " Image · 10×20 ")
            .unwrap();
        assert_eq!(plate[0].spans[image + 1].content, " ");

        state.move_input_left();
        state.backspace();
        let prompt = state.take_input().unwrap();
        assert_eq!(prompt.content, "az");
        assert!(prompt.images.is_empty());
    }

    #[test]
    fn pending_image_load_is_visible_and_blocks_submission() {
        let mut state = UiState::new();
        let id = state.begin_image_load("Processing image");
        state.insert_char('x');

        assert!(state.image_loading());
        assert!(state.take_input().is_none());
        assert!(state.input_lines()[0].spans.iter().any(|span| {
            span.content.starts_with(" Processing image · ") && span.style.bg == Some(Color::Yellow)
        }));

        assert!(state.finish_image_load(
            id,
            ImageContent {
                mime_type: "image/png".into(),
                data: "aW1hZ2U=".into(),
                path: None,
                width: 10,
                height: 20,
            }
        ));
        assert!(!state.image_loading());
        assert_eq!(state.take_input().unwrap().content, "x");
    }

    #[test]
    fn clipboard_text_replaces_pending_plate_at_its_original_position() {
        let mut state = UiState::new();
        state.insert_char('a');
        let id = state.begin_image_load("Reading clipboard");
        state.insert_char('z');

        assert!(state.replace_image_load_with_paste(id, "one\ntwo", 200));
        let prompt = state.take_input().unwrap();
        assert_eq!(prompt.content, "aone\ntwoz");
        assert!(prompt.images.is_empty());
    }

    #[test]
    fn image_only_input_can_be_submitted() {
        let mut state = UiState::new();
        state.insert_image(ImageContent {
            mime_type: "image/png".into(),
            data: "aW1hZ2U=".into(),
            path: None,
            width: 1,
            height: 1,
        });

        let prompt = state.take_input().unwrap();
        assert!(prompt.content.is_empty());
        assert_eq!(prompt.images.len(), 1);
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

    #[test]
    fn terminal_carriage_returns_can_be_normalized_before_paste() {
        let mut state = UiState::new();
        let text = "one\r\ntwo\rthree"
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        state.insert_paste(&text, 200);

        assert_eq!(state.input, "one\ntwo\nthree");
        assert_eq!(state.input_lines().len(), 3);
    }

    #[test]
    fn elapsed_time_stays_frozen_while_paused() {
        let mut elapsed = Elapsed::started();
        elapsed.finish();
        let paused = elapsed.value();

        std::thread::sleep(Duration::from_millis(2));

        assert_eq!(elapsed.value(), paused);
        elapsed.resume();
        assert!(elapsed.started.is_some());
    }
}
