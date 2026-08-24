mod history;
mod state;

use std::{
    io::{self, Write},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event as TerminalEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
        KeyboardEnhancementFlags, MouseButton, MouseEvent, MouseEventKind,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};
use pulldown_cmark::{Alignment, Event as MarkdownEvent, Options, Parser, Tag, TagEnd};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use tokio::sync::mpsc;

use crate::{
    config::Config,
    runtime::{Command, Event, ImageContent, UserPrompt},
    tool::PlanStatus,
};
use history::PromptHistory;
use state::{ChatBlock, MessageKind, TextPoint, TextSelection, ToolStatus, UiState};

#[derive(Clone, Copy)]
struct SlashCommand {
    name: &'static str,
    title: &'static str,
    hotkey: &'static str,
    argument: bool,
}

enum LoadedInput {
    Image(ImageContent),
    Text(String),
}

struct InputLoad {
    id: u64,
    action: &'static str,
    result: Result<LoadedInput>,
}

#[derive(Clone, Copy)]
struct MouseAreas {
    body: Rect,
    conversation: Rect,
    side: Option<Rect>,
    git: Option<Rect>,
    plan: Option<Rect>,
    input: Rect,
}

const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/new",
        title: "New session",
        hotkey: "Ctrl+N",
        argument: false,
    },
    SlashCommand {
        name: "/save",
        title: "Save session",
        hotkey: "Ctrl+S",
        argument: false,
    },
    SlashCommand {
        name: "/add",
        title: "Add context file",
        hotkey: "—",
        argument: true,
    },
    SlashCommand {
        name: "/drop",
        title: "Drop context file",
        hotkey: "—",
        argument: true,
    },
    SlashCommand {
        name: "/image",
        title: "Attach image file",
        hotkey: "Ctrl+V",
        argument: true,
    },
    SlashCommand {
        name: "/diff",
        title: "Open Git diff",
        hotkey: "Ctrl+D",
        argument: false,
    },
    SlashCommand {
        name: "/model",
        title: "Switch model",
        hotkey: "Alt+M",
        argument: false,
    },
    SlashCommand {
        name: "/reason",
        title: "Switch reasoning effort",
        hotkey: "Alt+R",
        argument: false,
    },
    SlashCommand {
        name: "/thinking",
        title: "Toggle thinking visibility",
        hotkey: "Alt+T",
        argument: false,
    },
    SlashCommand {
        name: "/tools",
        title: "Toggle tool visibility",
        hotkey: "Alt+O",
        argument: false,
    },
    SlashCommand {
        name: "/plan",
        title: "Toggle plan pane",
        hotkey: "—",
        argument: false,
    },
];

const MIN_CHAT_WIDTH: u16 = 40;
const MIN_GIT_WIDTH: u16 = 24;
const MIN_SIDE_PANE_HEIGHT: u16 = 4;
const STATUS_WIDTH: usize = "connecting".len();

pub async fn run(
    config: Config,
    commands: mpsc::Sender<Command>,
    mut events: mpsc::Receiver<Event>,
) -> Result<()> {
    let mut history = PromptHistory::load().await?;
    let mut terminal = TerminalGuard::new()?;
    let mut state = UiState::new();
    let (input_load_tx, mut input_load_rx) = mpsc::unbounded_channel();
    let mut chat_height = 0;
    let mut chat_size = None;
    loop {
        let size = terminal.terminal.size()?;
        state.expire_toast();
        if state.git_fullscreen_diff {
            state.git_diff_scroll = state
                .git_diff_scroll
                .min(git_diff_max_scroll(&state, size.height));
        }
        let page = Rect::new(0, 0, size.width, size.height);
        let body = page_areas(page, &state)[1];
        let (git, plan) = side_areas(body, &state);
        if let Some(area) = git {
            state.git_status_scroll = state
                .git_status_scroll
                .min(git_status_max_scroll(&state, area.height));
            state.git_panel_diff_scroll = state
                .git_panel_diff_scroll
                .min(git_panel_diff_max_scroll(&state, area.height));
        }
        if let Some(area) = plan {
            state.plan_scroll = state.plan_scroll.min(plan_max_scroll(&state, area.height));
        }
        let chat = conversation_viewport_area(page, &state);
        let current_chat_size = (chat.width, chat.height);
        if state.git_fullscreen_diff && chat_size != Some(current_chat_size) {
            chat_height = chat_layout(&state, chat).lines.len();
        }
        state.scroll = state.scroll.min(state.chat_scroll_max);
        if !state.git_fullscreen_diff {
            ensure_selected_visible(&mut state, chat);
        }
        let mut rendered_chat_height = 0;
        terminal
            .terminal
            .draw(|frame| rendered_chat_height = draw(frame, &config, &state, chat_height))?;
        if !state.git_fullscreen_diff {
            chat_height = rendered_chat_height;
        }
        chat_size = Some(current_chat_size);
        set_chat_scroll_max(&mut state, chat.height, chat_height);
        tokio::select! {
            event = events.recv() => if let Some(event) = event {
                chat_height = apply_runtime_event(&mut state, event, chat, chat_height);
            },
            load = input_load_rx.recv() => if let Some(load) = load {
                apply_input_load(load, &config, &mut state);
            },
            _ = tokio::time::sleep(Duration::from_millis(25)) => {
                while event::poll(Duration::ZERO)? {
                    match event::read()? {
                        TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => {
                            let page = Rect::new(0, 0, size.width, size.height);
                            if handle_key(key, page, &config, &mut state, &mut history, &commands, &input_load_tx).await? {
                                commands.send(Command::Shutdown).await.ok(); return Ok(());
                            }
                        }
                        TerminalEvent::Mouse(mouse) => {
                            let size = terminal.terminal.size()?;
                            let page = Rect::new(0, 0, size.width, size.height);
                            let [_, body, input] = page_areas(page, &state);
                            let area = conversation_area(page, &state);
                            let side = git_split(body, &state).1;
                            let (git, plan) = side_areas(body, &state);
                            handle_mouse(
                                mouse,
                                &mut state,
                                MouseAreas {
                                    body,
                                    conversation: area,
                                    side,
                                    git,
                                    plan,
                                    input,
                                },
                                &commands,
                            ).await?;
                        }
                        TerminalEvent::Paste(text) => {
                            history.reset_navigation();
                            let text = text.replace("\r\n", "\n").replace('\r', "\n");
                            state.insert_paste(&text, config.paste_collapse_chars);
                            state.palette_selected = 0;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn apply_runtime_event(state: &mut UiState, event: Event, chat: Rect, old_height: usize) -> usize {
    let preserve_viewport = old_height > 0
        && state.scroll > 0
        && matches!(
            &event,
            Event::GenerationStarted
                | Event::ModelRequestStarted(_)
                | Event::ResponseHeadersReceived
                | Event::ResponseStarted
                | Event::ReasoningDelta(_)
                | Event::TextDelta(_)
                | Event::ToolCallDelta { .. }
                | Event::ToolCallFinished { .. }
                | Event::ToolStarted { .. }
                | Event::ApprovalRequested(_)
                | Event::ToolResult { .. }
                | Event::Retrying { .. }
                | Event::CompactionStarted
                | Event::GenerationFinished
                | Event::Saved
                | Event::Error(_)
        );
    state.apply(event);
    let new_height = chat_layout(state, chat).lines.len();
    if preserve_viewport {
        if new_height >= old_height {
            let delta = u16::try_from(new_height - old_height).unwrap_or(u16::MAX);
            state.scroll = state.scroll.saturating_add(delta);
        } else {
            let delta = u16::try_from(old_height - new_height).unwrap_or(u16::MAX);
            state.scroll = state.scroll.saturating_sub(delta);
        }
    }
    set_chat_scroll_max(state, chat.height, new_height);
    new_height
}

async fn handle_key(
    key: KeyEvent,
    screen: Rect,
    config: &Config,
    state: &mut UiState,
    history: &mut PromptHistory,
    commands: &mpsc::Sender<Command>,
    input_loads: &mpsc::UnboundedSender<InputLoad>,
) -> Result<bool> {
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Ok(true);
    }
    if handle_fullscreen_diff_key(key, state, screen.height) {
        return Ok(false);
    }
    if key.code == KeyCode::Esc && state.generating {
        state.approval = None;
        commands.send(Command::Cancel).await?;
        return Ok(false);
    }
    if let Some(call) = &state.approval {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                state.notice = Some(format!("allowed {}", call.name));
                state.approval = None;
                commands.send(Command::Approve(true)).await?;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                state.notice = Some(format!("denied {}", call.name));
                state.approval = None;
                commands.send(Command::Approve(false)).await?;
            }
            _ => {}
        }
        return Ok(false);
    }
    if let Some(command) = hotkey_command(key) {
        dispatch(
            command.into(),
            Vec::new(),
            config,
            state,
            history,
            commands,
            input_loads,
        )
        .await?;
        return Ok(false);
    }
    if state.conversation_focused() {
        match key.code {
            KeyCode::Esc => state.focus_input(),
            KeyCode::Up => state.select_previous(),
            KeyCode::Down => state.select_next(),
            KeyCode::Enter | KeyCode::Char(' ') => {
                state.toggle_selected();
                clamp_chat_scroll(state, conversation_viewport_area(screen, state));
            }
            KeyCode::PageUp => scroll_chat_up(state, 8),
            KeyCode::PageDown => scroll_chat_down(state, 8),
            _ => {}
        }
        return Ok(false);
    }
    if key.code == KeyCode::Esc && palette_commands(&state.input).is_some() {
        state.clear_input();
        state.palette_selected = 0;
        return Ok(false);
    }
    if key.code == KeyCode::Esc
        || (key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('g'))
    {
        commands.send(Command::Cancel).await?;
        return Ok(false);
    }
    match key.code {
        KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
            history.reset_navigation();
            state.insert_char('\n');
        }
        KeyCode::Enter => {
            if let Some(filtered) = palette_commands(&state.input)
                && let Some(command) = filtered.get(state.palette_selected).copied()
            {
                if command.argument {
                    state.set_input(format!("{} ", command.name));
                } else {
                    state.clear_input();
                    dispatch(
                        command.name.into(),
                        Vec::new(),
                        config,
                        state,
                        history,
                        commands,
                        input_loads,
                    )
                    .await?;
                }
                return Ok(false);
            }
            if state.has_input_images() && !model_supports_vision(config, state) {
                state.set_error("the current model does not support image input");
                return Ok(false);
            }
            if let Some(input) = state.take_input() {
                dispatch(
                    input.content,
                    input.images,
                    config,
                    state,
                    history,
                    commands,
                    input_loads,
                )
                .await?;
            }
        }
        KeyCode::Backspace => {
            history.reset_navigation();
            state.backspace();
            state.palette_selected = 0;
        }
        KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            if !model_supports_vision(config, state) {
                state.set_error("the current model does not support image input");
            } else {
                queue_input_load(
                    state,
                    input_loads,
                    "Processing image",
                    "paste image",
                    || clipboard_image().map(LoadedInput::Image),
                );
            }
        }
        KeyCode::Insert if is_shift_insert(key) => {
            history.reset_navigation();
            queue_input_load(
                state,
                input_loads,
                "Reading clipboard",
                "paste clipboard",
                clipboard_content,
            );
        }
        KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
            history.reset_navigation();
            state.insert_char(character);
            state.palette_selected = 0;
        }
        KeyCode::Up if state.has_input_images() => {}
        KeyCode::Down if state.has_input_images() => {}
        KeyCode::Up => {
            if palette_commands(&state.input).is_some() {
                state.palette_selected = state.palette_selected.saturating_sub(1);
            } else {
                let mut input = state.input.clone();
                history.previous(&mut input);
                state.set_input(input);
            }
        }
        KeyCode::Down => {
            if let Some(filtered) = palette_commands(&state.input) {
                state.palette_selected =
                    (state.palette_selected + 1).min(filtered.len().saturating_sub(1));
            } else {
                let mut input = state.input.clone();
                history.next(&mut input);
                state.set_input(input);
            }
        }
        KeyCode::Left => state.move_input_left(),
        KeyCode::Right => state.move_input_right(),
        KeyCode::Home => state.move_input_home(),
        KeyCode::End => state.move_input_end(),
        KeyCode::Delete => state.delete(),
        KeyCode::Tab => {
            history.reset_navigation();
            state.insert_char('\t');
        }
        KeyCode::PageUp => scroll_chat_up(state, 8),
        KeyCode::PageDown => scroll_chat_down(state, 8),
        _ => {}
    }
    Ok(false)
}

fn handle_fullscreen_diff_key(key: KeyEvent, state: &mut UiState, screen_height: u16) -> bool {
    if !state.git_fullscreen_diff {
        return false;
    }
    match key.code {
        KeyCode::Esc => state.close_fullscreen_git_diff(),
        KeyCode::Up | KeyCode::PageUp => {
            state.git_diff_scroll = state.git_diff_scroll.saturating_sub(8)
        }
        KeyCode::Down | KeyCode::PageDown => {
            state.git_diff_scroll = state
                .git_diff_scroll
                .saturating_add(8)
                .min(git_diff_max_scroll(state, screen_height))
        }
        _ => {}
    }
    true
}

async fn dispatch(
    input: String,
    images: Vec<ImageContent>,
    config: &Config,
    state: &mut UiState,
    history: &mut PromptHistory,
    commands: &mpsc::Sender<Command>,
    input_loads: &mpsc::UnboundedSender<InputLoad>,
) -> Result<()> {
    if !images.is_empty() {
        submit(input, images, state, history, commands).await?;
        return Ok(());
    }
    let is_command = input.starts_with('/');
    let (command, argument) = input
        .split_once(' ')
        .map_or((input.as_str(), ""), |parts| parts);
    match command {
        "/new" => {
            commands
                .send(Command::NewSession(
                    (!argument.is_empty()).then(|| argument.to_owned()),
                ))
                .await?
        }
        "/save" if argument.is_empty() => commands.send(Command::Save).await?,
        "/add" if !argument.is_empty() => {
            commands
                .send(Command::AddContext(argument.to_owned()))
                .await?
        }
        "/drop" if !argument.is_empty() => {
            commands
                .send(Command::DropContext(argument.to_owned()))
                .await?
        }
        "/image" if !argument.is_empty() => {
            if !model_supports_vision(config, state) {
                state.set_error("the current model does not support image input");
            } else {
                let path = Path::new(argument.trim()).to_owned();
                queue_input_load(
                    state,
                    input_loads,
                    "Processing image",
                    "attach image",
                    move || image_from_path(&path).map(LoadedInput::Image),
                );
            }
        }
        "/model" if argument.is_empty() => commands.send(Command::NextModel).await?,
        "/reason" if argument.is_empty() => commands.send(Command::NextReasoningEffort).await?,
        "/thinking" if argument.is_empty() => state.toggle_thinking_default(),
        "/tools" if argument.is_empty() => state.toggle_tools_default(),
        "/plan" if argument.is_empty() => state.toggle_plan_panel(),
        "/diff" if argument.is_empty() => {
            state.open_fullscreen_git_diff();
            commands.send(Command::GitDiff(None)).await?;
        }
        value if value.starts_with('/') => {
            history.reset_navigation();
            state.set_error(format!("unknown or incomplete command: {value}"))
        }
        _ => {
            submit(input, Vec::new(), state, history, commands).await?;
        }
    }
    if is_command {
        history.reset_navigation();
    }
    Ok(())
}

async fn submit(
    content: String,
    images: Vec<ImageContent>,
    state: &mut UiState,
    history: &mut PromptHistory,
    commands: &mpsc::Sender<Command>,
) -> Result<()> {
    if !content.is_empty()
        && let Err(error) = history.record(&content).await
    {
        state.notice = Some(format!("history was not saved: {error:#}"));
    }
    state.push_user_with_images(content.clone(), images.clone());
    commands
        .send(Command::Submit(UserPrompt { content, images }))
        .await?;
    Ok(())
}

fn model_supports_vision(config: &Config, state: &UiState) -> bool {
    config
        .models
        .iter()
        .find(|model| model.name == state.model || model.id == state.model)
        .is_some_and(|model| model.vision)
}

fn clipboard_image() -> Result<ImageContent> {
    let mut clipboard = arboard::Clipboard::new().context("open clipboard")?;
    let image = clipboard.get_image().context("clipboard has no image")?;
    encode_image(image.width as u32, image.height as u32, &image.bytes)
}

fn clipboard_content() -> Result<LoadedInput> {
    let mut clipboard = arboard::Clipboard::new().context("open clipboard")?;
    match clipboard.get_text() {
        Ok(text) => Ok(LoadedInput::Text(text)),
        Err(text_error) => {
            let image = clipboard
                .get_image()
                .with_context(|| format!("clipboard has no text ({text_error}) or image"))?;
            encode_image(image.width as u32, image.height as u32, &image.bytes)
                .map(LoadedInput::Image)
        }
    }
}

fn image_from_path(path: &Path) -> Result<ImageContent> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let decoded = image::load_from_memory(&bytes)
        .with_context(|| format!("decode {}", path.display()))?
        .into_rgba8();
    encode_image(decoded.width(), decoded.height(), decoded.as_raw())
}

