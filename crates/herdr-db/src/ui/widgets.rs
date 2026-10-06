//! Shared pieces: environment banner, status line, popups, text input.

use super::theme::{self, env_color};
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use herdr_db_core::config::SourceConfig;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

/// Connection state shown in banners.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Link {
    #[default]
    Idle,
    Tunnel,
    Connecting,
    Connected,
    Lost,
}

impl Link {
    pub fn label(&self) -> &'static str {
        match self {
            Link::Idle => "",
            Link::Tunnel => "tunnel…",
            Link::Connecting => "connexion…",
            Link::Connected => "connecté",
            Link::Lost => "déconnecté",
        }
    }
}

/// Banner at the top of every pane opened on a source: source name and
/// environment, always visible (never hidden in production).
pub fn banner(frame: &mut Frame, area: Rect, source: &SourceConfig, context: &str, write_mode: bool, link: &Link) {
    let color = env_color(source.environment);
    let mut style = Style::new().bg(color).fg(Color::Black).add_modifier(Modifier::BOLD);
    let access = if write_mode {
        style = Style::new().bg(Color::Red).fg(Color::White).add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK);
        "ÉCRITURE ACTIVE"
    } else if source.read_only {
        "lecture seule"
    } else {
        "lecture/écriture"
    };
    let mut text = format!(" {} · {} · {}", source.label(), source.environment, access);
    if !context.is_empty() {
        text.push_str(" · ");
        text.push_str(context);
    }
    let link_label = link.label();
    let right = if link_label.is_empty() { String::new() } else { format!(" {link_label} ") };
    let width = area.width as usize;
    // Narrow pane: the context gives way, never the source and environment.
    let room = width.saturating_sub(right.width());
    if text.width() > room {
        text = fit_width(&text, room);
    }
    let used = text.width() + right.width();
    if used < width {
        text.push_str(&" ".repeat(width - used));
    }
    if text.width() + right.width() <= width {
        text.push_str(&right);
    }
    frame.render_widget(Paragraph::new(Line::styled(text, style)), area);
}

