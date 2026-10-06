//! Quick Documentation: a popup to read and close. Table or view summary
//! (columns with badges, keys, foreign keys, indexes, triggers, definition),
//! or one column's details.

use super::keys::Keymap;
use super::prompt::{PasswordPrompt, PromptOutcome};
use super::theme::{self, Icons};
use super::widgets::{self, Link, Status};
use super::{Input, Perform, Program, Sender};
use crate::clipboard;
use crate::db::{DbError, DbHandle, DbStatus};
use crate::introspect::{self, IntrospectError};
use crate::paths::HerdrEnv;
use crate::views::{Opener, Origin};
use crossterm::event::{KeyCode, KeyEvent, MouseEventKind};
use herdr_db_core::config::SourceConfig;
use herdr_db_core::model::{ObjectRef, TableDetail};
use herdr_db_core::request::{Action, PaneRequest};
use herdr_db_core::sql::{display_ident, qualified};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use secrecy::SecretString;
use std::sync::Arc;
use unicode_width::UnicodeWidthStr;

pub struct QuickDoc {
    source: SourceConfig,
    object: ObjectRef,
    column: Option<String>,
    keys: Keymap,
    icons: Icons,
    estimate: Option<i64>,
    detail: Option<TableDetail>,
    scroll: u16,
    password: Option<PasswordPrompt>,
    status: Status,
    link: Link,
    quit: bool,
}

#[derive(Debug)]
pub enum Msg {
    Detail(Result<TableDetail, IntrospectError>),
    Link(DbStatus),
    Copied(bool, String),
    Opened(Result<(), String>),
}

#[derive(Debug)]
pub enum Effect {
    Load,
    Copy(String),
    Open(PaneRequest),
    ProvidePassword(SecretString, bool),
}

fn heading(text: &str) -> Line<'static> {
    Line::styled(text.to_string(), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD))
}

impl QuickDoc {
    pub fn new(
        source: SourceConfig,
        object: ObjectRef,
        column: Option<String>,
        keys: Keymap,
        icons: Icons,
        estimate: Option<i64>,
    ) -> QuickDoc {
        QuickDoc {
            source,
            object,
            column,
            keys,
            icons,
            estimate,
            detail: None,
            scroll: 0,
            password: None,
            status: Status::info("chargement…"),
            link: Link::Idle,
            quit: false,
        }
    }

    fn reference(&self) -> String {
        let base = qualified(self.source.engine, &self.object.schema, &self.object.name);
        match &self.column {
            Some(column) => format!("{base}.{}", display_ident(self.source.engine, column)),
            None => base,
        }
    }