fn encode_image(width: u32, height: u32, rgba: &[u8]) -> Result<ImageContent> {
    if width == 0 || height == 0 {
        bail!("image has no pixels");
    }
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(rgba, width, height, ExtendedColorType::Rgba8)
        .context("encode image as PNG")?;
    Ok(ImageContent {
        mime_type: "image/png".into(),
        data: STANDARD.encode(png),
        path: None,
        width,
        height,
    })
}

fn queue_input_load(
    state: &mut UiState,
    input_loads: &mpsc::UnboundedSender<InputLoad>,
    label: &str,
    action: &'static str,
    load: impl FnOnce() -> Result<LoadedInput> + Send + 'static,
) {
    let id = state.begin_image_load(label);
    let input_loads = input_loads.clone();
    tokio::task::spawn_blocking(move || {
        input_loads.send(InputLoad {
            id,
            action,
            result: load(),
        })
    });
}

fn apply_input_load(load: InputLoad, config: &Config, state: &mut UiState) {
    match load.result {
        Ok(LoadedInput::Image(_)) if !model_supports_vision(config, state) => {
            if state.fail_image_load(load.id) {
                state.set_error("the current model does not support image input");
            }
        }
        Ok(LoadedInput::Image(image)) => {
            state.finish_image_load(load.id, image);
        }
        Ok(LoadedInput::Text(text)) => {
            let text = text.replace("\r\n", "\n").replace('\r', "\n");
            state.replace_image_load_with_paste(load.id, &text, config.paste_collapse_chars);
        }
        Err(error) => {
            if state.fail_image_load(load.id) {
                state.set_error(format!("{}: {error:#}", load.action));
            }
        }
    }
}

