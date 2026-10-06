//! Go to DDL: the object's DDL, read from the cached model (introspected on
//! demand when missing). PostgreSQL DDL is rebuilt from the catalog; MySQL's
//! comes from `SHOW CREATE`.

use super::highlight::highlight;
use super::keys::Keymap;
use super::prompt::{PasswordPrompt, PromptOutcome};
use super::theme;
use super::widgets::{self, Link, Status};
use super::{Input, Perform, Program, Sender};
use crate::clipboard;
use crate::db::{DbError, DbHandle, DbStatus};
use crate::introspect::{self, IntrospectError};
use crate::paths::HerdrEnv;
use crossterm::event::{KeyEvent, MouseEventKind};
use herdr_db_core::config::SourceConfig;
use herdr_db_core::ddl::object_ddl;
use herdr_db_core::model::{ObjectRef, TableDetail};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use secrecy::SecretString;
use std::sync::Arc;

pub struct DdlView {
    source: SourceConfig,
    object: ObjectRef,
    keys: Keymap,
    text: String,
    lines: Vec<Line<'static>>,
    scroll: usize,
    hscroll: u16,
    height: usize,
    loading: bool,
    password: Option<PasswordPrompt>,
    status: Status,
    link: Link,
    quit: bool,
}

#[derive(Debug)]
pub enum Msg {
    Detail(Result<TableDetail, IntrospectError>),
    Link(DbStatus),
    Copied(bool),
}

#[derive(Debug)]
pub enum Effect {
    Load { refresh: bool },
    Copy(String),
    ProvidePassword(SecretString, bool),
}

impl DdlView {
    pub fn new(source: SourceConfig, object: ObjectRef, keys: Keymap) -> DdlView {
        DdlView {
            source,
            object,
            keys,
            text: String::new(),
            lines: Vec::new(),
            scroll: 0,
            hscroll: 0,
            height: 1,
            loading: true,
            password: None,
            status: Status::info("chargement…"),
            link: Link::Idle,
            quit: false,
        }
    }

    fn set_detail(&mut self, detail: &TableDetail) {
        self.text = object_ddl(self.source.engine, &self.object, detail);
        self.lines = highlight(self.text.trim_end());
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.height)
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let Some(prompt) = &mut self.password {
            return match prompt.handle(&key) {
                PromptOutcome::Submit(password, save) => {
                    self.password = None;
                    self.loading = true;
                    vec![Effect::ProvidePassword(password, save), Effect::Load { refresh: false }]
                }
                PromptOutcome::Cancel => {
                    self.password = None;
                    self.status = Status::warning("connexion annulée");
                    Vec::new()
                }
                PromptOutcome::Pending => Vec::new(),
            };
        }
        let actions =
            ["up", "down", "left", "right", "top", "bottom", "page_up", "page_down", "copy_cell", "reload", "quit"];
        match self.keys.find(&actions, &key) {
            Some("left") => self.hscroll = self.hscroll.saturating_sub(8),
            Some("right") => self.hscroll = self.hscroll.saturating_add(8),
            Some("up") => self.scroll = self.scroll.saturating_sub(1),
            Some("down") => self.scroll = (self.scroll + 1).min(self.max_scroll()),
            Some("top") => self.scroll = 0,
            Some("bottom") => self.scroll = self.max_scroll(),
            Some("page_up") => self.scroll = self.scroll.saturating_sub(self.height / 2),
            Some("page_down") => self.scroll = (self.scroll + self.height / 2).min(self.max_scroll()),
            Some("copy_cell") if !self.text.is_empty() => return vec![Effect::Copy(self.text.clone())],
            Some("reload") => {
                self.loading = true;
                self.status = Status::info("introspection…");
                return vec![Effect::Load { refresh: true }];
            }
            Some("quit") => self.quit = true,
            _ => {}
        }
        Vec::new()
    }
}

impl Program for DdlView {
    type Msg = Msg;
    type Effect = Effect;

    fn init(&mut self) -> Vec<Effect> {
        vec![Effect::Load { refresh: false }]
    }

    fn update(&mut self, input: Input<Msg>) -> Vec<Effect> {
        match input {
            Input::Key(key) => self.on_key(key),
            Input::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => self.scroll = (self.scroll + 3).min(self.max_scroll()),
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
            Input::Msg(Msg::Detail(result)) => {
                self.loading = false;
                match result {
                    Ok(detail) => {
                        self.set_detail(&detail);
                        self.status = Status::default();
                    }
                    Err(IntrospectError::Db(DbError::NeedPassword { user, rejected })) => {
                        self.password = Some(PasswordPrompt::new(&self.source.id, &user, rejected));
                    }
                    Err(e) => self.status = Status::error(e.to_string()),
                }
                Vec::new()
            }
            Input::Msg(Msg::Link(status)) => {
                self.link = match status {
                    DbStatus::TunnelStarting => Link::Tunnel,
                    DbStatus::Connecting => Link::Connecting,
                    DbStatus::Connected { .. } => Link::Connected,
                    DbStatus::Disconnected => Link::Idle,
                };
                Vec::new()
            }
            Input::Msg(Msg::Copied(ok)) => {
                self.status =
                    if ok { Status::success("DDL copié") } else { Status::warning("presse-papier indisponible") };
                Vec::new()
            }
        }
    }

    fn view(&mut self, frame: &mut Frame) {
        let [banner, title, body, status] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
                .areas(frame.area());
        widgets::banner(frame, banner, &self.source, "DDL", false, &self.link);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(self.object.to_string(), Style::new().fg(Color::LightBlue)),
                Span::styled(format!("  {}", self.source.engine.label()), theme::dim()),
            ])),
            title,
        );
        self.height = body.height as usize;
        self.scroll = self.scroll.min(self.max_scroll());
        frame.render_widget(Paragraph::new(self.lines.clone()).scroll((self.scroll as u16, self.hscroll)), body);
        let hint = format!(
            "{} copier · {} réintrospecter · {} fermer",
            self.keys.label("copy_cell"),
            self.keys.label("reload"),
            self.keys.label("quit")
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

pub struct DdlExec {
    env: Arc<HerdrEnv>,
    source: SourceConfig,
    object: ObjectRef,
    db: Option<DbHandle>,
}

impl DdlExec {
    pub fn new(env: Arc<HerdrEnv>, source: SourceConfig, object: ObjectRef) -> DdlExec {
        DdlExec { env, source, object, db: None }
    }
}

impl Perform<DdlView> for DdlExec {
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
            Effect::Load { refresh } => {
                let (state_dir, source, object) =
                    (self.env.state_dir.clone(), self.source.clone(), self.object.clone());
                tokio::spawn(async move {
                    let result = if refresh {
                        introspect::introspect_table(&db, state_dir, source, object, || {}).await
                    } else {
                        introspect::detail(&db, state_dir, source, object, || {}).await
                    };
                    let _ = tx.send(Msg::Detail(result));
                });
            }
            Effect::Copy(text) => {
                tokio::task::spawn_blocking(move || {
                    let _ = tx.send(Msg::Copied(clipboard::copy(&text)));
                });
            }
            Effect::ProvidePassword(password, save) => db.provide_password(password, save),
        }
    }
}
