use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use serde_json::Value;

struct Node {
    text: String,
    depth: usize,
    parent: Option<usize>,
    branch: bool,
    open: bool,
}

pub struct RawView {
    pub block_id: String,
    nodes: Vec<Node>,
    query: String,
    selected: usize,
    top: usize,
    status: String,
    hits: Vec<usize>,
    follow: bool,
}

impl RawView {
    pub fn new(block_id: String) -> Self {
        Self {
            block_id,
            nodes: Vec::new(),
            query: String::new(),
            selected: 0,
            top: 0,
            status: "Loading request…".into(),
            hits: Vec::new(),
            follow: false,
        }
    }

    pub fn load(&mut self, result: Result<Value, String>) {
        match result {
            Ok(value) => {
                self.status.clear();
                self.add(None, &value, 0, None);
            }
            Err(error) => self.status = error,
        }
    }

    fn add(&mut self, key: Option<&str>, value: &Value, depth: usize, parent: Option<usize>) {
        let prefix = key
            .map(|k| format!("{}: ", serde_json::to_string(k).unwrap()))
            .unwrap_or_default();
        let branch = value.is_object() || value.is_array();
        let text = match value {
            Value::Array(items) => format!("{prefix}[{} items]", items.len()),
            Value::Object(fields) => format!("{prefix}{{{} keys}}", fields.len()),
            _ => format!("{prefix}{value}"),
        };
        let index = self.nodes.len();
        self.nodes.push(Node {
            text,
            depth,
            parent,
            branch,
            open: depth < 2,
        });
        match value {
            Value::Object(fields) => {
                for (key, value) in fields {
                    self.add(Some(key), value, depth + 1, Some(index));
                }
            }
            Value::Array(items) => {
                for (i, value) in items.iter().enumerate() {
                    self.add(Some(&i.to_string()), value, depth + 1, Some(index));
                }
            }
            _ => {}
        }
    }

    fn visible(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let mut visible = vec![false; self.nodes.len()];
        for (i, node) in self.nodes.iter().enumerate() {
            if query.is_empty() {
                visible[i] = node.parent.is_none_or(|p| visible[p] && self.nodes[p].open);
            } else if node.text.to_lowercase().contains(&query) {
                visible[i] = true;
                let mut parent = node.parent;
                while let Some(p) = parent {
                    visible[p] = true;
                    parent = self.nodes[p].parent;
                }
            }
        }
        visible
            .iter()
            .enumerate()
            .filter_map(|(i, show)| show.then_some(i))
            .collect()
    }

