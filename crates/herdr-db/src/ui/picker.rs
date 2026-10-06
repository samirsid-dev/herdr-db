//! Selector shown when a pane opens without a request (from Herdr's action
//! palette): pick a source (console) or a cached object (grid, DDL, docs).

use super::theme;
use super::widgets::{InputOutcome, TextInput};
use super::{Input, Perform, Program, Sender};
use crossterm::event::{KeyCode, MouseEventKind};
use herdr_db_core::request::PaneRequest;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

#[derive(Debug, Clone)]
pub struct PickItem {
    pub label: String,
    pub detail: String,
    pub request: PaneRequest,
}

pub struct Picker {
    title: String,
    items: Vec<PickItem>,
    filter: TextInput,
    cursor: usize,
    pub chosen: Option<PaneRequest>,
    quit: bool,
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<PickItem>) -> Picker {
        Picker { title: title.into(), items, filter: TextInput::default(), cursor: 0, chosen: None, quit: false }
    }

    fn visible(&self) -> Vec<&PickItem> {
        let query = self.filter.text.to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        self.items
            .iter()
            .filter(|item| {
                let haystack = format!("{} {}", item.label, item.detail).to_lowercase();
                words.iter().all(|w| haystack.contains(w))
            })
            .collect()
    }
}

impl Program for Picker {
    type Msg = ();
    type Effect = ();

    fn update(&mut self, input: Input<()>) -> Vec<()> {
        match input {
            Input::Key(key) => match key.code {
                KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
                KeyCode::Down => self.cursor += 1,
                _ => match self.filter.handle(&key) {
                    InputOutcome::Submit => {
                        self.chosen = self.visible().get(self.cursor).map(|i| i.request.clone());
                        self.quit = self.chosen.is_some();
                    }
                    InputOutcome::Cancel => self.quit = true,
                    InputOutcome::Changed => self.cursor = 0,
                    InputOutcome::Ignored => {}
                },
            },
            Input::Paste(text) => {
                self.filter.insert_str(&text);
                self.cursor = 0;
            }
            Input::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollDown => self.cursor += 1,
                MouseEventKind::ScrollUp => self.cursor = self.cursor.saturating_sub(1),
                _ => {}
            },
            _ => {}
        }
        self.cursor = self.cursor.min(self.visible().len().saturating_sub(1));
        Vec::new()
    }

    fn view(&mut self, frame: &mut Frame) {
        let [title, input, list] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1)]).areas(frame.area());
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {}", self.title), Style::new().fg(Color::Cyan))),
            title,
        );
        self.filter.render(frame, input, " › ", Style::new(), true);
        let height = list.height as usize;
        let skip = self.cursor.saturating_sub(height.saturating_sub(1));
        let lines: Vec<Line> = self
            .visible()
            .into_iter()
            .enumerate()
            .skip(skip)
            .take(height)
            .map(|(i, item)| {
                let style = if i == self.cursor { theme::selected() } else { Style::new() };
                Line::from(vec![
                    Span::styled(format!(" {} ", item.label), style),
                    Span::styled(format!(" {}", item.detail), theme::dim()),
                ])
            })
            .collect();
        if lines.is_empty() {
            frame
                .render_widget(Paragraph::new(Line::styled(" aucun résultat (Échap pour fermer)", theme::dim())), list);
        } else {
            frame.render_widget(Paragraph::new(lines), list);
        }
    }

    fn quit(&self) -> bool {
        self.quit
    }
}

pub struct NoEffects;

impl Perform<Picker> for NoEffects {
    fn perform(&mut self, _: (), _: &Sender<()>) {}
}

/// Full-pane error message: a pane that fails must say why before closing.
pub struct Fatal {
    lines: Vec<String>,
    quit: bool,
}

impl Fatal {
    pub fn new(message: &str) -> Fatal {
        Fatal { lines: message.lines().map(str::to_string).collect(), quit: false }
    }
}

impl Program for Fatal {
    type Msg = ();
    type Effect = ();

    fn update(&mut self, input: Input<()>) -> Vec<()> {
        if let Input::Key(key) = input
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc | KeyCode::Enter)
        {
            self.quit = true;
        }
        Vec::new()
    }

    fn view(&mut self, frame: &mut Frame) {
        let mut lines = vec![Line::styled(" Herdr DB", Style::new().fg(Color::Red)), Line::raw("")];
        lines.extend(self.lines.iter().map(|l| Line::raw(format!(" {l}"))));
        lines.push(Line::raw(""));
        lines.push(Line::styled(" q pour fermer", theme::dim()));
        frame.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), frame.area());
    }

    fn quit(&self) -> bool {
        self.quit
    }
}

impl Perform<Fatal> for NoEffects {
    fn perform(&mut self, _: (), _: &Sender<()>) {}
}
