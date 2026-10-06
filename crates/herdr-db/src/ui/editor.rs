//! Multi-line text editor for the console (no completion in v0.1).

use super::highlight::highlight;
use super::theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Debug, Clone)]
pub struct Editor {
    lines: Vec<String>,
    /// Cursor line and column (in characters).
    pub row: usize,
    pub col: usize,
    scroll: usize,
    hscroll: usize,
}

impl Default for Editor {
    fn default() -> Self {
        Editor { lines: vec![String::new()], row: 0, col: 0, scroll: 0, hscroll: 0 }
    }
}

fn byte_at(line: &str, chars: usize) -> usize {
    line.char_indices().nth(chars).map_or(line.len(), |(i, _)| i)
}

impl Editor {
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_text(&mut self, text: &str) {
        self.lines = text.split('\n').map(str::to_string).collect();
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
    }

    pub fn is_blank(&self) -> bool {
        self.lines.iter().all(|l| l.trim().is_empty())
    }

    /// Cursor as a byte offset into [`Editor::text`].
    pub fn cursor_offset(&self) -> usize {
        self.lines[..self.row].iter().map(|l| l.len() + 1).sum::<usize>() + byte_at(&self.lines[self.row], self.col)
    }

    fn line_len(&self) -> usize {
        self.lines[self.row].chars().count()
    }

    pub fn insert_str(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n").replace('\t', "    ");
        let mut parts = text.split('\n');
        let first = parts.next().unwrap_or("");
        let at = byte_at(&self.lines[self.row], self.col);
        let tail = self.lines[self.row].split_off(at);
        self.lines[self.row].push_str(first);
        self.col += first.chars().count();
        for part in parts {
            self.row += 1;
            self.lines.insert(self.row, part.to_string());
            self.col = part.chars().count();
        }
        self.lines[self.row].push_str(&tail);
    }

