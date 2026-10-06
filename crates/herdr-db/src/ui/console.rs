//! Minimal SQL console: run a statement, see the result, no completion.
//! The user's SQL is never rewritten to add a LIMIT: results are streamed and
//! reading stops at the row cap, then the statement is cancelled server side.
//!
//! The console is the only writing surface of v0.1, so the production guards
//! live here: read-only session set by the engine, temporary write mode after
//! retyping the source name (blinking red banner), confirmation before
//! destructive statements.

use super::editor::Editor;
use super::inspector::Inspector;
use super::keys::Keymap;
use super::prompt::{PasswordPrompt, PromptOutcome};
use super::table::TableView;
use super::theme;
use super::widgets::{self, InputOutcome, Link, Status, TextInput};
use super::{Input, Perform, Program, Sender};
use crate::clipboard;
use crate::db::{DbError, DbHandle, DbStatus};
use crate::paths::HerdrEnv;
use crate::update::BinaryStamp;
use crossterm::event::{KeyEvent, MouseButton, MouseEventKind};
use herdr_db_core::cell::{QueryOutcome, StatementOutcome};
use herdr_db_core::config::SourceConfig;
use herdr_db_core::model::Engine;
use herdr_db_core::sql::quote_ident;
use herdr_db_core::statements;
use herdr_db_core::warnings::{self, Warning};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use secrecy::SecretString;
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Editor,
    Results,
}

pub struct Console {
    source: SourceConfig,
    schema: Option<String>,
    keys: Keymap,
    max_rows: usize,
    editor: Editor,
    focus: Focus,
    outcome: Option<QueryOutcome>,
    result_index: usize,
    table: TableView,
    running: Option<Instant>,
    write_mode: bool,
    confirm: Option<(String, Vec<Warning>)>,
    write_prompt: Option<TextInput>,
    inspector: Option<Inspector>,
    password: Option<PasswordPrompt>,
    pending: Option<String>,
    history: Vec<String>,
    history_pos: Option<usize>,
    help: bool,
    status: Status,
    link: Link,
    binary_replaced: bool,
    tick: usize,
    quit: bool,
    editor_area: Rect,
    table_area: Rect,
}

#[derive(Debug)]
pub enum Msg {
    Executed(Result<QueryOutcome, DbError>),
    SchemaSet(Result<(), DbError>),
    ReadOnlySet(bool, Result<(), DbError>),
    Link(DbStatus),
    Copied(bool, String),
    BinaryReplaced,
}

#[derive(Debug)]
pub enum Effect {
    UseSchema(String),
    Execute(String),
    Cancel,
    SetReadOnly(bool),
    Copy(String, String),
    ProvidePassword(SecretString, bool),
    CheckBinary,
}

/// Statement selecting the default schema.
pub fn use_schema_sql(engine: Engine, schema: &str) -> String {
    match engine {
        Engine::Postgres => format!("SET search_path TO {}, public", quote_ident(engine, schema)),
        Engine::MySql => format!("USE {}", quote_ident(engine, schema)),
    }
}

impl Console {
    pub fn new(source: SourceConfig, schema: Option<String>, keys: Keymap, max_rows: usize) -> Console {
        Console {
            source,
            schema,
            keys,
            max_rows,
            editor: Editor::default(),
            focus: Focus::Editor,
            outcome: None,
            result_index: 0,
            table: TableView::default(),
            running: None,
            write_mode: false,
            confirm: None,
            write_prompt: None,
            inspector: None,
            password: None,
            pending: None,
            history: Vec::new(),
            history_pos: None,
            help: false,
            status: Status::default(),
            link: Link::Idle,
            binary_replaced: false,
            tick: 0,
            quit: false,
            editor_area: Rect::default(),
            table_area: Rect::default(),
        }
    }

    /// The session rejects writes: read-only source without write mode.
    fn session_read_only(&self) -> bool {
        self.source.read_only && !self.write_mode
    }