    pub fn lines(&self) -> Vec<Line<'static>> {
        let Some(detail) = &self.detail else {
            return Vec::new();
        };
        match &self.column {
            Some(column) => self.column_lines(detail, column),
            None => self.table_lines(detail),
        }
    }

    fn table_lines(&self, detail: &TableDetail) -> Vec<Line<'static>> {
        let mut lines = vec![Line::from(vec![
            Span::styled(self.object.name.clone(), Style::new().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("  {} · {} · {}", detail.kind.label(), self.object.schema, self.source.label()),
                theme::dim(),
            ),
        ])];
        if let Some(comment) = &detail.comment {
            lines.push(Line::styled(comment.clone(), Style::new().add_modifier(Modifier::ITALIC)));
        }
        if let Some(estimate) = self.estimate.filter(|e| *e >= 0) {
            lines.push(Line::styled(
                format!("~{} lignes (estimation du catalogue)", super::grid::group_thousands(estimate as u64)),
                theme::dim(),
            ));
        }
        lines.push(Line::raw(""));

        lines.push(heading("Colonnes"));
        let name_width = detail.columns.iter().map(|c| c.name.width()).max().unwrap_or(4);
        let type_width = detail.columns.iter().map(|c| c.data_type.width()).max().unwrap_or(4).min(30);
        for column in &detail.columns {
            let mut spans = vec![Span::raw("  ")];
            let badges = self.icons.badges(detail.badges(&column.name));
            let badge_width: usize = badges.iter().map(|s| s.content.width()).sum();
            spans.extend(badges);
            spans.push(Span::raw(" ".repeat(6usize.saturating_sub(badge_width))));
            spans.push(Span::raw(format!("{:<name_width$}  ", column.name)));
            spans.push(Span::styled(format!("{:<type_width$}", column.data_type), theme::dim()));
            let mut extra = Vec::new();
            if !column.nullable {
                extra.push("NOT NULL".to_string());
            }
            if let Some(default) = &column.default {
                extra.push(format!("default {default}"));
            }
            if let Some(generated) = &column.generated {
                extra.push(format!("generated {}", generated.expression));
            }
            if let Some(e) = &column.extra {
                extra.push(e.clone());
            }
            if !extra.is_empty() {
                spans.push(Span::styled(format!("  {}", extra.join(" · ")), theme::dim()));
            }
            if let Some(comment) = &column.comment {
                spans.push(Span::styled(format!("  — {comment}"), Style::new().add_modifier(Modifier::ITALIC)));
            }
            lines.push(Line::from(spans));
        }

        let keys: Vec<_> = detail.keys().collect();
        if !keys.is_empty() {
            lines.push(Line::raw(""));
            lines.push(heading("Clés"));
            for key in keys {
                lines.push(Line::raw(format!("  {}  {} ({})", key.name, key.kind.label(), key.columns.join(", "))));
            }
        }
        if !detail.foreign_keys.is_empty() {
            lines.push(Line::raw(""));
            lines.push(heading("Clés étrangères"));
            for fk in &detail.foreign_keys {
                let mut text = format!(
                    "  {}  ({}) → {}.{} ({})",
                    fk.name,
                    fk.columns.join(", "),
                    fk.ref_schema,
                    fk.ref_table,
                    fk.ref_columns.join(", ")
                );
                if let Some(rule) = &fk.on_delete {
                    text.push_str(&format!(" ON DELETE {rule}"));
                }
                if let Some(rule) = &fk.on_update {
                    text.push_str(&format!(" ON UPDATE {rule}"));
                }
                lines.push(Line::raw(text));
            }
        }
        if !detail.indexes.is_empty() {
            lines.push(Line::raw(""));
            lines.push(heading("Index"));
            for index in &detail.indexes {
                let unique = if index.unique { "unique " } else { "" };
                let method = index.method.as_deref().map(|m| format!(" {m}")).unwrap_or_default();
                lines.push(Line::raw(format!("  {}  {unique}({}){method}", index.name, index.columns.join(", "))));
            }
        }
        let checks: Vec<_> = detail.checks().collect();
        if !checks.is_empty() {
            lines.push(Line::raw(""));
            lines.push(heading("Checks"));
            for check in checks {
                lines.push(Line::raw(format!("  {}  {}", check.name, check.definition.clone().unwrap_or_default())));
            }
        }
        if !detail.triggers.is_empty() {
            lines.push(Line::raw(""));
            lines.push(heading("Triggers"));
            for trigger in &detail.triggers {
                lines.push(Line::raw(format!(
                    "  {}  {} {}",
                    trigger.name,
                    trigger.timing,
                    trigger.events.join(" OR ")
                )));
            }
        }
        if let Some(definition) = &detail.view_definition {
            lines.push(Line::raw(""));
            lines.push(heading("Définition"));
            for line in super::highlight::highlight(definition.trim()) {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(line.spans);
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    fn column_lines(&self, detail: &TableDetail, name: &str) -> Vec<Line<'static>> {
        let Some(column) = detail.column(name) else {
            return vec![Line::styled(format!("colonne {name} introuvable"), theme::error())];
        };
        let mut title =
            vec![Span::styled(column.name.clone(), Style::new().add_modifier(Modifier::BOLD)), Span::raw("  ")];
        title.extend(self.icons.badges(detail.badges(name)));
        title.push(Span::styled(format!("  colonne de {}", self.object), theme::dim()));
        let mut lines = vec![Line::from(title)];
        if let Some(comment) = &column.comment {
            lines.push(Line::styled(comment.clone(), Style::new().add_modifier(Modifier::ITALIC)));
        }
        lines.push(Line::raw(""));
        let field = |label: &str, value: String| {
            Line::from(vec![Span::styled(format!("{label:<14}"), theme::dim()), Span::raw(value)])
        };
        lines.push(field("type", column.data_type.clone()));
        lines.push(field("nullable", if column.nullable { "oui".into() } else { "non (NOT NULL)".into() }));
        lines.push(field("position", column.ordinal.to_string()));
        if let Some(default) = &column.default {
            lines.push(field("défaut", default.clone()));
        }
        if let Some(generated) = &column.generated {
            let kind = if generated.stored { "stockée" } else { "virtuelle" };
            lines.push(field("générée", format!("{} ({kind})", generated.expression)));
        }
        if let Some(identity) = column.identity {
            lines.push(field("identité", format!("{identity:?}")));
        }
        if let Some(extra) = &column.extra {
            lines.push(field("extra", extra.clone()));
        }
        if let Some((fk, target)) = detail.foreign_key_for(name) {
            lines.push(field("référence", format!("{}.{}.{target} ({})", fk.ref_schema, fk.ref_table, fk.name)));
        }
        let indexes: Vec<String> =
            detail.indexes.iter().filter(|i| i.columns.iter().any(|c| c == name)).map(|i| i.name.clone()).collect();
        if !indexes.is_empty() {
            lines.push(field("index", indexes.join(", ")));
        }
        lines
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let Some(prompt) = &mut self.password {
            return match prompt.handle(&key) {
                PromptOutcome::Submit(password, save) => {
                    self.password = None;
                    vec![Effect::ProvidePassword(password, save), Effect::Load]
                }
                PromptOutcome::Cancel => {
                    self.password = None;
                    self.quit = true;
                    Vec::new()
                }
                PromptOutcome::Pending => Vec::new(),
            };
        }
        if key.code == KeyCode::Esc {
            self.quit = true;
            return Vec::new();
        }
        let actions = ["up", "down", "page_up", "page_down", "top", "copy_reference", "edit_data", "ddl", "quit"];
        match self.keys.find(&actions, &key) {
            Some("up") => self.scroll = self.scroll.saturating_sub(1),
            Some("down") => self.scroll = self.scroll.saturating_add(1).min(self.lines().len() as u16),
            Some("page_up") => self.scroll = self.scroll.saturating_sub(10),
            Some("page_down") => self.scroll = self.scroll.saturating_add(10).min(self.lines().len() as u16),
            Some("top") => self.scroll = 0,
            Some("copy_reference") => return vec![Effect::Copy(self.reference())],
            Some("edit_data") => {
                return vec![Effect::Open(PaneRequest::for_object(Action::EditData, self.object.clone()))];
            }
            Some("ddl") => return vec![Effect::Open(PaneRequest::for_object(Action::GoToDdl, self.object.clone()))],
            Some("quit") => self.quit = true,
            _ => {}
        }
        Vec::new()
    }
}