async fn handle_mouse(
    mouse: MouseEvent,
    state: &mut UiState,
    areas: MouseAreas,
    commands: &mpsc::Sender<Command>,
) -> Result<()> {
    let MouseAreas {
        body,
        conversation,
        side,
        git,
        plan,
        input,
    } = areas;
    if state.git_fullscreen_diff {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                state.git_diff_scroll = state.git_diff_scroll.saturating_sub(3)
            }
            MouseEventKind::ScrollDown => {
                state.git_diff_scroll = state
                    .git_diff_scroll
                    .saturating_add(3)
                    .min(git_diff_max_scroll(state, conversation.height))
            }
            _ => {}
        }
        return Ok(());
    }
    if state.git_split_dragging {
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                resize_git_panel(state, body, mouse.column);
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left) => {
                resize_git_panel(state, body, mouse.column);
                state.git_split_dragging = false;
                return Ok(());
            }
            _ => {}
        }
    }
    if state.plan_split_dragging {
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(side) = side {
                    resize_plan_panel(state, side, mouse.row);
                }
                return Ok(());
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(side) = side {
                    resize_plan_panel(state, side, mouse.row);
                }
                state.plan_split_dragging = false;
                return Ok(());
            }
            _ => {}
        }
    }
    if let Some(area) = side
        && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        && (mouse.column == area.x || mouse.column.saturating_add(1) == area.x)
        && (body.y..body.bottom()).contains(&mouse.row)
    {
        state.git_split_dragging = true;
        return Ok(());
    }
    if let (Some(side), Some(plan)) = (side, plan)
        && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        && (mouse.row == plan.y || mouse.row.saturating_add(1) == plan.y)
        && (side.x..side.right()).contains(&mouse.column)
    {
        state.plan_split_dragging = true;
        return Ok(());
    }
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) && mouse.row == input.y {
        let model_start = input.x + 2;
        let model_end = model_start + state.model.chars().count() as u16;
        let reason_start = model_end + 3;
        let reason_end = reason_start
            + state
                .reasoning_effort
                .map_or(3, |effort| effort.to_string().chars().count() as u16);
        if (model_start..model_end).contains(&mouse.column) {
            commands.send(Command::NextModel).await?;
        } else if (reason_start..reason_end).contains(&mouse.column) {
            commands.send(Command::NextReasoningEffort).await?;
        }
        return Ok(());
    }
    if let Some(area) = git
        && area.contains((mouse.column, mouse.row).into())
    {
        match mouse.kind {
            MouseEventKind::ScrollUp if !state.git_diff_mode => {
                state.git_status_scroll = state.git_status_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown if !state.git_diff_mode => {
                state.git_status_scroll = state
                    .git_status_scroll
                    .saturating_add(3)
                    .min(git_status_max_scroll(state, area.height));
            }
            MouseEventKind::ScrollUp if state.git_diff_mode => {
                state.git_panel_diff_scroll = state.git_panel_diff_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown if state.git_diff_mode => {
                state.git_panel_diff_scroll = state
                    .git_panel_diff_scroll
                    .saturating_add(3)
                    .min(git_panel_diff_max_scroll(state, area.height));
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let row = mouse.row.saturating_sub(area.y + 1) as usize;
                if state.git_diff_mode {
                    if row == 0 {
                        state.git_diff_mode = false;
                        state.git_panel_diff_scroll = 0;
                        commands.send(Command::RefreshProject).await?;
                    }
                } else {
                    let row = row + state.git_status_scroll as usize;
                    if let Some(file) = state.project.git_files.get(row) {
                        state.git_diff_mode = true;
                        state.git_panel_diff_scroll = 0;
                        commands
                            .send(Command::GitDiff(Some(file.path.clone())))
                            .await?;
                    }
                }
            }
            _ => {}
        }
        return Ok(());
    }
    if let Some(area) = plan
        && area.contains((mouse.column, mouse.row).into())
    {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                state.plan_scroll = state.plan_scroll.saturating_sub(3);
            }
            MouseEventKind::ScrollDown => {
                state.plan_scroll = state
                    .plan_scroll
                    .saturating_add(3)
                    .min(plan_max_scroll(state, area.height));
            }
            _ => {}
        }
        return Ok(());
    }
    if !conversation.contains((mouse.column, mouse.row).into()) {
        return Ok(());
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(diff) = chat_diff_hit_test(state, conversation, mouse.column, mouse.row) {
                state.text_selection = None;
                state.selection_anchor = None;
                state.open_fullscreen_tool_diff(diff);
                return Ok(());
            }
            let point = chat_point(state, conversation, mouse.column, mouse.row);
            state.selection_anchor = Some(point);
            state.text_selection = Some(TextSelection {
                start: point,
                end: point,
            });
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if let Some(start) = state.selection_anchor {
                state.text_selection = Some(TextSelection {
                    start,
                    end: chat_point(state, conversation, mouse.column, mouse.row),
                });
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(start) = state.selection_anchor.take() {
                let end = chat_point(state, conversation, mouse.column, mouse.row);
                if start == end {
                    state.text_selection = None;
                    if let Some(index) = chat_hit_test(state, conversation, mouse.row) {
                        state.focus_input();
                        state.toggle(index);
                        clamp_chat_scroll(state, conversation);
                    }
                } else {
                    state.text_selection = Some(TextSelection { start, end });
                    let layout = chat_layout(state, conversation);
                    let selected = selected_text(&layout.lines, TextSelection { start, end });
                    if !selected.is_empty() {
                        copy_to_clipboard(&selected)?;
                        state.show_toast("copied to clipboard");
                    }
                }
            }
        }
        MouseEventKind::ScrollUp => scroll_chat_up(state, 3),
        MouseEventKind::ScrollDown => scroll_chat_down(state, 3),
        _ => {}
    }
    Ok(())
}

fn chat_point(state: &UiState, area: Rect, column: u16, row: u16) -> TextPoint {
    let layout = chat_layout(state, area);
    TextPoint {
        row: (layout.offset + row.saturating_sub(area.y))
            .min(layout.lines.len().saturating_sub(1) as u16),
        column: column
            .saturating_sub(area.x + 1)
            .min(area.width.saturating_sub(3)),
    }
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let encoded = STANDARD.encode(text);
    let mut stdout = io::stdout();
    write!(stdout, "\x1b]52;c;{encoded}\x07")?;
    stdout.flush()?;
    Ok(())
}

fn page_areas(area: Rect, state: &UiState) -> [Rect; 3] {
    let input_height = (state.input_lines().len() as u16 + 2).clamp(3, 8);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(input_height),
        ])
        .split(area);
    [areas[0], areas[1], areas[2]]
}

fn conversation_area(area: Rect, state: &UiState) -> Rect {
    if state.git_fullscreen_diff {
        return area;
    }
    conversation_viewport_area(area, state)
}

fn conversation_viewport_area(area: Rect, state: &UiState) -> Rect {
    let body = page_areas(area, state)[1];
    git_split(body, state).0
}

fn side_areas(body: Rect, state: &UiState) -> (Option<Rect>, Option<Rect>) {
    let Some(side) = git_split(body, state).1 else {
        return (None, None);
    };
    let (git, plan) = side_split(side, state);
    (Some(git), plan)
}

fn git_split(body: Rect, state: &UiState) -> (Rect, Option<Rect>) {
    if !state.git_panel || body.width < 100 {
        return (body, None);
    }
    let default_width = body.width.saturating_mul(30) / 100;
    let max_width = body.width.saturating_sub(MIN_CHAT_WIDTH);
    let width = state
        .git_panel_width
        .unwrap_or(default_width)
        .clamp(MIN_GIT_WIDTH, max_width);
    let areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(MIN_CHAT_WIDTH), Constraint::Length(width)])
        .split(body);
    (areas[0], Some(areas[1]))
}

fn side_split(side: Rect, state: &UiState) -> (Rect, Option<Rect>) {
    if !state.plan_panel || side.height < MIN_SIDE_PANE_HEIGHT * 2 {
        return (side, None);
    }
    let default_height = side.height / 2;
    let max_height = side.height.saturating_sub(MIN_SIDE_PANE_HEIGHT);
    let height = state
        .plan_panel_height
        .unwrap_or(default_height)
        .clamp(MIN_SIDE_PANE_HEIGHT, max_height);
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(MIN_SIDE_PANE_HEIGHT),
            Constraint::Length(height),
        ])
        .split(side);
    (areas[0], Some(areas[1]))
}

fn resize_git_panel(state: &mut UiState, body: Rect, column: u16) {
    if body.width < MIN_CHAT_WIDTH + MIN_GIT_WIDTH {
        state.git_split_dragging = false;
        return;
    }
    let max_width = body.width.saturating_sub(MIN_CHAT_WIDTH);
    let width = body
        .right()
        .saturating_sub(column)
        .clamp(MIN_GIT_WIDTH, max_width);
    state.git_panel_width = Some(width);
}

fn resize_plan_panel(state: &mut UiState, side: Rect, row: u16) {
    if side.height < MIN_SIDE_PANE_HEIGHT * 2 {
        state.plan_split_dragging = false;
        return;
    }
    let max_height = side.height.saturating_sub(MIN_SIDE_PANE_HEIGHT);
    let height = side
        .bottom()
        .saturating_sub(row)
        .clamp(MIN_SIDE_PANE_HEIGHT, max_height);
    state.plan_panel_height = Some(height);
}

fn draw(
    frame: &mut ratatui::Frame,
    config: &Config,
    state: &UiState,
    hidden_chat_height: usize,
) -> usize {
    if state.git_fullscreen_diff {
        draw_fullscreen_git(frame, state);
        return hidden_chat_height;
    }
    let [header, body, input] = page_areas(frame.area(), state);
    let effort = state
        .reasoning_effort
        .map(|value| value.to_string())
        .unwrap_or_else(|| "off".into());
    let price = state.total_tokens as f64 * config.price_per_token;
    let context_percent = if state.max_context_tokens == 0 {
        0
    } else {
        (state.context_tokens.saturating_mul(100) / state.max_context_tokens).min(100)
    };
    let (status, status_color) = app_status(state);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw(" "),
            Span::styled(
                format!("{status:<STATUS_WIDTH$}"),
                Style::default().fg(status_color),
            ),
            Span::raw(format!(
                "  {}  tokens:{}  context:{context_percent}%  ${price:.2}  {}",
                state.session,
                state.total_tokens,
                state.project.cwd.display()
            )),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        header,
    );

    let (chat, side) = git_split(body, state);
    let chat_height = draw_chat(frame, state, chat);
    if let Some(side) = side {
        let (git, plan) = side_split(side, state);
        draw_git(frame, state, git);
        if let Some(plan) = plan {
            draw_plan(frame, state, plan);
        }
    }

    let mut title = vec![
        Span::raw(" "),
        Span::styled(&state.model, Style::default().fg(Color::Cyan)),
        Span::raw(" · "),
        Span::styled(effort, Style::default().fg(Color::Magenta)),
    ];
    if let Some(call) = &state.approval {
        title.push(Span::raw(format!(" · allow tool {}? y/n", call.name)));
    }
    if state.generating {
        title.push(Span::styled(
            " · Esc to cancel",
            Style::default().fg(Color::Yellow),
        ));
    }
    title.push(Span::raw(" "));
    frame.render_widget(
        Paragraph::new(state.input_lines())
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Line::from(title)),
            ),
        input,
    );
    draw_command_palette(frame, state, input);

    let input_width = input.width.saturating_sub(2).max(1);
    let (row, column) = state.input_cursor(input_width);
    if !state.conversation_focused() {
        frame.set_cursor_position((input.x + 1 + column, input.y + 1 + row));
    }
    draw_toast(frame, state);
    chat_height
}

fn draw_fullscreen_git(frame: &mut ratatui::Frame, state: &UiState) {
    let tool_diff = state.fullscreen_tool_diff.is_some();
    let lines = if tool_diff || state.project.git_available {
        diff_lines(state.fullscreen_diff())
    } else {
        vec![Line::styled(
            " not a Git repository",
            Style::default().fg(Color::DarkGray),
        )]
    };
    frame.render_widget(Clear, frame.area());
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((state.git_diff_scroll, 0))
            .block(Block::default().borders(Borders::ALL).title(if tool_diff {
                " tool diff · Esc to close "
            } else {
                " git diff · Esc to close "
            })),
        frame.area(),
    );
}

fn git_diff_max_scroll(state: &UiState, height: u16) -> u16 {
    let line_count = if state.fullscreen_tool_diff.is_some() || state.project.git_available {
        state.fullscreen_diff().lines().count().max(1)
    } else {
        1
    };
    let visible = height.saturating_sub(2).max(1) as usize;
    line_count.saturating_sub(visible).min(u16::MAX as usize) as u16
}

fn git_status_max_scroll(state: &UiState, height: u16) -> u16 {
    let visible = height.saturating_sub(2).max(1) as usize;
    state
        .project
        .git_files
        .len()
        .saturating_sub(visible)
        .min(u16::MAX as usize) as u16
}