    pub fn key(&mut self, key: KeyEvent) {
        let visible = self.visible();
        self.follow = matches!(
            key.code,
            KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End
        );
        match key.code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(visible.len().saturating_sub(1))
            }
            KeyCode::PageUp => self.top = self.top.saturating_sub(10),
            KeyCode::PageDown => self.top += 10,
            KeyCode::Home => {
                self.selected = 0;
                self.top = 0;
            }
            KeyCode::End => self.selected = visible.len().saturating_sub(1),
            KeyCode::Enter | KeyCode::Left | KeyCode::Right => {
                if let Some(&index) = visible.get(self.selected) {
                    let node = &mut self.nodes[index];
                    node.open = match key.code {
                        KeyCode::Left => false,
                        KeyCode::Right => true,
                        _ => !node.open,
                    };
                }
            }
            KeyCode::Char('+') if self.query.is_empty() => {
                for node in &mut self.nodes {
                    node.open = true;
                }
            }
            KeyCode::Char('-') if self.query.is_empty() => {
                for node in &mut self.nodes {
                    node.open = false;
                }
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.top = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.selected = 0;
                self.top = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.push(c);
                self.selected = 0;
                self.top = 0;
            }
            _ => {}
        }
    }

    pub fn mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.top = self.top.saturating_sub(3),
            MouseEventKind::ScrollDown => self.top += 3,
            MouseEventKind::Down(MouseButton::Left) if mouse.row >= 3 => {
                if let Some(&index) = self.hits.get(mouse.row.saturating_sub(3) as usize) {
                    self.nodes[index].open = !self.nodes[index].open;
                    self.selected = self.visible().iter().position(|&i| i == index).unwrap_or(0);
                }
            }
            _ => {}
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title(" Raw model request · Esc close "),
            area,
        );
        frame.render_widget(
            Paragraph::new(format!("Search: {}", self.query)),
            Rect::new(1, 1, area.width.saturating_sub(2), 1),
        );
        frame.render_widget(
            Paragraph::new("↑↓ select · ←→ fold · +/- all · type to search · Ctrl+U clear"),
            Rect::new(1, 2, area.width.saturating_sub(2), 1),
        );
        let width = area.width.saturating_sub(2).max(1) as usize;
        let height = area.height.saturating_sub(4) as usize;
        let visible = self.visible();
        self.selected = self.selected.min(visible.len().saturating_sub(1));
        let selected = visible.get(self.selected).copied();
        let mut lines = Vec::new();
        let mut hits = Vec::new();
        for index in visible {
            let node = &self.nodes[index];
            let indent = " ".repeat((node.depth * 2).min(width / 3));
            let marker = if node.branch {
                if node.open { "▾ " } else { "▸ " }
            } else {
                "  "
            };
            let text = format!("{indent}{marker}{}", node.text);
            let style = if selected == Some(index) {
                Style::default().bg(Color::DarkGray)
            } else if !self.query.is_empty()
                && node
                    .text
                    .to_lowercase()
                    .contains(&self.query.to_lowercase())
            {
                Style::default().fg(Color::Yellow)
            } else {
                Style::default()
            };
            let mut chunk = String::new();
            let mut cells = 0;
            for character in text.chars() {
                let size = Span::raw(character.to_string()).width();
                if cells + size > width && !chunk.is_empty() {
                    lines.push(Line::styled(std::mem::take(&mut chunk), style));
                    hits.push(index);
                    cells = 0;
                }
                chunk.push(character);
                cells += size;
            }
            lines.push(Line::styled(chunk, style));
            hits.push(index);
        }
        if !self.status.is_empty() {
            lines.push(Line::raw(self.status.clone()));
        } else if lines.is_empty() {
            lines.push(Line::raw("No matching keys or values"));
        }
        if self.follow
            && let Some(row) = hits.iter().position(|&i| Some(i) == selected)
        {
            if row < self.top {
                self.top = row;
            } else if row >= self.top + height {
                self.top = row.saturating_sub(height.saturating_sub(1));
            }
        }
        self.follow = false;
        self.top = self.top.min(lines.len().saturating_sub(height));
        self.hits = hits.into_iter().skip(self.top).take(height).collect();
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(self.top)
                    .take(height)
                    .collect::<Vec<_>>(),
            ),
            Rect::new(1, 3, width as u16, height as u16),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{runtime::Event, ui::state::UiState};
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;
    use std::cell::RefCell;

    #[test]
    fn search_reveals_folded_values_and_navigation_scrolls() {
        let mut view = RawView::new("1".into());
        view.load(Ok(
            json!({"input": [{"content":"Needle"}], "tools": (0..40).collect::<Vec<_>>() }),
        ));
        view.key(KeyEvent::new(KeyCode::Char('-'), KeyModifiers::NONE));
        assert_eq!(view.visible(), vec![0]);
        for c in "needle".chars() {
            view.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        assert!(
            view.visible()
                .iter()
                .any(|&i| view.nodes[i].text.contains("Needle"))
        );
        view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(view.visible(), vec![0]);
        view.key(KeyEvent::new(KeyCode::Char('+'), KeyModifiers::NONE));
        view.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(view.top > 0);
        assert!(view.hits.contains(&(view.nodes.len() - 1)));
        view.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(view.top, 0);
    }
    #[test]
    fn tui_loads_only_the_open_request_and_ignores_late_replies() {
        let mut state = UiState::new();
        state.session_id = "session".into();
        state.raw_view = Some(RefCell::new(RawView::new("block".into())));
        state.apply(Event::RawData {
            session_id: "other".into(),
            block_id: "block".into(),
            result: Ok(serde_json::json!({"wrong":true})),
        });
        assert!(state.raw_view.as_ref().unwrap().borrow().nodes.is_empty());
        state.apply(Event::RawData {
            session_id: "session".into(),
            block_id: "block".into(),
            result: Ok(serde_json::json!({"correct":true})),
        });
        assert!(!state.raw_view.as_ref().unwrap().borrow().nodes.is_empty());
        state.raw_view = None;
        state.apply(Event::RawData {
            session_id: "session".into(),
            block_id: "block".into(),
            result: Ok(Value::Null),
        });
        assert!(state.raw_view.is_none());
    }
}