    fn current_statement(&self) -> Option<String> {
        let text = self.editor.text();
        let range = statements::statement_at(self.source.engine, &text, self.editor.cursor_offset())?;
        Some(text[range].to_string())
    }

    fn execute(&mut self, sql: String, confirmed: bool) -> Vec<Effect> {
        if self.running.is_some() {
            self.status = Status::warning(format!("requête en cours ({} pour annuler)", self.keys.label("cancel")));
            return Vec::new();
        }
        let found = warnings::analyze(self.source.engine, &sql, self.session_read_only());
        if !confirmed && found.iter().any(|w| w.needs_confirmation()) && !self.session_read_only() {
            self.confirm = Some((sql, found));
            return Vec::new();
        }
        if found.contains(&Warning::WriteOnReadOnly) {
            self.status = Status::warning(Warning::WriteOnReadOnly.message());
        } else {
            self.status = Status::default();
        }
        if self.history.last() != Some(&sql) {
            self.history.push(sql.clone());
        }
        self.history_pos = None;
        self.running = Some(Instant::now());
        self.pending = Some(sql.clone());
        vec![Effect::Execute(sql)]
    }

    fn show_result(&mut self, index: usize) {
        let Some(outcome) = &self.outcome else { return };
        self.result_index = index.min(outcome.statements.len().saturating_sub(1));
        match outcome.statements.get(self.result_index) {
            Some(StatementOutcome::Rows { set, .. }) => {
                self.table.row = 0;
                self.table.col = 0;
                self.table.set(set.columns.clone(), set.rows.clone());
            }
            _ => self.table.clear(),
        }
    }