fn git_status_title(state: &UiState, height: u16) -> String {
    let total = state.project.git_files.len().max(1);
    let visible = height.saturating_sub(2).max(1) as usize;
    let offset = (state.git_status_scroll as usize).min(total.saturating_sub(visible));
    let end = (offset + visible).min(total);
    format!(" git status · {}-{end}/{total} ", offset + 1)
}

fn git_panel_diff_max_scroll(state: &UiState, height: u16) -> u16 {
    let visible = height.saturating_sub(3).max(1) as usize;
    state
        .project
        .git_diff
        .lines()
        .count()
        .max(1)
        .saturating_sub(visible)
        .min(u16::MAX as usize) as u16
}

fn git_panel_diff_title(state: &UiState, height: u16) -> String {
    let total = state.project.git_diff.lines().count().max(1);
    let visible = height.saturating_sub(3).max(1) as usize;
    let offset = (state.git_panel_diff_scroll as usize).min(total.saturating_sub(visible));
    let end = (offset + visible).min(total);
    let path = state
        .project
        .git_diff_path
        .as_ref()
        .map_or_else(|| "git diff".into(), |path| path.display().to_string());
    format!(" {}-{end}/{total} · {path} ", offset + 1)
}

fn draw_toast(frame: &mut ratatui::Frame, state: &UiState) {
    let Some(message) = state.toast() else {
        return;
    };
    let width = (message.chars().count() as u16 + 4).min(frame.area().width);
    let area = Rect::new(
        frame.area().right().saturating_sub(width + 2),
        frame.area().y + 1,
        width,
        3.min(frame.area().height),
    );
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(format!(" {message}"))
            .style(Style::default().fg(Color::Black).bg(Color::Green))
            .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn palette_commands(input: &str) -> Option<Vec<SlashCommand>> {
    if !input.starts_with('/') || input.contains(char::is_whitespace) {
        return None;
    }
    let query = input.trim_start_matches('/').to_ascii_lowercase();
    Some(
        COMMANDS
            .iter()
            .copied()
            .filter(|command| {
                command.name[1..].to_ascii_lowercase().contains(&query)
                    || command.title.to_ascii_lowercase().contains(&query)
            })
            .collect(),
    )
}

fn hotkey_command(key: KeyEvent) -> Option<&'static str> {
    match (key.modifiers, key.code) {
        (KeyModifiers::CONTROL, KeyCode::Char('n')) => Some("/new"),
        (KeyModifiers::CONTROL, KeyCode::Char('s')) => Some("/save"),
        (KeyModifiers::CONTROL, KeyCode::Char('d')) => Some("/diff"),
        (KeyModifiers::ALT, KeyCode::Char('m')) => Some("/model"),
        (KeyModifiers::ALT, KeyCode::Char('r')) => Some("/reason"),
        (KeyModifiers::ALT, KeyCode::Char('t')) => Some("/thinking"),
        (KeyModifiers::ALT, KeyCode::Char('o')) => Some("/tools"),
        _ => None,
    }
}

fn is_shift_insert(key: KeyEvent) -> bool {
    key.code == KeyCode::Insert && key.modifiers.contains(KeyModifiers::SHIFT)
}

fn draw_command_palette(frame: &mut ratatui::Frame, state: &UiState, input: Rect) {
    let Some(commands) = palette_commands(&state.input) else {
        return;
    };
    let height = (commands.len() as u16 + 2).min(input.y);
    if height <= 2 {
        return;
    }
    let area = Rect::new(input.x, input.y - height, input.width, height);
    let lines = commands
        .iter()
        .enumerate()
        .map(|(index, command)| {
            let style = if index == state.palette_selected {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!(" {:<12}", command.name), style.fg(Color::Cyan)),
                Span::styled(format!("{:<32}", command.title), style),
                Span::styled(command.hotkey, style.fg(Color::DarkGray)),
            ])
        })
        .collect::<Vec<_>>();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" commands ")),
        area,
    );
}

fn draw_git(frame: &mut ratatui::Frame, state: &UiState, area: Rect) {
    if state.git_diff_mode {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(git_panel_diff_title(state, area.height));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Line::styled(
                " ← git status",
                Style::default().fg(Color::Cyan),
            )),
            Rect::new(inner.x, inner.y, inner.width, 1),
        );
        if inner.height > 1 {
            frame.render_widget(
                Paragraph::new(diff_lines(&state.project.git_diff))
                    .scroll((state.git_panel_diff_scroll, 0))
                    .wrap(Wrap { trim: false }),
                Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1),
            );
        }
        return;
    }

    let (title, lines, scroll, wrap) = if !state.project.git_available {
        (
            " git ".to_owned(),
            vec![Line::styled(
                " not a Git repository",
                Style::default().fg(Color::DarkGray),
            )],
            0,
            true,
        )
    } else {
        let lines = if state.project.git_files.is_empty() {
            vec![Line::styled(
                " working tree clean",
                Style::default().fg(Color::DarkGray),
            )]
        } else {
            state
                .project
                .git_files
                .iter()
                .map(|file| {
                    Line::from(vec![
                        Span::styled(format!(" {} ", file.status), git_status_color(&file.status)),
                        Span::raw(file.path.display().to_string()),
                    ])
                })
                .collect()
        };
        (
            git_status_title(state, area.height),
            lines,
            state.git_status_scroll,
            false,
        )
    };
    let paragraph = Paragraph::new(lines)
        .scroll((scroll, 0))
        .block(Block::default().borders(Borders::ALL).title(title));
    if wrap {
        frame.render_widget(paragraph.wrap(Wrap { trim: false }), area);
    } else {
        frame.render_widget(paragraph, area);
    }
}

fn plan_max_scroll(state: &UiState, height: u16) -> u16 {
    let lines = state.plan.as_ref().map_or(1, |plan| {
        plan.plan.len() + usize::from(plan.explanation.is_some())
    });
    let visible = height.saturating_sub(2).max(1) as usize;
    lines.saturating_sub(visible).min(u16::MAX as usize) as u16
}

