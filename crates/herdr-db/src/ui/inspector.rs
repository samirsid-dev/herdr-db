//! Row inspector: every field of a row, full values, JSON pretty-printed.

use super::theme;
use super::widgets;
use herdr_db_core::cell::Cell;
use ratatui::Frame;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

#[derive(Debug, Clone)]
pub struct Inspector {
    pub title: String,
    pub fields: Vec<(String, Option<String>, Cell)>,
    pub scroll: u16,
}

impl Inspector {
    pub fn lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (name, type_name, cell) in &self.fields {
            let mut header =
                vec![Span::styled(name.clone(), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))];
            if let Some(t) = type_name {
                header.push(Span::styled(format!("  {t}"), theme::dim()));
            }
            lines.push(Line::from(header));
            let style = match cell {
                Cell::Null => theme::null_style(),
                Cell::Json(_) => Style::new().fg(Color::LightYellow),
                Cell::Binary { .. } => theme::dim(),
                Cell::Text(_) => Style::new(),
            };
            let text = cell.full_text();
            if text.is_empty() {
                lines.push(Line::styled("  (chaîne vide)", theme::dim()));
            }
            for line in text.lines() {
                lines.push(Line::styled(format!("  {line}"), style));
            }
            lines.push(Line::raw(""));
        }
        lines
    }

    pub fn scroll(&mut self, delta: i32) {
        let max = self.lines().len().saturating_sub(1) as i32;
        self.scroll = (self.scroll as i32 + delta).clamp(0, max) as u16;
    }

    pub fn render(&self, frame: &mut Frame) {
        let area = frame.area();
        let rect = widgets::centered(area, area.width.saturating_sub(6).max(40), area.height.saturating_sub(4));
        let inner = widgets::popup(frame, rect, &self.title);
        frame.render_widget(Paragraph::new(self.lines()).scroll((self.scroll, 0)), inner);
    }
}