impl Program for QuickDoc {
    type Msg = Msg;
    type Effect = Effect;

    fn init(&mut self) -> Vec<Effect> {
        vec![Effect::Load]
    }

    fn update(&mut self, input: Input<Msg>) -> Vec<Effect> {
        match input {
            Input::Key(key) => self.on_key(key),
            Input::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(3),
                    MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
                    _ => {}
                }
                Vec::new()
            }
            Input::Paste(text) => {
                if let Some(prompt) = &mut self.password {
                    prompt.input.insert_str(&text);
                }
                Vec::new()
            }
            Input::Resize | Input::Tick => Vec::new(),
            Input::Msg(msg) => {
                match msg {
                    Msg::Detail(Ok(detail)) => {
                        self.detail = Some(detail);
                        self.status = Status::default();
                    }
                    Msg::Detail(Err(IntrospectError::Db(DbError::NeedPassword { user, rejected }))) => {
                        self.password = Some(PasswordPrompt::new(&self.source.id, &user, rejected));
                    }
                    Msg::Detail(Err(e)) => self.status = Status::error(e.to_string()),
                    Msg::Link(status) => {
                        self.link = match status {
                            DbStatus::TunnelStarting => Link::Tunnel,
                            DbStatus::Connecting => Link::Connecting,
                            _ => Link::Idle,
                        }
                    }
                    Msg::Copied(ok, text) => {
                        self.status = if ok {
                            Status::success(format!("copié : {text}"))
                        } else {
                            Status::warning("presse-papier indisponible")
                        }
                    }
                    Msg::Opened(Ok(())) => self.quit = true,
                    Msg::Opened(Err(e)) => self.status = Status::error(e),
                }
                Vec::new()
            }
        }
    }

    fn view(&mut self, frame: &mut Frame) {
        let [banner, body, status] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
        widgets::banner(frame, banner, &self.source, "Quick Documentation", false, &self.link);
        frame.render_widget(Paragraph::new(self.lines()).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), body);
        let hint = format!(
            "{} copier · {} données · {} DDL · Échap fermer",
            self.keys.label("copy_reference"),
            self.keys.label("edit_data"),
            self.keys.label("ddl")
        );
        widgets::status_line(frame, status, &self.status, &hint);
        if let Some(prompt) = &self.password {
            prompt.render(frame);
        }
    }

    fn quit(&self) -> bool {
        self.quit
    }
}