fn draw_plan(frame: &mut ratatui::Frame, state: &UiState, area: Rect) {
    let (title, lines) = state.plan.as_ref().map_or_else(
        || {
            (
                " plan ".into(),
                vec![Line::styled(
                    " no plan yet",
                    Style::default().fg(Color::DarkGray),
                )],
            )
        },
        |plan| {
            let completed = plan
                .plan
                .iter()
                .filter(|step| step.status == PlanStatus::Completed)
                .count();
            let mut lines =
                Vec::with_capacity(plan.plan.len() + usize::from(plan.explanation.is_some()));
            if let Some(explanation) = &plan.explanation {
                lines.push(Line::styled(
                    format!(" {explanation}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            lines.extend(plan.plan.iter().map(|step| {
                let (marker, style) = match step.status {
                    PlanStatus::Completed => ("✓", Style::default().fg(Color::DarkGray)),
                    PlanStatus::InProgress => (
                        "●",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    PlanStatus::Pending => ("○", Style::default().fg(Color::Gray)),
                };
                Line::styled(format!(" {marker} {}", step.step), style)
            }));
            (format!(" plan · {completed}/{} ", plan.plan.len()), lines)
        },
    );
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((state.plan_scroll, 0))
            .block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn git_status_color(status: &str) -> Style {
    let color = if status.contains('?') {
        Color::Cyan
    } else if status.contains('D') {
        Color::Red
    } else if status.contains('A') {
        Color::Green
    } else {
        Color::Yellow
    };
    Style::default().fg(color)
}

fn diff_lines(diff: &str) -> Vec<Line<'static>> {
    if diff.is_empty() {
        return vec![Line::styled(
            " no changes",
            Style::default().fg(Color::DarkGray),
        )];
    }
    diff.lines()
        .map(|line| {
            let color = if line.starts_with('+') && !line.starts_with("+++") {
                Color::Green
            } else if line.starts_with('-') && !line.starts_with("---") {
                Color::Red
            } else if line.starts_with("@@") {
                Color::Cyan
            } else {
                Color::Gray
            };
            Line::styled(format!(" {line}"), Style::default().fg(color))
        })
        .collect()
}

fn draw_chat(frame: &mut ratatui::Frame, state: &UiState, area: Rect) -> usize {
    let layout = chat_layout(state, area);
    let height = layout.lines.len();
    frame.render_widget(
        Paragraph::new(layout.lines)
            .scroll((layout.offset, 0))
            .block(Block::default().borders(Borders::LEFT | Borders::RIGHT)),
        area,
    );
    height
}

fn chat_max_scroll(state: &UiState, area: Rect) -> u16 {
    max_chat_scroll(chat_layout(state, area).lines.len(), area.height)
}

fn max_chat_scroll(height: usize, visible: u16) -> u16 {
    height
        .saturating_sub(visible as usize)
        .min(u16::MAX as usize) as u16
}

fn set_chat_scroll_max(state: &mut UiState, visible: u16, height: usize) {
    state.chat_scroll_max = max_chat_scroll(height, visible);
    state.scroll = state.scroll.min(state.chat_scroll_max);
}

fn clamp_chat_scroll(state: &mut UiState, area: Rect) {
    let max = chat_max_scroll(state, area);
    state.chat_scroll_max = max;
    state.scroll = state.scroll.min(max);
}

fn scroll_chat_up(state: &mut UiState, amount: u16) {
    state.scroll = state
        .scroll
        .saturating_add(amount)
        .min(state.chat_scroll_max);
}

fn scroll_chat_down(state: &mut UiState, amount: u16) {
    state.scroll = state.scroll.min(state.chat_scroll_max);
    state.scroll = state.scroll.saturating_sub(amount);
}

struct ChatLayout {
    lines: Vec<Line<'static>>,
    headers: Vec<(usize, u16, u16)>,
    diff_buttons: Vec<(usize, u16, u16)>,
    offset: u16,
}

fn chat_layout(state: &UiState, area: Rect) -> ChatLayout {
    let mut lines = Vec::new();
    let mut header_lines = Vec::new();
    let mut diff_header_lines = Vec::new();
    for (index, block) in state.blocks.iter().enumerate() {
        match block {
            ChatBlock::Message {
                label,
                content,
                images,
                model,
                kind,
                expanded,
            } => {
                let color = match kind {
                    MessageKind::User => Color::Cyan,
                    MessageKind::Assistant => Color::Blue,
                    MessageKind::System => Color::Magenta,
                };
                if matches!(kind, MessageKind::User | MessageKind::Assistant) {
                    header_lines.push((index, lines.len()));
                    let header = format!("{} {label}", if *expanded { "▾" } else { "▸" });
                    if matches!(kind, MessageKind::Assistant) && !model.is_empty() {
                        lines.push(assistant_header(
                            header,
                            model,
                            color,
                            state.selected() == Some(index),
                        ));
                    } else {
                        lines.push(section_header(
                            header,
                            color,
                            state.selected() == Some(index),
                        ));
                    }
                    if *expanded {
                        let rendered = if matches!(kind, MessageKind::User) {
                            markdown_preserving_breaks(content)
                        } else {
                            markdown(content)
                        };
                        lines.extend(rendered.into_iter().map(pad_line));
                        lines.extend(images.iter().map(|image| {
                            let dimensions = if image.width == 0 || image.height == 0 {
                                String::new()
                            } else {
                                format!(" · {}×{}", image.width, image.height)
                            };
                            Line::styled(
                                format!(" Image{dimensions}"),
                                Style::default().fg(Color::Cyan),
                            )
                        }));
                    }
                } else {
                    lines.push(Line::styled(
                        label.clone(),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ));
                    lines.extend(markdown(content).into_iter().map(pad_line));
                }
            }
            ChatBlock::Thinking {
                content,
                expanded,
                elapsed,
            } => {
                header_lines.push((index, lines.len()));
                lines.push(section_header(
                    format!(
                        "{} Thinking · {}{}",
                        if *expanded { "▾" } else { "▸" },
                        size_label(content.chars().count()),
                        elapsed_label(elapsed.value()),
                    ),
                    Color::DarkGray,
                    state.selected() == Some(index),
                ));
                if *expanded {
                    lines.extend(markdown(content).into_iter().map(|line| {
                        pad_line(
                            line.style(
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::ITALIC),
                            ),
                        )
                    }));
                }
            }
            ChatBlock::Tool {
                name,
                arguments,
                output,
                diff,
                status,
                expanded,
                counter,
                elapsed,
                ..
            } => {
                header_lines.push((index, lines.len()));
                let color = tool_color(*status);
                let selected = state.selected() == Some(index);
                let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
                if selected {
                    style = style.bg(Color::DarkGray).fg(Color::White);
                }
                let prefix = format!(
                    "{} Tool: {}",
                    if *expanded { "▾" } else { "▸" },
                    if name.is_empty() { "…" } else { name },
                );
                let mut header = Line::from(Span::styled(prefix.clone(), style));
                if diff.is_some() {
                    let start = prefix.chars().count() as u16 + 1;
                    diff_header_lines.push((index, lines.len(), start, start + 6));
                    header.spans.push(Span::raw(" ").style(style));
                    let button_style = if selected {
                        style.add_modifier(Modifier::UNDERLINED)
                    } else {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
                    };
                    header.spans.push(Span::styled("[diff]", button_style));
                    header.spans.push(Span::raw(" ").style(style));
                }
                header.spans.push(Span::styled(
                    format!(
                        "{} · {} · {}{}",
                        argument_summary(arguments),
                        tool_status(*status),
                        counter.label(),
                        elapsed_label(elapsed.value()),
                    ),
                    style,
                ));
                lines.push(header);
                if *expanded {
                    lines.push(Line::styled(
                        " arguments",
                        Style::default().fg(Color::DarkGray),
                    ));
                    lines.extend(arguments.lines().map(|line| {
                        Line::styled(format!(" {line}"), Style::default().fg(Color::Cyan))
                    }));
                    if let Some(output) = output {
                        lines.push(Line::styled(
                            " output",
                            Style::default().fg(Color::DarkGray),
                        ));
                        lines.extend(
                            markdown(output)
                                .into_iter()
                                .map(|line| pad_line(line.style(Style::default().fg(Color::Gray)))),
                        );
                    }
                }
            }
        }
        lines.push(Line::default());
    }
    lines.push(Line::default());
    if let Some(notice) = &state.notice {
        lines.push(Line::styled(
            format!(" {notice}"),
            Style::default().fg(Color::Green),
        ));
    }
    if let Some(error) = &state.error {
        lines.push(Line::styled(
            format!(" Error: {}", error_summary(error)),
            Style::default().fg(Color::Red),
        ));
    }

    let width = area.width.saturating_sub(2).max(1);
    let (mut lines, starts) = wrap_chat_lines(lines, width);
    let content_height = lines.len() as u16;
    let offset = content_height
        .saturating_sub(area.height)
        .saturating_sub(state.scroll);
    let headers = header_lines
        .into_iter()
        .map(|(block, line)| {
            let row = starts[line];
            let end = starts.get(line + 1).copied().unwrap_or(content_height);
            (block, row, end.saturating_sub(row).max(1))
        })
        .collect();
    let diff_buttons = diff_header_lines
        .into_iter()
        .flat_map(|(block, line, start, end)| {
            let row = starts[line];
            (start..end).map(move |column| (block, row + column / width, column % width))
        })
        .collect();
    if let Some(selection) = state.text_selection {
        highlight_selection(&mut lines, selection);
    }
    ChatLayout {
        lines,
        headers,
        diff_buttons,
        offset,
    }
}

fn wrap_chat_lines(lines: Vec<Line<'static>>, width: u16) -> (Vec<Line<'static>>, Vec<u16>) {
    let width = width as usize;
    let mut output = Vec::new();
    let mut starts = Vec::with_capacity(lines.len());
    for line in lines {
        starts.push(output.len() as u16);
        let line_style = line.style;
        if line.spans.is_empty() {
            output.push(Line::default().style(line_style));
            continue;
        }
        let mut current = Line::default();
        let mut column = 0;
        for span in line.spans {
            for character in span.content.chars() {
                if column == width {
                    output.push(current);
                    current = Line::default();
                    column = 0;
                }
                push_styled_char(&mut current, character, line_style.patch(span.style));
                column += 1;
            }
        }
        output.push(current);
    }
    (output, starts)
}

fn push_styled_char(line: &mut Line<'static>, character: char, style: Style) {
    if let Some(span) = line.spans.last_mut()
        && span.style == style
    {
        span.content.to_mut().push(character);
    } else {
        line.spans.push(Span::styled(character.to_string(), style));
    }
}

fn selection_bounds(selection: TextSelection) -> (TextPoint, TextPoint) {
    if (selection.start.row, selection.start.column) <= (selection.end.row, selection.end.column) {
        (selection.start, selection.end)
    } else {
        (selection.end, selection.start)
    }
}

fn highlight_selection(lines: &mut [Line<'static>], selection: TextSelection) {
    let (start, end) = selection_bounds(selection);
    for (row, line) in lines.iter_mut().enumerate() {
        let row = row as u16;
        if row < start.row || row > end.row {
            continue;
        }
        let from = if row == start.row { start.column } else { 0 } as usize;
        let to = if row == end.row {
            end.column as usize
        } else {
            usize::MAX
        };
        let spans = std::mem::take(&mut line.spans);
        let mut highlighted = Line::default();
        let mut column = 0;
        for span in spans {
            for character in span.content.chars() {
                let style = if column >= from && column <= to {
                    span.style.bg(Color::Blue).fg(Color::White)
                } else {
                    span.style
                };
                push_styled_char(&mut highlighted, character, style);
                column += 1;
            }
        }
        line.spans = highlighted.spans;
    }
}

fn selected_text(lines: &[Line<'_>], selection: TextSelection) -> String {
    let (start, end) = selection_bounds(selection);
    let mut selected = Vec::new();
    for row in start.row..=end.row.min(lines.len().saturating_sub(1) as u16) {
        let text = lines[row as usize]
            .spans
            .iter()
            .flat_map(|span| span.content.chars())
            .collect::<String>();
        let from = if row == start.row { start.column } else { 0 } as usize;
        let to = if row == end.row {
            end.column as usize
        } else {
            text.chars().count().saturating_sub(1)
        };
        selected.push(
            text.chars()
                .skip(from)
                .take(to.saturating_sub(from) + 1)
                .collect::<String>(),
        );
    }
    selected.join("\n")
}

fn chat_hit_test(state: &UiState, area: Rect, screen_row: u16) -> Option<usize> {
    let layout = chat_layout(state, area);
    let row = layout.offset + screen_row.saturating_sub(area.y);
    layout
        .headers
        .into_iter()
        .find_map(|(block, start, height)| (row >= start && row < start + height).then_some(block))
}

fn chat_diff_hit_test(
    state: &UiState,
    area: Rect,
    screen_column: u16,
    screen_row: u16,
) -> Option<String> {
    let layout = chat_layout(state, area);
    let row = layout.offset + screen_row.saturating_sub(area.y);
    let column = screen_column.saturating_sub(area.x + 1);
    let block =
        layout
            .diff_buttons
            .into_iter()
            .find_map(|(block, button_row, button_column)| {
                (row == button_row && column == button_column).then_some(block)
            })?;
    match &state.blocks[block] {
        ChatBlock::Tool {
            diff: Some(diff), ..
        } => Some(diff.clone()),
        _ => None,
    }
}

fn ensure_selected_visible(state: &mut UiState, area: Rect) {
    let Some(selected) = state.selected() else {
        return;
    };
    let layout = chat_layout(state, area);
    let Some((_, row, height)) = layout
        .headers
        .into_iter()
        .find(|(block, _, _)| *block == selected)
    else {
        return;
    };
    let end = layout.offset.saturating_add(area.height);
    if row < layout.offset {
        state.scroll = state.scroll.saturating_add(layout.offset - row);
    } else if row + height > end {
        state.scroll = state.scroll.saturating_sub(row + height - end);
    }
}

fn section_header(text: String, color: Color, selected: bool) -> Line<'static> {
    let mut style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    if selected {
        style = style.bg(Color::DarkGray).fg(Color::White);
    }
    Line::styled(text, style)
}

fn assistant_header(text: String, model: &str, color: Color, selected: bool) -> Line<'static> {
    if selected {
        return Line::styled(
            format!("{text}  {model}"),
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    }
    Line::from(vec![
        Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {model}"), Style::default().fg(Color::DarkGray)),
    ])
}

fn pad_line(mut line: Line<'static>) -> Line<'static> {
    line.spans.insert(0, Span::raw(" "));
    line
}

fn app_status(state: &UiState) -> (&'static str, Color) {
    if state.error.is_some() {
        ("error", Color::Red)
    } else if state.connecting {
        ("connecting", Color::Yellow)
    } else if state.waiting {
        ("waiting", Color::Yellow)
    } else if state.tool_running {
        ("tool", Color::Cyan)
    } else if state.generating {
        ("generating", Color::Green)
    } else {
        ("idle", Color::DarkGray)
    }
}

fn error_summary(error: &str) -> String {
    let mut characters = error.chars();
    let summary = characters.by_ref().take(100).collect::<String>();
    if characters.next().is_some() {
        format!("{summary}...")
    } else {
        summary
    }
}

fn size_label(chars: usize) -> String {
    if chars >= 1000 {
        format!("{:.1}k chars", chars as f32 / 1000.0)
    } else {
        format!("{chars} chars")
    }
}

fn format_elapsed(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    if millis < 60_000 {
        return format!("{:.1}s", millis as f64 / 1_000.0);
    }
    let seconds = duration.as_secs();
    if seconds < 3_600 {
        return format!("{}min {}s", seconds / 60, seconds % 60);
    }
    format!("{}h {}min", seconds / 3_600, seconds % 3_600 / 60)
}

fn elapsed_label(duration: Option<Duration>) -> String {
    duration
        .map(|duration| format!(" · {}", format_elapsed(duration)))
        .unwrap_or_default()
}

fn argument_summary(arguments: &str) -> String {
    let value = serde_json::from_str::<serde_json::Value>(arguments).ok();
    let summary = value
        .as_ref()
        .and_then(|value| {
            ["path", "command", "pattern", "query"]
                .into_iter()
                .find_map(|key| value.get(key).and_then(|value| value.as_str()))
        })
        .unwrap_or(arguments);
    let summary = summary.replace(['\n', '\r'], " ");
    let truncated = summary.chars().count() > 48;
    let mut summary = summary.chars().take(48).collect::<String>();
    if truncated {
        summary.push('…');
    }
    if summary.trim().is_empty() {
        String::new()
    } else {
        format!("({})", summary.trim())
    }
}

fn tool_status(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Streaming => "streaming",
        ToolStatus::Pending => "pending",
        ToolStatus::WaitingApproval => "approval",
        ToolStatus::Running => "running",
        ToolStatus::Done => "done",
        ToolStatus::Failed => "failed",
    }
}

fn tool_color(status: ToolStatus) -> Color {
    match status {
        ToolStatus::Done => Color::Green,
        ToolStatus::Failed => Color::Red,
        ToolStatus::Running | ToolStatus::Streaming => Color::Yellow,
        ToolStatus::Pending | ToolStatus::WaitingApproval => Color::DarkGray,
    }
}

fn markdown(content: &str) -> Vec<Line<'static>> {
    render_markdown(content, false)
}

fn markdown_preserving_breaks(content: &str) -> Vec<Line<'static>> {
    render_markdown(content, true)
}