    fn summary(&self) -> Status {
        let Some(outcome) = &self.outcome else {
            return Status::default();
        };
        let elapsed = outcome.elapsed.as_millis();
        let count = outcome.statements.len();
        let which = if count > 1 { format!("[{}/{}] ", self.result_index + 1, count) } else { String::new() };
        match outcome.statements.get(self.result_index) {
            Some(StatementOutcome::Rows { set, truncated: true }) => Status::warning(format!(
                "{which}{} lignes affichées : plafond de {} atteint, requête arrêtée · {elapsed} ms",
                set.rows.len(),
                self.max_rows
            )),
            Some(StatementOutcome::Rows { set, .. }) => {
                Status::success(format!("{which}{} ligne(s) · {elapsed} ms", set.rows.len()))
            }
            Some(StatementOutcome::Command { tag, affected }) => Status::success(format!(
                "{which}{tag}{} · {elapsed} ms",
                affected.map(|n| format!(" {n}")).unwrap_or_default()
            )),
            None => Status::success(format!("OK · {elapsed} ms")),
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let Some(prompt) = &mut self.password {
            return match prompt.handle(&key) {
                PromptOutcome::Submit(password, save) => {
                    self.password = None;
                    let mut effects = vec![Effect::ProvidePassword(password, save)];
                    if let Some(schema) = &self.schema {
                        effects.push(Effect::UseSchema(schema.clone()));
                    }
                    if let Some(sql) = self.pending.take() {
                        effects.extend(self.execute(sql, true));
                    }
                    effects
                }
                PromptOutcome::Cancel => {
                    self.password = None;
                    self.pending = None;
                    self.status = Status::warning("connexion annulée");
                    Vec::new()
                }
                PromptOutcome::Pending => Vec::new(),
            };
        }
        if self.help {
            self.help = false;
            return Vec::new();
        }
        if let Some((sql, _)) = self.confirm.take() {
            return match key.code {
                crossterm::event::KeyCode::Char('o' | 'y' | 'O' | 'Y') => self.execute(sql, true),
                _ => {
                    self.status = Status::info("exécution annulée");
                    Vec::new()
                }
            };
        }
        if let Some(input) = &mut self.write_prompt {
            match input.handle(&key) {
                InputOutcome::Submit => {
                    let typed = input.text.trim().to_string();
                    self.write_prompt = None;
                    if typed == self.source.id {
                        return vec![Effect::SetReadOnly(false)];
                    }
                    self.status = Status::error("nom incorrect : mode écriture refusé");
                }
                InputOutcome::Cancel => self.write_prompt = None,
                _ => {}
            }
            return Vec::new();
        }
        if let Some(inspector) = &mut self.inspector {
            match self.keys.find(&["up", "down", "page_up", "page_down"], &key) {
                Some("up") => inspector.scroll(-1),
                Some("down") => inspector.scroll(1),
                Some("page_up") => inspector.scroll(-10),
                Some("page_down") => inspector.scroll(10),
                _ => self.inspector = None,
            }
            return Vec::new();
        }

        // Global console keys, valid in both focus modes.
        let global = ["execute", "cancel", "switch_focus", "write_mode", "history_prev", "history_next", "close"];
        if let Some(action) = self.keys.find(&global, &key) {
            match action {
                "execute" => {
                    return match self.current_statement() {
                        Some(sql) => self.execute(sql, false),
                        None => {
                            self.status = Status::info("rien à exécuter");
                            Vec::new()
                        }
                    };
                }
                "cancel" => {
                    if self.running.is_some() {
                        self.status = Status::info("annulation…");
                        return vec![Effect::Cancel];
                    }
                }
                "switch_focus" => {
                    self.focus = match self.focus {
                        Focus::Editor if !self.table.columns.is_empty() => Focus::Results,
                        _ => Focus::Editor,
                    }
                }
                "write_mode" => {
                    if !self.source.read_only {
                        self.status = Status::info("source déjà en lecture/écriture");
                    } else if self.write_mode {
                        return vec![Effect::SetReadOnly(true)];
                    } else {
                        self.write_prompt = Some(TextInput::default());
                    }
                }
                "history_prev" | "history_next" => {
                    if self.history.is_empty() {
                        return Vec::new();
                    }
                    let last = self.history.len() - 1;
                    let pos = match (action, self.history_pos) {
                        ("history_prev", None) => last,
                        ("history_prev", Some(p)) => p.saturating_sub(1),
                        (_, Some(p)) if p < last => p + 1,
                        _ => {
                            self.history_pos = None;
                            self.editor.set_text("");
                            return Vec::new();
                        }
                    };
                    self.history_pos = Some(pos);
                    self.editor.set_text(&self.history[pos]);
                }
                "close" => self.quit = true,
                _ => {}
            }
            return Vec::new();
        }

        match self.focus {
            Focus::Editor => {
                if key.code == crossterm::event::KeyCode::Esc && !self.table.columns.is_empty() {
                    self.focus = Focus::Results;
                } else {
                    self.editor.handle(&key);
                }
            }
            Focus::Results => {
                let actions = [
                    "up",
                    "down",
                    "left",
                    "right",
                    "top",
                    "bottom",
                    "page_up",
                    "page_down",
                    "prev_page",
                    "next_page",
                    "inspect",
                    "copy_cell",
                    "copy_csv",
                    "copy_json",
                    "select",
                    "help",
                    "quit",
                ];
                match self.keys.find(&actions, &key) {
                    Some("up") => self.table.move_row(-1),
                    Some("down") => self.table.move_row(1),
                    Some("left") => self.table.move_col(-1),
                    Some("right") => self.table.move_col(1),
                    Some("top") => self.table.top(),
                    Some("bottom") => self.table.bottom(),
                    Some("page_up") => self.table.page(-1),
                    Some("page_down") => self.table.page(1),
                    Some("prev_page") => {
                        self.show_result(self.result_index.saturating_sub(1));
                        self.status = self.summary();
                    }
                    Some("next_page") => {
                        self.show_result(self.result_index + 1);
                        self.status = self.summary();
                    }
                    Some("inspect") if !self.table.is_empty() => {
                        self.inspector = Some(Inspector {
                            title: format!("ligne {}", self.table.row + 1),
                            fields: self.table.row_fields(self.table.row),
                            scroll: 0,
                        });
                    }
                    Some("copy_cell") => {
                        if let Some(cell) = self.table.current_cell() {
                            let text = cell.copy_text().unwrap_or_default();
                            let label = herdr_db_core::cell::truncate_line(&text, 40);
                            return vec![Effect::Copy(text, label)];
                        }
                    }
                    Some("copy_csv") => {
                        return vec![Effect::Copy(self.table.rows_csv(), "ligne(s) en CSV".into())];
                    }
                    Some("copy_json") => {
                        return vec![Effect::Copy(self.table.rows_json(), "ligne(s) en JSON".into())];
                    }
                    Some("select") => self.table.toggle_selection(),
                    Some("help") => self.help = true,
                    Some("quit") => self.quit = true,
                    _ if key.code == crossterm::event::KeyCode::Esc => self.focus = Focus::Editor,
                    _ => {}
                }
            }
        }
        Vec::new()
    }

    fn on_msg(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Executed(result) => {
                self.running = None;
                match result {
                    Ok(outcome) => {
                        self.pending = None;
                        // Show the last result set, the usual one to look at.
                        let index = outcome
                            .statements
                            .iter()
                            .rposition(|s| matches!(s, StatementOutcome::Rows { .. }))
                            .unwrap_or(outcome.statements.len().saturating_sub(1));
                        self.outcome = Some(outcome);
                        self.show_result(index);
                        self.status = self.summary();
                    }
                    Err(DbError::NeedPassword { user, rejected }) => {
                        self.password = Some(PasswordPrompt::new(&self.source.id, &user, rejected));
                    }
                    Err(e) if e.is_cancelled() => {
                        self.pending = None;
                        self.status = Status::warning("requête annulée");
                    }
                    Err(e) => {
                        self.pending = None;
                        self.status = Status::error(e.to_string());
                    }
                }
            }
            Msg::SchemaSet(result) => match result {
                Ok(()) => {}
                Err(DbError::NeedPassword { user, rejected }) => {
                    self.password = Some(PasswordPrompt::new(&self.source.id, &user, rejected));
                }
                Err(e) => self.status = Status::error(format!("schéma par défaut : {e}")),
            },
            Msg::ReadOnlySet(read_only, result) => match result {
                Ok(()) => {
                    self.write_mode = !read_only;
                    self.status = if read_only {
                        Status::success("retour en lecture seule")
                    } else {
                        Status::warning("mode écriture actif pour cette session")
                    };
                }
                Err(e) => self.status = Status::error(e.to_string()),
            },
            Msg::Link(status) => {
                self.link = match status {
                    DbStatus::TunnelStarting => Link::Tunnel,
                    DbStatus::Connecting => Link::Connecting,
                    DbStatus::Connected { .. } => Link::Connected,
                    DbStatus::Disconnected => Link::Lost,
                }
            }
            Msg::Copied(ok, label) => {
                self.status = if ok {
                    Status::success(format!("copié : {label}"))
                } else {
                    Status::warning("presse-papier indisponible")
                }
            }
            Msg::BinaryReplaced => self.binary_replaced = true,
        }
        Vec::new()
    }
}