pub struct QuickDocExec {
    env: Arc<HerdrEnv>,
    source: SourceConfig,
    object: ObjectRef,
    opener: Opener,
    db: Option<DbHandle>,
}

impl QuickDocExec {
    pub fn new(env: Arc<HerdrEnv>, source: SourceConfig, object: ObjectRef, opener: Opener) -> QuickDocExec {
        QuickDocExec { env, source, object, opener, db: None }
    }
}

impl Perform<QuickDoc> for QuickDocExec {
    fn perform(&mut self, effect: Effect, tx: &Sender<Msg>) {
        let db = self
            .db
            .get_or_insert_with(|| {
                let tx = tx.clone();
                DbHandle::spawn(
                    self.source.clone(),
                    self.env.clone(),
                    Some(std::time::Duration::from_secs(60)),
                    Box::new(move |status| {
                        let _ = tx.send(Msg::Link(status));
                    }),
                )
            })
            .clone();
        let tx = tx.clone();
        match effect {
            Effect::Load => {
                let (state_dir, source, object) =
                    (self.env.state_dir.clone(), self.source.clone(), self.object.clone());
                tokio::spawn(async move {
                    let result = introspect::detail(&db, state_dir, source, object, || {}).await;
                    let _ = tx.send(Msg::Detail(result));
                });
            }
            Effect::Copy(text) => {
                tokio::task::spawn_blocking(move || {
                    let ok = clipboard::copy(&text);
                    let _ = tx.send(Msg::Copied(ok, text));
                });
            }
            Effect::Open(request) => {
                let opener = self.opener.clone();
                tokio::spawn(async move {
                    let result = opener.open(request, Origin::Other).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Opened(result));
                });
            }
            Effect::ProvidePassword(password, save) => db.provide_password(password, save),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_db_core::config;
    use herdr_db_core::model::{Column, Constraint, ConstraintKind, ObjectKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn doc(column: Option<&str>) -> QuickDoc {
        let source = config::load(
            Some((
                std::path::Path::new("t.toml"),
                "[[sources]]\nid = \"local\"\nengine = \"postgres\"\ndatabase = \"app\"\n",
            )),
            None,
        )
        .unwrap()
        .sources
        .remove(0);
        let mut doc = QuickDoc::new(
            source,
            ObjectRef::new("local", "public", "users"),
            column.map(str::to_string),
            Keymap::new(&BTreeMap::new()),
            Icons { nerd: false },
            Some(42),
        );
        let col = |name: &str, data_type: &str, nullable: bool, comment: Option<&str>| Column {
            name: name.into(),
            ordinal: 1,
            data_type: data_type.into(),
            nullable,
            default: None,
            comment: comment.map(str::to_string),
            generated: None,
            identity: None,
            extra: None,
        };
        doc.update(Input::Msg(Msg::Detail(Ok(TableDetail {
            kind: ObjectKind::Table,
            columns: vec![col("id", "bigint", false, None), col("email", "text", true, Some("adresse de contact"))],
            indexes: vec![],
            foreign_keys: vec![],
            constraints: vec![Constraint {
                name: "users_pkey".into(),
                kind: ConstraintKind::PrimaryKey,
                columns: vec!["id".into()],
                definition: None,
            }],
            triggers: vec![],
            comment: Some("Comptes utilisateurs".into()),
            partition_key: None,
            view_definition: None,
            native_ddl: None,
        }))));
        doc
    }

    #[test]
    fn renders_table_documentation() {
        let mut doc = doc(None);
        let mut terminal = Terminal::new(TestBackend::new(64, 12)).unwrap();
        terminal.draw(|f| doc.view(f)).unwrap();
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn column_documentation_and_reference() {
        let mut doc = doc(Some("email"));
        let text: Vec<String> = doc.lines().iter().map(|l| l.to_string()).collect();
        assert!(text[0].starts_with("email"));
        assert!(text.iter().any(|l| l.contains("adresse de contact")));
        let effects = doc.update(Input::Key(KeyEvent::new(KeyCode::Char('y'), crossterm::event::KeyModifiers::NONE)));
        assert!(matches!(effects.as_slice(), [Effect::Copy(t)] if t == "public.users.email"));
    }
}