    fn newline(&mut self) {
        let indent: String = self.lines[self.row].chars().take_while(|c| *c == ' ').collect();
        let at = byte_at(&self.lines[self.row], self.col);
        let tail = self.lines[self.row].split_off(at);
        self.row += 1;
        self.lines.insert(self.row, format!("{indent}{tail}"));
        self.col = indent.chars().count();
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let at = byte_at(&self.lines[self.row], self.col - 1);
            self.lines[self.row].remove(at);
            self.col -= 1;
        } else if self.row > 0 {
            let line = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&line);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len() {
            let at = byte_at(&self.lines[self.row], self.col);
            self.lines[self.row].remove(at);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn word_left(&mut self) {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        while self.col > 0 && !chars[self.col - 1].is_alphanumeric() {
            self.col -= 1;
        }
        while self.col > 0 && chars[self.col - 1].is_alphanumeric() {
            self.col -= 1;
        }
    }

    fn word_right(&mut self) {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        while self.col < chars.len() && !chars[self.col].is_alphanumeric() {
            self.col += 1;
        }
        while self.col < chars.len() && chars[self.col].is_alphanumeric() {
            self.col += 1;
        }
    }

    /// Returns true when the key was an editing key.
    pub fn handle(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('a') if ctrl => self.col = 0,
            KeyCode::Char('e') if ctrl => self.col = self.line_len(),
            KeyCode::Char('k') if ctrl => {
                let at = byte_at(&self.lines[self.row], self.col);
                self.lines[self.row].truncate(at);
            }
            KeyCode::Char('b') if alt => self.word_left(),
            KeyCode::Char('f') if alt => self.word_right(),
            KeyCode::Char(c) if !ctrl && !alt => self.insert_str(&c.to_string()),
            KeyCode::Enter if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT => self.newline(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left if ctrl || alt => self.word_left(),
            KeyCode::Right if ctrl || alt => self.word_right(),
            KeyCode::Left => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.line_len();
                }
            }
            KeyCode::Right => {
                if self.col < self.line_len() {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.col = self.col.min(self.line_len());
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.col = self.col.min(self.line_len());
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.line_len(),
            KeyCode::PageUp => {
                self.row = self.row.saturating_sub(10);
                self.col = self.col.min(self.line_len());
            }
            KeyCode::PageDown => {
                self.row = (self.row + 10).min(self.lines.len() - 1);
                self.col = self.col.min(self.line_len());
            }
            _ => return false,
        }
        true
    }

    /// `active`: byte range of the statement that would run, underlined in
    /// the gutter.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool, active: Option<Range<usize>>) {
        let height = area.height as usize;
        if height == 0 {
            return;
        }
        if self.row < self.scroll {
            self.scroll = self.row;
        } else if self.row >= self.scroll + height {
            self.scroll = self.row + 1 - height;
        }
        let gutter = 2usize;
        let width = (area.width as usize).saturating_sub(gutter + 1);
        let before: String = self.lines[self.row].chars().take(self.col).collect();
        let cursor_x = before.width();
        if cursor_x < self.hscroll {
            self.hscroll = cursor_x;
        } else if cursor_x >= self.hscroll + width {
            self.hscroll = cursor_x + 1 - width;
        }

        let text = self.text();
        let highlighted = highlight(&text);
        let mut offset = 0;
        let mut lines = Vec::new();
        for (i, line) in self.lines.iter().enumerate() {
            let range = offset..offset + line.len();
            offset += line.len() + 1;
            if i < self.scroll || i >= self.scroll + height {
                continue;
            }
            let in_active = active
                .as_ref()
                .is_some_and(|a| range.start <= a.end && a.start <= range.end && !line.trim().is_empty());
            let marker = if in_active {
                Span::styled("▎ ", Style::new().fg(ratatui::style::Color::Cyan))
            } else {
                Span::raw("  ")
            };
            let mut spans = vec![marker];
            spans.extend(skip_columns(highlighted[i].clone(), self.hscroll));
            lines.push(Line::from(spans));
        }
        if self.is_blank() && !focused {
            lines = vec![Line::from(vec![Span::raw("  "), Span::styled("Tapez une requête SQL…", theme::dim())])];
        }
        frame.render_widget(Paragraph::new(lines), area);
        if focused {
            frame.set_cursor_position(Position::new(
                area.x + (gutter + cursor_x - self.hscroll) as u16,
                area.y + (self.row - self.scroll) as u16,
            ));
        }
    }
}

/// Drops the first `skip` display columns of a highlighted line.
fn skip_columns(line: Line<'static>, skip: usize) -> Vec<Span<'static>> {
    if skip == 0 {
        return line.spans;
    }
    let mut left = skip;
    let mut out = Vec::new();
    for span in line.spans {
        if left == 0 {
            out.push(span);
            continue;
        }
        let mut kept = String::new();
        for c in span.content.chars() {
            let w = c.width().unwrap_or(0);
            if left >= w && left > 0 {
                left -= w;
            } else {
                kept.push(c);
            }
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, span.style));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut Editor, code: KeyCode) {
        editor.handle(&KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn typing_newlines_and_offsets() {
        let mut editor = Editor::default();
        editor.insert_str("select 1;");
        press(&mut editor, KeyCode::Enter);
        editor.insert_str("  select é");
        press(&mut editor, KeyCode::Enter);
        assert_eq!(editor.text(), "select 1;\n  select é\n  ");
        press(&mut editor, KeyCode::Backspace);
        press(&mut editor, KeyCode::Backspace);
        press(&mut editor, KeyCode::Backspace);
        assert_eq!(editor.text(), "select 1;\n  select é");
        assert_eq!(editor.cursor_offset(), editor.text().len());
        press(&mut editor, KeyCode::Home);
        press(&mut editor, KeyCode::Backspace);
        assert_eq!(editor.text(), "select 1;  select é");
        assert_eq!(editor.cursor_offset(), 9);
    }

    #[test]
    fn paste_multiline() {
        let mut editor = Editor::default();
        editor.insert_str("a\r\nb\tc");
        assert_eq!(editor.text(), "a\nb    c");
        assert_eq!((editor.row, editor.col), (1, 6));
    }
}