impl Program for Console {
    type Msg = Msg;
    type Effect = Effect;

    fn init(&mut self) -> Vec<Effect> {
        match &self.schema {
            Some(schema) => vec![Effect::UseSchema(schema.clone())],
            None => Vec::new(),
        }
    }

    fn update(&mut self, input: Input<Msg>) -> Vec<Effect> {
        match input {
            Input::Key(key) => self.on_key(key),
            Input::Paste(text) => {
                if let Some(prompt) = &mut self.password {
                    prompt.input.insert_str(&text);
                } else if let Some(input) = &mut self.write_prompt {
                    input.insert_str(&text);
                } else {
                    self.focus = Focus::Editor;
                    self.editor.insert_str(&text);
                }
                Vec::new()
            }
            Input::Mouse(mouse) => {
                let inside = |area: Rect| {
                    mouse.column >= area.x
                        && mouse.column < area.x + area.width
                        && mouse.row >= area.y
                        && mouse.row < area.y + area.height
                };
                match mouse.kind {
                    MouseEventKind::Down(MouseButton::Left) if inside(self.table_area) => {
                        self.focus = Focus::Results;
                        self.table.click(self.table_area, mouse.column, mouse.row);
                    }
                    MouseEventKind::Down(MouseButton::Left) if inside(self.editor_area) => self.focus = Focus::Editor,
                    MouseEventKind::ScrollDown if inside(self.table_area) => self.table.move_row(3),
                    MouseEventKind::ScrollUp if inside(self.table_area) => self.table.move_row(-3),
                    _ => {}
                }
                Vec::new()
            }
            Input::Resize => Vec::new(),
            Input::Tick => {
                self.tick += 1;
                if self.tick.is_multiple_of(120) { vec![Effect::CheckBinary] } else { Vec::new() }
            }
            Input::Msg(msg) => self.on_msg(msg),
        }
    }

