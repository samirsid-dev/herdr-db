//! Password prompt shown when the server asks for one and neither
//! `password_command` nor the keyring provided it.

use super::theme;
use super::widgets::{self, InputOutcome, TextInput};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use secrecy::SecretString;

#[derive(Debug, Clone)]
pub struct PasswordPrompt {
    pub source: String,
    pub user: String,
    pub rejected: bool,
    pub input: TextInput,
    pub save: bool,
}

pub enum PromptOutcome {
    Pending,
    Submit(SecretString, bool),
    Cancel,
}

impl PasswordPrompt {
    pub fn new(source: &str, user: &str, rejected: bool) -> PasswordPrompt {
        PasswordPrompt {
            source: source.to_string(),
            user: user.to_string(),
            rejected,
            input: TextInput::masked(),
            save: true,
        }
    }

    pub fn handle(&mut self, key: &KeyEvent) -> PromptOutcome {
        if key.code == KeyCode::Tab {
            self.save = !self.save;
            return PromptOutcome::Pending;
        }
        match self.input.handle(key) {
            InputOutcome::Submit => {
                PromptOutcome::Submit(SecretString::from(std::mem::take(&mut self.input.text)), self.save)
            }
            InputOutcome::Cancel => PromptOutcome::Cancel,
            _ => PromptOutcome::Pending,
        }
    }

    pub fn render(&self, frame: &mut Frame) {
        let rect = widgets::centered(frame.area(), 52, 6);
        let inner = widgets::popup(frame, rect, &format!("Mot de passe · {}", self.source));
        let [message, input, save] =
            Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
        let (text, style) = if self.rejected {
            (format!("Mot de passe refusé pour {}.", self.user), theme::error())
        } else {
            (format!("Le serveur demande un mot de passe pour {}.", self.user), Style::new())
        };
        frame.render_widget(Paragraph::new(text).style(style), message);
        self.input.render(frame, input, "› ", Style::new(), true);
        let mark = if self.save { "[x]" } else { "[ ]" };
        frame.render_widget(
            Paragraph::new(Line::styled(format!("{mark} enregistrer dans le trousseau (Tab)"), theme::dim())),
            save,
        );
    }
}