fn render_markdown(content: &str, preserve_soft_breaks: bool) -> Vec<Line<'static>> {
    let options = Options::ENABLE_GFM
        | Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS;
    let mut renderer = MarkdownRenderer {
        preserve_soft_breaks,
        ..MarkdownRenderer::default()
    };
    for event in Parser::new_ext(content, options) {
        renderer.event(event);
    }
    renderer.finish()
}

#[derive(Default)]
struct MarkdownRenderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style: Style,
    styles: Vec<Style>,
    lists: Vec<Option<u64>>,
    quote_depth: usize,
    code_block: bool,
    table: Option<MarkdownTable>,
    preserve_soft_breaks: bool,
}

struct MarkdownTable {
    alignments: Vec<Alignment>,
    rows: Vec<TableRow>,
    cells: Vec<Vec<Span<'static>>>,
}

struct TableRow {
    cells: Vec<Vec<Span<'static>>>,
    header: bool,
}

impl MarkdownRenderer {
    fn event(&mut self, event: MarkdownEvent<'_>) {
        match event {
            MarkdownEvent::Start(tag) => self.start(tag),
            MarkdownEvent::End(tag) => self.end(tag),
            MarkdownEvent::Text(text) if self.code_block => self.code(&text),
            MarkdownEvent::Text(text) => self.text(&text),
            MarkdownEvent::Code(code) => self.span(
                code.into_string(),
                self.style.fg(Color::Green).add_modifier(Modifier::BOLD),
            ),
            MarkdownEvent::InlineMath(math) => {
                self.span(format!("${math}$"), self.style.fg(Color::Yellow))
            }
            MarkdownEvent::DisplayMath(math) => {
                self.line();
                self.span(format!("$$ {math} $$"), Style::default().fg(Color::Yellow));
                self.line();
            }
            MarkdownEvent::Html(html) | MarkdownEvent::InlineHtml(html) => {
                self.span(html.into_string(), self.style.fg(Color::DarkGray));
            }
            MarkdownEvent::FootnoteReference(label) => {
                self.span(format!("[^{label}]"), self.style.fg(Color::Blue));
            }
            MarkdownEvent::SoftBreak if self.preserve_soft_breaks => self.line(),
            MarkdownEvent::SoftBreak => self.text(" "),
            MarkdownEvent::HardBreak if self.table.is_some() => self.text(" "),
            MarkdownEvent::HardBreak => self.line(),
            MarkdownEvent::Rule => {
                self.line();
                self.lines.push(Line::styled(
                    "─".repeat(24),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            MarkdownEvent::TaskListMarker(checked) => {
                self.span(
                    if checked { "[x] " } else { "[ ] " }.into(),
                    self.style.fg(Color::Cyan),
                );
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.prefix(),
            Tag::Heading { .. } => {
                self.line();
                self.push_style(self.style.fg(Color::Magenta).add_modifier(Modifier::BOLD));
            }
            Tag::BlockQuote(_) => {
                self.line();
                self.quote_depth += 1;
            }
            Tag::CodeBlock(_) => {
                self.line();
                self.code_block = true;
            }
            Tag::List(start) => self.lists.push(start),
            Tag::Item => {
                self.line();
                self.prefix();
                let marker = match self.lists.last_mut() {
                    Some(Some(number)) => {
                        let marker = format!("{number}. ");
                        *number += 1;
                        marker
                    }
                    _ => "• ".into(),
                };
                self.span(marker, Style::default().fg(Color::Cyan));
            }
            Tag::Strong => self.push_style(self.style.add_modifier(Modifier::BOLD)),
            Tag::Emphasis => self.push_style(self.style.add_modifier(Modifier::ITALIC)),
            Tag::Strikethrough => self.push_style(self.style.add_modifier(Modifier::CROSSED_OUT)),
            Tag::Superscript | Tag::Subscript => {
                self.push_style(self.style.add_modifier(Modifier::ITALIC))
            }
            Tag::Link { .. } => self.push_style(
                self.style
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED),
            ),
            Tag::Image { .. } => {
                self.span("image: ".into(), self.style.fg(Color::DarkGray));
                self.push_style(self.style.add_modifier(Modifier::ITALIC));
            }
            Tag::FootnoteDefinition(label) => {
                self.line();
                self.span(format!("[^{label}]: "), Style::default().fg(Color::Blue));
            }
            Tag::DefinitionListTitle => self.push_style(self.style.add_modifier(Modifier::BOLD)),
            Tag::DefinitionListDefinition => self.span("  ".into(), self.style),
            Tag::Table(alignments) => {
                self.line();
                self.table = Some(MarkdownTable {
                    alignments,
                    rows: Vec::new(),
                    cells: Vec::new(),
                });
            }
            Tag::TableHead => self.push_style(self.style.add_modifier(Modifier::BOLD)),
            Tag::TableRow | Tag::TableCell => {}
            Tag::HtmlBlock | Tag::DefinitionList | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph
            | TagEnd::Item
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionListDefinition => self.line(),
            TagEnd::Heading(_) => {
                self.pop_style();
                self.line();
            }
            TagEnd::BlockQuote(_) => {
                self.line();
                self.quote_depth -= 1;
            }
            TagEnd::CodeBlock => self.code_block = false,
            TagEnd::List(_) => {
                self.line();
                self.lists.pop();
            }
            TagEnd::Strong
            | TagEnd::Emphasis
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::Link
            | TagEnd::Image
            | TagEnd::DefinitionListTitle => self.pop_style(),
            TagEnd::TableCell => {
                self.table
                    .as_mut()
                    .unwrap()
                    .cells
                    .push(std::mem::take(&mut self.spans));
            }
            TagEnd::TableHead => {
                self.pop_style();
                self.finish_table_row(true);
            }
            TagEnd::TableRow => self.finish_table_row(false),
            TagEnd::Table => self.render_table(),
            TagEnd::HtmlBlock | TagEnd::DefinitionList | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn text(&mut self, text: &str) {
        let mut lines = text.split('\n').peekable();
        while let Some(text) = lines.next() {
            if !text.is_empty() {
                self.prefix();
                self.span(text.to_owned(), self.style);
            }
            if lines.peek().is_some() {
                self.line();
            }
        }
    }

    fn code(&mut self, code: &str) {
        self.line();
        for line in code.lines() {
            self.lines.push(highlight_code(line));
        }
    }

    fn prefix(&mut self) {
        if self.spans.is_empty() && self.quote_depth > 0 {
            self.spans.push(Span::styled(
                "│ ".repeat(self.quote_depth),
                Style::default().fg(Color::DarkGray),
            ));
        }
    }

    fn span(&mut self, content: String, style: Style) {
        self.spans.push(Span::styled(content, style));
    }

    fn line(&mut self) {
        if !self.spans.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.spans)));
        }
    }

    fn push_style(&mut self, style: Style) {
        self.styles.push(self.style);
        self.style = style;
    }

    fn pop_style(&mut self) {
        self.style = self.styles.pop().unwrap();
    }

    fn finish_table_row(&mut self, header: bool) {
        let table = self.table.as_mut().unwrap();
        table.rows.push(TableRow {
            cells: std::mem::take(&mut table.cells),
            header,
        });
    }

    fn render_table(&mut self) {
        let table = self.table.take().unwrap();
        let columns = table
            .rows
            .iter()
            .map(|row| row.cells.len())
            .max()
            .unwrap_or(0);
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (column, cell) in row.cells.iter().enumerate() {
                widths[column] = widths[column].max(Line::from(cell.clone()).width());
            }
        }

        for row in table.rows {
            let mut spans = Vec::new();
            for (column, width) in widths.iter().copied().enumerate() {
                if column > 0 {
                    spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
                }
                let cell = row.cells.get(column).cloned().unwrap_or_default();
                let padding = width.saturating_sub(Line::from(cell.clone()).width());
                let (left, right) = match table
                    .alignments
                    .get(column)
                    .copied()
                    .unwrap_or(Alignment::None)
                {
                    Alignment::Right => (padding, 0),
                    Alignment::Center => (padding / 2, padding - padding / 2),
                    Alignment::None | Alignment::Left => (0, padding),
                };
                if left > 0 {
                    spans.push(Span::raw(" ".repeat(left)));
                }
                spans.extend(cell);
                if right > 0 {
                    spans.push(Span::raw(" ".repeat(right)));
                }
            }
            self.lines.push(Line::from(spans));
            if row.header && !widths.is_empty() {
                self.lines.push(Line::styled(
                    widths
                        .iter()
                        .map(|width| "─".repeat(*width))
                        .collect::<Vec<_>>()
                        .join("─┼─"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.line();
        self.lines
    }
}

fn highlight_code(line: &str) -> Line<'static> {
    const KEYWORDS: &[&str] = &[
        "fn", "let", "mut", "pub", "struct", "enum", "impl", "use", "mod", "match", "if", "else",
        "for", "while", "return", "async", "await", "const", "class", "def", "import", "from",
        "function", "var",
    ];
    if line.trim_start().starts_with("//") || line.trim_start().starts_with('#') {
        return Line::styled(line.to_owned(), Style::default().fg(Color::DarkGray));
    }
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut word = false;
    for character in line.chars().chain(std::iter::once('\0')) {
        let next_word = character.is_alphanumeric() || character == '_';
        if !current.is_empty() && next_word != word {
            let style = if KEYWORDS.contains(&current.as_str()) {
                Style::default().fg(Color::Magenta)
            } else if current.chars().all(|c| c.is_ascii_digit()) {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default().fg(Color::Cyan)
            };
            spans.push(Span::styled(std::mem::take(&mut current), style));
        }
        if character != '\0' {
            current.push(character);
            word = next_word;
        }
    }
    Line::from(spans)
}

struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            EnableBracketedPaste,
            EnableMouseCapture,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        ) {
            disable_raw_mode().ok();
            execute!(
                stdout,
                PopKeyboardEnhancementFlags,
                DisableBracketedPaste,
                DisableMouseCapture,
                LeaveAlternateScreen
            )
            .ok();
            return Err(error.into());
        }
        match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                disable_raw_mode().ok();
                execute!(
                    io::stdout(),
                    PopKeyboardEnhancementFlags,
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                )
                .ok();
                Err(error.into())
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        disable_raw_mode().ok();
        execute!(
            self.terminal.backend_mut(),
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        )
        .ok();
        self.terminal.show_cursor().ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_hides_fences_and_styles_code() {
        let lines = markdown("# Title\n```rust\nfn main() {}\n```");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].width(), 5);
    }