    fn view(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let editor_height = (area.height.saturating_sub(3) * 2 / 5).clamp(3, 14);
        let [banner, editor, results, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(editor_height + 1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let mut context = format!("console{}", self.schema.as_ref().map(|s| format!(" · {s}")).unwrap_or_default());
        if self.binary_replaced {
            context.push_str(" · binaire mis à jour : relancez");
        }
        widgets::banner(frame, banner, &self.source, &context, self.write_mode, &self.link);

        let editor_block = Block::new().borders(Borders::BOTTOM).border_style(if self.focus == Focus::Editor {
            Style::new().fg(Color::Cyan)
        } else {
            theme::dim()
        });
        let editor_inner = editor_block.inner(editor);
        frame.render_widget(editor_block, editor);
        self.editor_area = editor_inner;
        let text = self.editor.text();
        let active = statements::statement_at(self.source.engine, &text, self.editor.cursor_offset());
        let editor_focused = self.focus == Focus::Editor
            && self.password.is_none()
            && self.write_prompt.is_none()
            && self.confirm.is_none();
        self.editor.render(frame, editor_inner, editor_focused, active);

        self.table_area = results;
        if self.table.columns.is_empty() {
            let hint = match &self.outcome {
                Some(_) => String::new(),
                None => format!(
                    "{} exécute l'instruction sous le curseur · {} bascule éditeur/résultats",
                    self.keys.label("execute"),
                    self.keys.label("switch_focus")
                ),
            };
            frame.render_widget(Paragraph::new(Line::styled(hint, theme::dim())), results);
        } else {
            self.table.render(frame, results, self.focus == Focus::Results);
        }

        let shown = match self.running {
            Some(started) => Status::info(format!(
                "{} exécution… {:.1} s ({} pour annuler)",
                ["|", "/", "-", "\\"][self.tick % 4],
                started.elapsed().as_secs_f32(),
                self.keys.label("cancel")
            )),
            None => self.status.clone(),
        };
        widgets::status_line(frame, status, &shown, &format!("{} aide", self.keys.label("help")));

        if let Some(input) = &self.write_prompt {
            let rect = widgets::centered(area, 60, 5);
            let inner = widgets::popup(frame, rect, "Mode écriture");
            let [message, field] = Layout::vertical([Constraint::Length(2), Constraint::Length(1)]).areas(inner);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::styled(
                        format!("Source {} en lecture seule ({}).", self.source.id, self.source.environment),
                        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(format!("Retapez « {} » pour écrire pendant cette session :", self.source.id)),
                ]),
                message,
            );
            input.render(frame, field, "› ", Style::new(), true);
        }
        if let Some((_, warnings)) = &self.confirm {
            let mut lines: Vec<Line<'static>> = warnings
                .iter()
                .filter(|w| w.needs_confirmation())
                .map(|w| Line::styled(format!("⚠ {}", w.message()), Style::new().fg(Color::Yellow)))
                .collect();
            lines.push(Line::raw(""));
            lines.push(Line::from(vec![
                Span::raw("Exécuter sur "),
                Span::styled(self.source.id.clone(), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(format!(" ({}) ? o / N", self.source.environment)),
            ]));
            widgets::message_popup(frame, "Confirmation", lines);
        }
        if let Some(inspector) = &self.inspector {
            inspector.render(frame);
        }
        if self.help {
            let k = |a: &str| self.keys.label(a);
            let entries = vec![
                (k("execute"), "exécuter l'instruction sous le curseur"),
                (k("cancel"), "annuler la requête en cours"),
                (k("switch_focus"), "éditeur ↔ résultats"),
                (format!("{} / {}", k("history_prev"), k("history_next")), "historique"),
                (k("write_mode"), "mode écriture (source en lecture seule)"),
                (format!("{} / {}", k("prev_page"), k("next_page")), "résultat précédent / suivant"),
                (k("inspect"), "inspecteur de ligne (résultats)"),
                (format!("{} {} {}", k("copy_cell"), k("copy_csv"), k("copy_json")), "copier cellule / CSV / JSON"),
                (k("close"), "fermer la console"),
            ];
            widgets::help_popup(frame, "Console — touche pour fermer", &entries);
        }
        if let Some(prompt) = &self.password {
            prompt.render(frame);
        }
    }

    fn quit(&self) -> bool {
        self.quit
    }
}

// ------------------------------------------------------------------ executor

pub struct ConsoleExec {
    env: Arc<HerdrEnv>,
    source: SourceConfig,
    max_rows: usize,
    db: Option<DbHandle>,
    stamp: BinaryStamp,
}

impl ConsoleExec {
    pub fn new(env: Arc<HerdrEnv>, source: SourceConfig, max_rows: usize) -> ConsoleExec {
        ConsoleExec { env, source, max_rows, db: None, stamp: BinaryStamp::current() }
    }
}

impl Perform<Console> for ConsoleExec {
    fn perform(&mut self, effect: Effect, tx: &Sender<Msg>) {
        let db = self
            .db
            .get_or_insert_with(|| {
                let tx = tx.clone();
                DbHandle::spawn(
                    self.source.clone(),
                    self.env.clone(),
                    None,
                    Box::new(move |status| {
                        let _ = tx.send(Msg::Link(status));
                    }),
                )
            })
            .clone();
        let tx = tx.clone();
        let engine = self.source.engine;
        let max_rows = self.max_rows;
        match effect {
            Effect::UseSchema(schema) => {
                tokio::spawn(async move {
                    let result = db.execute(use_schema_sql(engine, &schema), 1).await.map(|_| ());
                    let _ = tx.send(Msg::SchemaSet(result));
                });
            }
            Effect::Execute(sql) => {
                tokio::spawn(async move {
                    let _ = tx.send(Msg::Executed(db.execute(sql, max_rows).await));
                });
            }
            Effect::Cancel => {
                tokio::spawn(async move {
                    let _ = db.cancel().await;
                });
            }
            Effect::SetReadOnly(read_only) => {
                tokio::spawn(async move {
                    let result = db.set_read_only(read_only).await;
                    let _ = tx.send(Msg::ReadOnlySet(read_only, result));
                });
            }
            Effect::Copy(text, label) => {
                tokio::task::spawn_blocking(move || {
                    let ok = clipboard::copy(&text);
                    let _ = tx.send(Msg::Copied(ok, label));
                });
            }
            Effect::ProvidePassword(password, save) => db.provide_password(password, save),
            Effect::CheckBinary => {
                if self.stamp.replaced() {
                    let _ = tx.send(Msg::BinaryReplaced);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};
    use herdr_db_core::cell::{Cell, ResultColumn, RowSet};
    use herdr_db_core::config;
    use std::collections::BTreeMap;
    use std::time::Duration;

    fn console(read_only: bool) -> Console {
        let text = format!(
            "[[sources]]\nid = \"db_prod\"\nengine = \"postgres\"\nenvironment = \"production\"\ndatabase = \"app\"\nread_only = {read_only}\n"
        );
        let source = config::load(Some((std::path::Path::new("t.toml"), &text)), None).unwrap().sources.remove(0);
        Console::new(source, Some("public".into()), Keymap::new(&BTreeMap::new()), 100)
    }

    fn ctrl(c: char) -> Input<Msg> {
        Input::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    fn type_text(console: &mut Console, text: &str) {
        console.update(Input::Paste(text.to_string()));
    }

    #[test]
    fn runs_the_statement_under_the_cursor() {
        let mut console = console(true);
        assert!(matches!(console.init().as_slice(), [Effect::UseSchema(s)] if s == "public"));
        type_text(&mut console, "select 1;\nselect 2");
        let effects = console.update(ctrl('r'));
        assert!(matches!(effects.as_slice(), [Effect::Execute(sql)] if sql == "select 2"));
        assert!(console.update(ctrl('r')).is_empty(), "one query at a time");
        assert!(matches!(console.update(ctrl('c')).as_slice(), [Effect::Cancel]));
        let outcome = QueryOutcome {
            statements: vec![StatementOutcome::Rows {
                set: RowSet {
                    columns: vec![ResultColumn::new("x", Some("int4".into()))],
                    rows: vec![vec![Cell::Text("2".into())]],
                },
                truncated: false,
            }],
            elapsed: Duration::from_millis(4),
        };
        console.update(Input::Msg(Msg::Executed(Ok(outcome))));
        assert_eq!(console.status.text, "1 ligne(s) · 4 ms");
        assert_eq!(console.table.rows.len(), 1);
    }

    #[test]
    fn destructive_statements_need_confirmation_when_writable() {
        let mut console = console(false);
        type_text(&mut console, "delete from users");
        assert!(console.update(ctrl('r')).is_empty());
        assert!(console.confirm.is_some());
        let effects = console.update(Input::Key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)));
        assert!(matches!(effects.as_slice(), [Effect::Execute(sql)] if sql == "delete from users"));
    }

    #[test]
    fn read_only_source_needs_the_name_retyped() {
        let mut console = console(true);
        type_text(&mut console, "delete from users");
        // Read-only session: no confirmation, the engine refuses the write.
        assert!(matches!(console.update(ctrl('r')).as_slice(), [Effect::Execute(_)]));
        assert_eq!(console.status.text, Warning::WriteOnReadOnly.message());
        console.update(Input::Msg(Msg::Executed(Err(DbError::Driver(herdr_db_drivers::DriverError::Cancelled)))));

        assert!(console.update(ctrl('w')).is_empty());
        type_text(&mut console, "db_pro");
        console.update(Input::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!console.write_mode);
        assert_eq!(console.status.text, "nom incorrect : mode écriture refusé");

        console.update(ctrl('w'));
        type_text(&mut console, "db_prod");
        let effects = console.update(Input::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(matches!(effects.as_slice(), [Effect::SetReadOnly(false)]));
        console.update(Input::Msg(Msg::ReadOnlySet(false, Ok(()))));
        assert!(console.write_mode);
        // Now writable: destructive statements ask first.
        console.update(ctrl('r'));
        assert!(console.confirm.is_some());
        console.update(Input::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE)));
        assert!(matches!(console.update(ctrl('w')).as_slice(), [Effect::SetReadOnly(true)]));
    }

    #[test]
    fn schema_statements() {
        assert_eq!(use_schema_sql(Engine::Postgres, "Audit"), "SET search_path TO \"Audit\", public");
        assert_eq!(use_schema_sql(Engine::MySql, "platform"), "USE `platform`");
    }
}