/// Cuts `text` to `width` columns, ending with an ellipsis when cut.
pub fn fit_width(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) + 1 > width {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Severity {
    #[default]
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Status {
    pub text: String,
    pub severity: Severity,
}

impl Status {
    pub fn info(text: impl Into<String>) -> Status {
        Status { text: text.into(), severity: Severity::Info }
    }
    pub fn success(text: impl Into<String>) -> Status {
        Status { text: text.into(), severity: Severity::Success }
    }
    pub fn warning(text: impl Into<String>) -> Status {
        Status { text: text.into(), severity: Severity::Warning }
    }
    pub fn error(text: impl Into<String>) -> Status {
        Status { text: text.into(), severity: Severity::Error }
    }

    pub fn style(&self) -> Style {
        match self.severity {
            Severity::Info => theme::dim(),
            Severity::Success => Style::new().fg(Color::Green),
            Severity::Warning => Style::new().fg(Color::Yellow),
            Severity::Error => theme::error(),
        }
    }
}

pub fn status_line(frame: &mut Frame, area: Rect, status: &Status, hint: &str) {
    let mut spans = vec![Span::styled(status.text.clone(), status.style())];
    let used = status.text.width();
    let width = area.width as usize;
    if !hint.is_empty() && used + hint.width() + 2 <= width {
        spans.push(Span::raw(" ".repeat(width - used - hint.width())));
        spans.push(Span::styled(hint.to_string(), theme::dim()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(10.min(area.width));
    let height = height.min(area.height.saturating_sub(2)).max(3.min(area.height));
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// Clears `area` and draws a bordered block; returns the inner area.
pub fn popup(frame: &mut Frame, area: Rect, title: &str) -> Rect {
    frame.render_widget(Clear, area);
    let block = Block::bordered().border_type(BorderType::Rounded).title(Line::from(format!(" {title} ")).bold());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

pub fn message_popup(frame: &mut Frame, title: &str, lines: Vec<Line<'static>>) {
    let area = frame.area();
    let width = lines.iter().map(|l| l.width() as u16).max().unwrap_or(20).max(title.len() as u16 + 4) + 4;
    let rect = centered(area, width.min(area.width.saturating_sub(4)), lines.len() as u16 + 2);
    let inner = popup(frame, rect, title);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// Help popup listing `(key, description)` pairs.
pub fn help_popup(frame: &mut Frame, title: &str, entries: &[(String, &str)]) {
    let key_width = entries.iter().map(|(k, _)| k.width()).max().unwrap_or(4);
    let lines: Vec<Line<'static>> = entries
        .iter()
        .map(|(key, text)| {
            Line::from(vec![
                Span::styled(format!(" {key:<key_width$}  "), Style::new().fg(Color::Cyan)),
                Span::raw(text.to_string()),
            ])
        })
        .collect();
    message_popup(frame, title, lines);
}

/// "il y a 3 min", from the introspection time.
pub fn age(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = (now - at).num_seconds().max(0);
    match seconds {
        0..=59 => "à l'instant".to_string(),
        60..=3599 => format!("il y a {} min", seconds / 60),
        3600..=86_399 => format!("il y a {} h", seconds / 3600),
        _ => format!("il y a {} j", seconds / 86_400),
    }
}

/// Single-line text input.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextInput {
    pub text: String,
    /// Cursor position in characters.
    pub cursor: usize,
    pub masked: bool,
}

pub enum InputOutcome {
    Changed,
    Submit,
    Cancel,
    Ignored,
}

impl TextInput {
    pub fn new(text: impl Into<String>) -> TextInput {
        let text = text.into();
        TextInput { cursor: text.chars().count(), text, masked: false }
    }

    pub fn masked() -> TextInput {
        TextInput { masked: true, ..TextInput::default() }
    }

    fn byte_index(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map_or(self.text.len(), |(i, _)| i)
    }

    pub fn insert_str(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        let at = self.byte_index(self.cursor);
        self.text.insert_str(at, &s);
        self.cursor += s.chars().count();
    }

    pub fn handle(&mut self, key: &KeyEvent) -> InputOutcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Enter => InputOutcome::Submit,
            KeyCode::Esc => InputOutcome::Cancel,
            KeyCode::Char('c') if ctrl => InputOutcome::Cancel,
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                InputOutcome::Changed
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.text.chars().count();
                InputOutcome::Changed
            }
            KeyCode::Char('u') if ctrl => {
                let at = self.byte_index(self.cursor);
                self.text.drain(..at);
                self.cursor = 0;
                InputOutcome::Changed
            }
            KeyCode::Char('w') if ctrl => {
                let chars: Vec<char> = self.text.chars().collect();
                let mut start = self.cursor;
                while start > 0 && chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && chars[start - 1] != ' ' {
                    start -= 1;
                }
                let (a, b) = (self.byte_index(start), self.byte_index(self.cursor));
                self.text.drain(a..b);
                self.cursor = start;
                InputOutcome::Changed
            }
            KeyCode::Char(c) if !ctrl => {
                self.insert_str(&c.to_string());
                InputOutcome::Changed
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let at = self.byte_index(self.cursor - 1);
                self.text.remove(at);
                self.cursor -= 1;
                InputOutcome::Changed
            }
            KeyCode::Delete if self.cursor < self.text.chars().count() => {
                let at = self.byte_index(self.cursor);
                self.text.remove(at);
                InputOutcome::Changed
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                InputOutcome::Changed
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.text.chars().count());
                InputOutcome::Changed
            }
            KeyCode::Home => {
                self.cursor = 0;
                InputOutcome::Changed
            }
            KeyCode::End => {
                self.cursor = self.text.chars().count();
                InputOutcome::Changed
            }
            _ => InputOutcome::Ignored,
        }
    }

    pub fn display(&self) -> String {
        if self.masked { "•".repeat(self.text.chars().count()) } else { self.text.clone() }
    }

    /// Renders `prefix` + text, scrolled so the cursor stays visible, and
    /// places the terminal cursor.
    pub fn render(&self, frame: &mut Frame, area: Rect, prefix: &str, style: Style, focused: bool) {
        let display = self.display();
        let prefix_width = prefix.width() as u16;
        let available = area.width.saturating_sub(prefix_width + 1) as usize;
        let before: String = display.chars().take(self.cursor).collect();
        let cursor_col = before.width();
        let skip = cursor_col.saturating_sub(available);
        let mut visible = String::new();
        let mut col = 0;
        for c in display.chars() {
            let w = UnicodeWidthStr::width(c.to_string().as_str());
            if col >= skip && col + w <= skip + available {
                visible.push(c);
            }
            col += w;
        }
        let line = Line::from(vec![Span::styled(prefix.to_string(), theme::dim()), Span::styled(visible, style)]);
        frame.render_widget(Paragraph::new(line), area);
        if focused {
            frame.set_cursor_position(Position::new(area.x + prefix_width + (cursor_col - skip) as u16, area.y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_input_editing() {
        let mut input = TextInput::new("héllo");
        input.handle(&KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        input.handle(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        assert_eq!(input.text, "hélo");
        input.handle(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(input.text, "hélxo");
        input.handle(&KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(input.text, "o");
        assert!(matches!(input.handle(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)), InputOutcome::Submit));
    }

    #[test]
    fn narrow_banner_keeps_the_link_state() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let source = herdr_db_core::config::load(
            Some((
                std::path::Path::new("t.toml"),
                "[[sources]]\nid = \"local\"\nengine = \"postgres\"\ndatabase = \"d\"\n",
            )),
            None,
        )
        .unwrap()
        .sources
        .remove(0);
        let mut terminal = Terminal::new(TestBackend::new(48, 1)).unwrap();
        terminal.draw(|f| banner(f, f.area(), &source, "console · herdr_fixture", false, &Link::Connected)).unwrap();
        let line: String = terminal.backend().buffer().content().iter().map(|c| c.symbol()).collect();
        assert_eq!(line, " local · local · lecture/écriture · c… connecté ");
    }

    #[test]
    fn ages() {
        let now = Utc::now();
        assert_eq!(age(now, now), "à l'instant");
        assert_eq!(age(now - chrono::Duration::minutes(5), now), "il y a 5 min");
        assert_eq!(age(now - chrono::Duration::hours(3), now), "il y a 3 h");
        assert_eq!(age(now - chrono::Duration::days(2), now), "il y a 2 j");
    }
}