    #[test]
    fn markdown_renders_inline_emphasis() {
        let lines = markdown("plain **bold** and *italic*");
        assert!(lines[0].spans.iter().any(|span| {
            span.content == "bold" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
        assert!(lines[0].spans.iter().any(|span| {
            span.content == "italic" && span.style.add_modifier.contains(Modifier::ITALIC)
        }));
    }

    #[test]
    fn user_markdown_preserves_original_soft_breaks() {
        let lines = markdown_preserving_breaks("first line\nsecond line\nthird line");
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(text, ["first line", "second line", "third line"]);
        assert_eq!(markdown("first line\nsecond line").len(), 1);
    }

    #[test]
    fn markdown_renders_tables_as_rows_and_cells() {
        let lines = markdown("| Name | Value |\n| --- | ---: |\n| first | 42 |");
        let text = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(text, ["Name  │ Value", "──────┼──────", "first │    42"]);
        assert!(lines[0].spans.iter().any(|span| {
            span.content.as_ref() == "Name" && span.style.add_modifier.contains(Modifier::BOLD)
        }));
    }

    #[test]
    fn markdown_table_pipes_align_by_display_width() {
        let lines = markdown("| A | Longer |\n| :-: | --: |\n| wide | 7 |\n| x | 123 |");
        let rows = [&lines[0], &lines[2], &lines[3]];
        let pipes = rows
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .find('│')
            })
            .collect::<Vec<_>>();

        assert_eq!(pipes, [Some(5), Some(5), Some(5)]);
    }

    #[test]
    fn collapsed_section_header_is_mouse_hittable() {
        let mut state = UiState::new();
        state.apply(Event::ResponseStarted);
        state.apply(Event::ReasoningDelta("hidden details".into()));
        let area = Rect::new(0, 3, 80, 12);

        let layout = chat_layout(&state, area);
        assert!(layout.lines[0].spans[0].content.starts_with("▸ Thinking"));
        assert_eq!(chat_hit_test(&state, area, area.y), Some(0));
    }

    #[test]
    fn tool_diff_button_uses_the_persisted_call_diff() {
        let expected = "--- a/file.txt\n+++ b/file.txt\n-old\n+new\n";
        let mut state = UiState::new();
        state.apply(Event::History(vec![
            crate::runtime::Message::assistant(
                String::new(),
                "model".into(),
                String::new(),
                vec![crate::runtime::ToolCall {
                    id: "write-1".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({ "path": "file.txt" }),
                }],
            ),
            crate::runtime::Message::tool(
                "write-1".into(),
                "wrote file.txt".into(),
                None,
                Some(expected.into()),
            ),
        ]));
        let area = Rect::new(2, 3, 80, 12);
        let layout = chat_layout(&state, area);
        assert!(layout.lines[0].to_string().contains("[diff]"));
        assert!(layout.lines[0].to_string().contains("[diff] (file.txt)"));
        let (_, row, column) = layout.diff_buttons[0];
        let diff = chat_diff_hit_test(
            &state,
            area,
            area.x + 1 + column,
            area.y + row - layout.offset,
        )
        .unwrap();

        assert_eq!(diff, expected);
        state.open_fullscreen_tool_diff(diff);
        assert_eq!(state.fullscreen_diff(), expected);
    }

    #[test]
    fn update_plan_tool_calls_are_rendered_in_chat() {
        let mut state = UiState::new();
        state.apply(Event::ToolCallDelta {
            index: 0,
            name: Some("update_plan".into()),
            arguments: r#"{"plan":[]}"#.into(),
        });

        let layout = chat_layout(&state, Rect::new(0, 3, 80, 12));
        let text = layout.lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Tool: update_plan"));
    }

    #[tokio::test]
    async fn clicking_a_section_keeps_focus_in_the_composer() {
        let mut state = UiState::new();
        state.apply(Event::ResponseStarted);
        state.apply(Event::ReasoningDelta("details".into()));
        let area = Rect::new(0, 3, 80, 12);
        let input = Rect::new(0, 20, 80, 3);
        let (commands, _events) = mpsc::channel(1);
        let mouse = |kind| MouseEvent {
            kind,
            column: area.x + 1,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        };

        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left)),
            &mut state,
            MouseAreas {
                body: area,
                conversation: area,
                side: None,
                git: None,
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left)),
            &mut state,
            MouseAreas {
                body: area,
                conversation: area,
                side: None,
                git: None,
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();

        assert!(!state.conversation_focused());
        assert!(matches!(
            state.blocks[0],
            ChatBlock::Thinking { expanded: true, .. }
        ));
    }

    #[test]
    fn git_status_viewport_reports_and_clamps_position() {
        let mut state = UiState::new();
        state.project.git_files = (0..8)
            .map(|index| crate::project::GitFile {
                status: " M".into(),
                path: format!("file-{index}").into(),
            })
            .collect();

        state.git_status_scroll = 3;
        assert_eq!(git_status_max_scroll(&state, 5), 5);
        assert_eq!(git_status_title(&state, 5), " git status · 4-6/8 ");

        state.git_status_scroll = 20;
        assert_eq!(git_status_title(&state, 5), " git status · 6-8/8 ");
    }

    #[test]
    fn git_panel_diff_viewport_accounts_for_fixed_back_row() {
        let mut state = UiState::new();
        state.project.git_diff = (1..=8)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.project.git_diff_path = Some("src/main.rs".into());
        state.git_panel_diff_scroll = 3;

        assert_eq!(git_panel_diff_max_scroll(&state, 5), 6);
        assert_eq!(git_panel_diff_title(&state, 5), " 4-5/8 · src/main.rs ");
    }

    #[tokio::test]
    async fn git_status_mouse_scroll_preserves_file_hit_testing() {
        let mut state = UiState::new();
        state.project.git_available = true;
        state.project.git_files = (0..8)
            .map(|index| crate::project::GitFile {
                status: " M".into(),
                path: format!("file-{index}").into(),
            })
            .collect();
        let conversation = Rect::new(0, 3, 60, 5);
        let git = Rect::new(60, 3, 20, 5);
        let body = Rect::new(0, 3, 80, 5);
        let input = Rect::new(0, 8, 80, 3);
        let (commands, mut events) = mpsc::channel(1);
        let mouse = |kind, row| MouseEvent {
            kind,
            column: git.x + 1,
            row,
            modifiers: KeyModifiers::NONE,
        };

        handle_mouse(
            mouse(MouseEventKind::ScrollDown, git.y + 1),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(git),
                git: Some(git),
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        assert_eq!(state.git_status_scroll, 3);

        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), git.y + 1),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(git),
                git: Some(git),
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();

        assert!(state.git_diff_mode);
        match events.recv().await.unwrap() {
            Command::GitDiff(Some(path)) => assert_eq!(path, Path::new("file-3")),
            _ => panic!("expected a file diff command"),
        }

        state.project.git_diff = (1..=8)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        handle_mouse(
            mouse(MouseEventKind::ScrollDown, git.y + 2),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(git),
                git: Some(git),
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        assert_eq!(state.git_panel_diff_scroll, 3);

        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), git.y + 1),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(git),
                git: Some(git),
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        assert!(!state.git_diff_mode);
        assert_eq!(state.git_panel_diff_scroll, 0);
        assert!(matches!(
            events.recv().await.unwrap(),
            Command::RefreshProject
        ));
    }

    #[tokio::test]
    async fn dragging_git_splitter_changes_panel_width() {
        let mut state = UiState::new();
        state.project.git_available = true;
        let body = Rect::new(0, 3, 120, 20);
        let (conversation, git) = git_split(body, &state);
        let git = git.unwrap();
        let input = Rect::new(0, 23, 120, 3);
        let (commands, _events) = mpsc::channel(1);
        let mouse = |kind, column| MouseEvent {
            kind,
            column,
            row: body.y + 2,
            modifiers: KeyModifiers::NONE,
        };

        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), git.x),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(git),
                git: Some(git),
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        assert!(state.git_split_dragging);

        handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), 70),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(git),
                git: Some(git),
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        assert_eq!(state.git_panel_width, Some(50));

        handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), 70),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(git),
                git: Some(git),
                plan: None,
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        assert!(!state.git_split_dragging);
        let (conversation, git) = git_split(body, &state);
        assert_eq!(conversation.width, 70);
        assert_eq!(git.unwrap().width, 50);
    }

    #[tokio::test]
    async fn plan_pane_defaults_to_half_height_and_has_a_draggable_split() {
        let mut state = UiState::new();
        state.plan_panel = true;
        let body = Rect::new(0, 3, 120, 20);
        let (conversation, side) = git_split(body, &state);
        let side = side.unwrap();
        let (git, plan) = side_split(side, &state);
        let plan = plan.unwrap();
        assert_eq!(git.height, 10);
        assert_eq!(plan.height, 10);
        let input = Rect::new(0, 23, 120, 3);
        let (commands, _events) = mpsc::channel(1);
        let mouse = |kind, row| MouseEvent {
            kind,
            column: side.x + 2,
            row,
            modifiers: KeyModifiers::NONE,
        };

        handle_mouse(
            mouse(MouseEventKind::Down(MouseButton::Left), plan.y),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(side),
                git: Some(git),
                plan: Some(plan),
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        assert!(state.plan_split_dragging);

        handle_mouse(
            mouse(MouseEventKind::Drag(MouseButton::Left), side.y + 6),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(side),
                git: Some(git),
                plan: Some(plan),
                input,
            },
            &commands,
        )
        .await
        .unwrap();
        handle_mouse(
            mouse(MouseEventKind::Up(MouseButton::Left), side.y + 6),
            &mut state,
            MouseAreas {
                body,
                conversation,
                side: Some(side),
                git: Some(git),
                plan: Some(plan),
                input,
            },
            &commands,
        )
        .await
        .unwrap();

        assert!(!state.plan_split_dragging);
        assert_eq!(state.plan_panel_height, Some(14));
        let (git, plan) = side_split(side, &state);
        assert_eq!(git.height, 6);
        assert_eq!(plan.unwrap().height, 14);
    }

    #[test]
    fn status_uses_phase_colors_and_error_priority() {
        let mut state = UiState::new();
        assert_eq!(app_status(&state), ("idle", Color::DarkGray));

        state.apply(Event::GenerationStarted);
        assert_eq!(app_status(&state), ("connecting", Color::Yellow));

        state.apply(Event::ResponseHeadersReceived);
        assert_eq!(app_status(&state), ("waiting", Color::Yellow));

        state.apply(Event::ResponseStarted);
        assert_eq!(app_status(&state), ("generating", Color::Green));

        state.apply(Event::ToolStarted {
            call_id: "call-1".into(),
        });
        assert_eq!(app_status(&state), ("tool", Color::Cyan));

        state.apply(Event::ToolResult {
            call_id: "call-1".into(),
            output: "done".into(),
            success: true,
            diff: None,
        });
        assert_eq!(app_status(&state), ("generating", Color::Green));

        state.apply(Event::Error("failed".into()));
        assert_eq!(app_status(&state), ("error", Color::Red));
    }

    #[test]
    fn streaming_preserves_a_scrolled_chat_viewport() {
        let mut state = UiState::new();
        let area = Rect::new(0, 0, 24, 6);
        state.apply(Event::ResponseStarted);
        state.apply(Event::TextDelta("old content ".repeat(30)));
        state.scroll = 4;
        let before = chat_layout(&state, area);
        let old_scroll = state.scroll;

        apply_runtime_event(
            &mut state,
            Event::TextDelta("new streamed content ".repeat(30)),
            area,
            before.lines.len(),
        );
        let after = chat_layout(&state, area);

        assert_eq!(after.offset, before.offset);
        assert!(state.scroll > old_scroll);
    }

    #[test]
    fn collapsing_content_clamps_chat_scroll_immediately() {
        let mut state = UiState::new();
        let area = Rect::new(0, 0, 24, 6);
        state.apply(Event::ResponseStarted);
        state.apply(Event::TextDelta("old content ".repeat(60)));
        state.scroll = chat_max_scroll(&state, area);
        assert!(state.scroll > 0);

        state.toggle(0);
        clamp_chat_scroll(&mut state, area);

        assert_eq!(state.scroll, chat_max_scroll(&state, area));
        assert_eq!(state.scroll, 0);
        scroll_chat_down(&mut state, 3);
        assert_eq!(state.scroll, 0);
    }

    #[test]
    fn fullscreen_diff_keeps_streaming_chat_viewport_state() {
        use ratatui::backend::TestBackend;

        let page = Rect::new(0, 0, 40, 20);
        let mut state = UiState::new();
        state.apply(Event::ResponseStarted);
        state.apply(Event::TextDelta("old content ".repeat(80)));
        let chat = conversation_viewport_area(page, &state);
        state.scroll = 4;
        let before = chat_layout(&state, chat);
        state.open_fullscreen_tool_diff("--- a/file\n+++ b/file\n-old\n+new\n".into());

        let mut terminal = Terminal::new(TestBackend::new(page.width, page.height)).unwrap();
        let mut hidden_chat_height = 0;
        terminal
            .draw(|frame| {
                hidden_chat_height = draw(frame, &Config::default(), &state, before.lines.len())
            })
            .unwrap();
        assert_eq!(hidden_chat_height, before.lines.len());

        apply_runtime_event(
            &mut state,
            Event::TextDelta("new streamed content ".repeat(40)),
            chat,
            hidden_chat_height,
        );
        let after = chat_layout(&state, chat);
        assert_eq!(after.offset, before.offset);

        state.close_fullscreen_git_diff();
        clamp_chat_scroll(&mut state, chat);
        assert!(!chat_layout(&state, chat).lines.is_empty());
        assert!(state.scroll <= chat_max_scroll(&state, chat));
    }

    #[test]
    fn escape_closes_diff_without_changing_active_generation() {
        let mut state = UiState::new();
        state.generating = true;
        state.open_fullscreen_tool_diff("diff".into());

        assert!(handle_fullscreen_diff_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut state,
            20,
        ));
        assert!(!state.git_fullscreen_diff);
        assert!(state.generating);
    }

    #[test]
    fn streaming_follows_chat_at_the_bottom() {
        let mut state = UiState::new();
        let area = Rect::new(0, 0, 24, 6);
        state.apply(Event::ResponseStarted);
        state.apply(Event::TextDelta("old content ".repeat(30)));
        let before = chat_layout(&state, area);

        apply_runtime_event(
            &mut state,
            Event::TextDelta("new streamed content ".repeat(30)),
            area,
            before.lines.len(),
        );
        let after = chat_layout(&state, area);

        assert_eq!(state.scroll, 0);
        assert!(after.offset > before.offset);
    }

    #[test]
    fn long_errors_are_collapsed_without_losing_unicode_boundaries() {
        let error = "é".repeat(101);
        assert_eq!(error_summary(&error), format!("{}...", "é".repeat(100)));
        assert_eq!(error_summary(&"é".repeat(100)), "é".repeat(100));
    }

    #[test]
    fn elapsed_time_uses_compact_units() {
        assert_eq!(format_elapsed(Duration::from_millis(999)), "999ms");
        assert_eq!(format_elapsed(Duration::from_millis(1_500)), "1.5s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1min 5s");
        assert_eq!(format_elapsed(Duration::from_secs(3_720)), "1h 2min");
    }

    #[test]
    fn message_content_is_padded_but_its_header_is_not() {
        let mut state = UiState::new();
        state.push_user("hello".into());
        let layout = chat_layout(&state, Rect::new(0, 0, 80, 10));
        let text = layout
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(text[0], "▾ You");
        assert_eq!(text[1], " hello");
    }

    #[test]
    fn user_image_attachments_are_visible_in_chat_history() {
        let mut state = UiState::new();
        state.push_user_with_images(
            String::new(),
            vec![ImageContent {
                mime_type: "image/png".into(),
                data: String::new(),
                path: Some("attachments/image.png".into()),
                width: 640,
                height: 480,
            }],
        );
        let layout = chat_layout(&state, Rect::new(0, 0, 80, 10));
        let text = layout
            .lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert!(text.contains("Image · 640×480"));
    }

    #[test]
    fn wrapping_preserves_section_header_colors_and_modifiers() {
        let style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let (lines, _) = wrap_chat_lines(vec![Line::styled("▾ You", style)], 80);

        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn fullscreen_diff_scroll_stops_at_the_last_visible_line() {
        let mut state = UiState::new();
        state.project.git_available = true;

        assert_eq!(git_diff_max_scroll(&state, 10), 0);

        state.project.git_diff = (0..10)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(git_diff_max_scroll(&state, 5), 7);
    }

    #[test]
    fn command_palette_filters_by_name_and_title() {
        let by_name = palette_commands("/rea").unwrap();
        assert_eq!(by_name.len(), 1);
        assert_eq!(by_name[0].name, "/reason");

        let by_title = palette_commands("/visibility").unwrap();
        assert_eq!(
            by_title
                .iter()
                .map(|command| command.name)
                .collect::<Vec<_>>(),
            ["/thinking", "/tools"]
        );
        assert!(palette_commands("/add ").is_none());
    }

    #[test]
    fn shift_insert_is_recognized_as_direct_clipboard_paste() {
        assert!(is_shift_insert(KeyEvent::new(
            KeyCode::Insert,
            KeyModifiers::SHIFT
        )));
        assert!(!is_shift_insert(KeyEvent::new(
            KeyCode::Insert,
            KeyModifiers::NONE
        )));
    }

    #[tokio::test]
    async fn image_processing_does_not_block_composer_updates() {
        let mut config = Config::default();
        config.models[0].vision = true;
        let mut state = UiState::new();
        state.model = config.models[0].name.clone();
        let (loads, mut loaded) = mpsc::unbounded_channel();
        let (release, wait) = std::sync::mpsc::channel();

        queue_input_load(
            &mut state,
            &loads,
            "Processing image",
            "test image",
            move || {
                wait.recv().unwrap();
                Ok(LoadedInput::Image(ImageContent {
                    mime_type: "image/png".into(),
                    data: "aW1hZ2U=".into(),
                    path: None,
                    width: 10,
                    height: 20,
                }))
            },
        );

        assert!(state.image_loading());
        state.insert_char('x');
        release.send(()).unwrap();
        let load = tokio::time::timeout(Duration::from_secs(1), loaded.recv())
            .await
            .unwrap()
            .unwrap();
        apply_input_load(load, &config, &mut state);

        assert!(!state.image_loading());
        assert_eq!(state.take_input().unwrap().content, "x");
    }

    #[test]
    fn conversation_selection_copies_across_wrapped_rows() {
        let (mut lines, _) = wrap_chat_lines(vec![Line::raw("abcdef"), Line::raw("ghij")], 80);
        let selection = TextSelection {
            start: TextPoint { row: 0, column: 2 },
            end: TextPoint { row: 1, column: 1 },
        };

        assert_eq!(selected_text(&lines, selection), "cdef\ngh");
        highlight_selection(&mut lines, selection);
        assert_eq!(lines[0].spans.last().unwrap().style.bg, Some(Color::Blue));
        assert_eq!(lines[1].spans[0].style.bg, Some(Color::Blue));
    }
}
