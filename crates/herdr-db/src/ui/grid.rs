//! Data grid (read-only in v0.1). Opens a table or a view in its own pane and
//! never loads more than one page: a `SELECT *` on a table of millions of
//! rows stays instant. Keyset pagination on the primary key when it applies,
//! `OFFSET` otherwise; catalog estimate for the row count, exact `COUNT(*)`
//! on demand.

use super::inspector::Inspector;
use super::keys::Keymap;
use super::prompt::{PasswordPrompt, PromptOutcome};
use super::table::TableView;
use super::theme;
use super::widgets::{self, InputOutcome, Link, Status, TextInput};
use super::{Input, Perform, Program, Sender};
use crate::clipboard;
use crate::db::{DbError, DbHandle, DbStatus};
use crate::introspect::{self, IntrospectError};
use crate::paths::HerdrEnv;
use crate::update::BinaryStamp;
use crate::views::{Opener, Origin};
use crossterm::event::{KeyEvent, MouseButton, MouseEventKind};
use herdr_db_core::cell::{Cell, Page};
use herdr_db_core::config::SourceConfig;
use herdr_db_core::model::{ObjectRef, TableDetail};
use herdr_db_core::paging::{PageRequest, Position, Sort, SortDir, count_sql};
use herdr_db_core::request::{Action, PaneRequest};
use herdr_db_core::sql::{quote_ident, quote_literal};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use secrecy::SecretString;
use std::sync::Arc;
use std::time::Duration;

pub struct Grid {
    source: SourceConfig,
    object: ObjectRef,
    keys: Keymap,
    page_size: usize,
    detail: Option<TableDetail>,
    estimate: Option<i64>,
    exact: Option<u64>,
    filter: Option<String>,
    sort: Option<Sort>,
    position: Position,
    page_index: Option<usize>,
    more_before: bool,
    more_after: bool,
    force_offset: bool,
    table: TableView,
    seq: u64,
    loading: bool,
    counting: bool,
    elapsed: Option<Duration>,
    filter_input: Option<TextInput>,
    inspector: Option<Inspector>,
    password: Option<PasswordPrompt>,
    help: bool,
    status: Status,
    link: Link,
    binary_replaced: bool,
    tick: usize,
    quit: bool,
    table_area: Rect,
}

#[derive(Debug)]
pub enum Msg {
    Detail(Result<TableDetail, IntrospectError>),
    Page(u64, Position, Result<Page, DbError>),
    Count(Result<u64, DbError>),
    Link(DbStatus),
    Copied(bool, String),
    Opened(Result<(), String>),
    BinaryReplaced,
}

#[derive(Debug)]
pub enum Effect {
    LoadDetail,
    Fetch(u64, Box<PageRequest>),
    Count(String),
    Cancel,
    Copy(String, String),
    Open(PaneRequest),
    ProvidePassword(SecretString, bool),
    CheckBinary,
}

impl Grid {
    pub fn new(
        source: SourceConfig,
        object: ObjectRef,
        filter: Option<String>,
        keys: Keymap,
        page_size: usize,
        estimate: Option<i64>,
    ) -> Grid {
        Grid {
            source,
            object,
            keys,
            page_size,
            detail: None,
            estimate,
            exact: None,
            filter,
            sort: None,
            position: Position::First,
            page_index: Some(0),
            more_before: false,
            more_after: false,
            force_offset: false,
            table: TableView::default(),
            seq: 0,
            loading: true,
            counting: false,
            elapsed: None,
            filter_input: None,
            inspector: None,
            password: None,
            help: false,
            status: Status::info("chargement…"),
            link: Link::Idle,
            binary_replaced: false,
            tick: 0,
            quit: false,
            table_area: Rect::default(),
        }
    }

    fn primary_key(&self) -> Vec<String> {
        self.detail.as_ref().map(TableDetail::primary_key_columns).unwrap_or_default()
    }

    fn request(&self, position: Position) -> PageRequest {
        let column_types = self
            .detail
            .as_ref()
            .map(|d| d.columns.iter().map(|c| (c.name.clone(), c.data_type.clone())).collect())
            .unwrap_or_default();
        PageRequest {
            engine: self.source.engine,
            object: self.object.clone(),
            filter: self.filter.clone(),
            sort: self.sort.clone(),
            primary_key: if self.force_offset { Vec::new() } else { self.primary_key() },
            page_size: self.page_size,
            position,
            column_types,
        }
    }

    fn keyset(&self) -> bool {
        self.request(Position::First).keyset_applicable()
    }

    fn fetch(&mut self, position: Position) -> Vec<Effect> {
        self.seq += 1;
        self.loading = true;
        self.status = Status::info("chargement…");
        let request = self.request(position);
        vec![Effect::Fetch(self.seq, Box::new(request))]
    }

    /// Key values of a displayed row, for keyset positions.
    fn row_keys(&self, row: usize) -> Option<Vec<String>> {
        let cells = self.table.rows.get(row)?;
        self.primary_key()
            .iter()
            .map(|k| match cells.get(self.table.column_index(k)?)? {
                Cell::Text(t) => Some(t.clone()),
                _ => None,
            })
            .collect()
    }

    fn offset(&self) -> u64 {
        self.page_index.unwrap_or(0) as u64 * self.page_size as u64
    }

    fn next_page(&mut self) -> Vec<Effect> {
        if !self.more_after {
            self.status = Status::info("dernière page");
            return Vec::new();
        }
        if self.keyset() {
            if let Some(keys) = self.table.rows.len().checked_sub(1).and_then(|last| self.row_keys(last)) {
                return self.fetch(Position::After(keys));
            }
            self.force_offset = true;
        }
        let offset = self.offset() + self.page_size as u64;
        self.fetch(Position::Offset(offset))
    }

    fn prev_page(&mut self) -> Vec<Effect> {
        if !self.more_before {
            self.status = Status::info("première page");
            return Vec::new();
        }
        if self.keyset()
            && let Some(keys) = self.row_keys(0)
        {
            return self.fetch(Position::Before(keys));
        }
        let offset = self.offset().saturating_sub(self.page_size as u64);
        self.fetch(Position::Offset(offset))
    }

    fn last_page(&mut self) -> Vec<Effect> {
        if self.keyset() {
            return self.fetch(Position::Last);
        }
        match self.exact {
            Some(count) if count > 0 => {
                let offset = (count - 1) / self.page_size as u64 * self.page_size as u64;
                self.fetch(Position::Offset(offset))
            }
            _ => {
                self.status = Status::warning(format!(
                    "sans clé primaire, la dernière page demande le compte exact ({})",
                    self.keys.label("count")
                ));
                Vec::new()
            }
        }
    }

    fn cycle_sort(&mut self) -> Vec<Effect> {
        let Some(column) = self.table.current_column().map(|c| c.name.clone()) else {
            return Vec::new();
        };
        self.sort = match &self.sort {
            Some(s) if s.column == column && s.dir == SortDir::Asc => Some(Sort { column, dir: SortDir::Desc }),
            Some(s) if s.column == column => None,
            _ => Some(Sort { column, dir: SortDir::Asc }),
        };
        self.table.sort = self.sort.as_ref().map(|s| (s.column.clone(), s.dir));
        self.fetch(Position::First)
    }

    fn follow_foreign_key(&mut self) -> Vec<Effect> {
        let Some(column) = self.table.current_column().map(|c| c.name.clone()) else {
            return Vec::new();
        };
        let Some(detail) = &self.detail else {
            return Vec::new();
        };
        let Some((fk, _)) = detail.foreign_key_for(&column) else {
            self.status = Status::warning(format!("{column} n'est pas une clé étrangère"));
            return Vec::new();
        };
        let engine = self.source.engine;
        let mut conditions = Vec::new();
        for (local, remote) in fk.columns.iter().zip(&fk.ref_columns) {
            let value = self.table.column_index(local).and_then(|i| self.table.rows.get(self.table.row)?.get(i));
            match value {
                Some(Cell::Text(v)) => {
                    conditions.push(format!("{} = {}", quote_ident(engine, remote), quote_literal(engine, v)))
                }
                _ => {
                    self.status = Status::warning("valeur NULL : aucune ligne référencée");
                    return Vec::new();
                }
            }
        }
        let target = ObjectRef::new(self.source.id.clone(), fk.ref_schema.clone(), fk.ref_table.clone());
        let mut request = PaneRequest::for_object(Action::EditData, target);
        request.filter = Some(conditions.join(" AND "));
        self.status = Status::info(format!("ouverture de {}.{}…", fk.ref_schema, fk.ref_table));
        vec![Effect::Open(request)]
    }

    fn copy(&mut self, action: &str) -> Vec<Effect> {
        let (text, label) = match action {
            "copy_cell" => match self.table.current_cell() {
                Some(Cell::Null) => (String::new(), "NULL (copié vide)".to_string()),
                Some(cell) => {
                    let text = cell.copy_text().unwrap_or_default();
                    let label = herdr_db_core::cell::truncate_line(&text, 40);
                    (text, label)
                }
                None => return Vec::new(),
            },
            "copy_csv" => (self.table.rows_csv(), format!("{} ligne(s) en CSV", self.table.selected_rows().len())),
            _ => (self.table.rows_json(), format!("{} ligne(s) en JSON", self.table.selected_rows().len())),
        };
        vec![Effect::Copy(text, label)]
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let Some(prompt) = &mut self.password {
            return match prompt.handle(&key) {
                PromptOutcome::Submit(password, save) => {
                    self.password = None;
                    self.status = Status::info("connexion…");
                    vec![Effect::ProvidePassword(password, save), Effect::LoadDetail]
                }
                PromptOutcome::Cancel => {
                    self.password = None;
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
        if let Some(input) = &mut self.filter_input {
            match input.handle(&key) {
                InputOutcome::Submit => {
                    let text = input.text.trim().to_string();
                    self.filter = Some(text).filter(|t| !t.is_empty());
                    self.filter_input = None;
                    self.exact = None;
                    return self.fetch(Position::First);
                }
                InputOutcome::Cancel => self.filter_input = None,
                _ => {}
            }
            return Vec::new();
        }
        if let Some(inspector) = &mut self.inspector {
            match self.keys.find(&["up", "down", "page_up", "page_down", "copy_json", "copy_cell"], &key) {
                Some("up") => inspector.scroll(-1),
                Some("down") => inspector.scroll(1),
                Some("page_up") => inspector.scroll(-10),
                Some("page_down") => inspector.scroll(10),
                Some("copy_json" | "copy_cell") => {
                    return vec![Effect::Copy(self.table.rows_json(), "ligne en JSON".into())];
                }
                _ => self.inspector = None,
            }
            return Vec::new();
        }
        let actions = [
            "cancel",
            "up",
            "down",
            "left",
            "right",
            "top",
            "bottom",
            "page_up",
            "page_down",
            "next_page",
            "prev_page",
            "first_page",
            "last_page",
            "sort",
            "filter",
            "inspect",
            "follow_fk",
            "copy_cell",
            "copy_csv",
            "copy_json",
            "select",
            "count",
            "reload",
            "console",
            "help",
            "quit",
        ];
        let Some(action) = self.keys.find(&actions, &key) else {
            return Vec::new();
        };
        self.status = Status::default();
        match action {
            "cancel" => {
                if self.loading || self.counting {
                    return vec![Effect::Cancel];
                }
            }
            "up" => self.table.move_row(-1),
            "down" => self.table.move_row(1),
            "left" => self.table.move_col(-1),
            "right" => self.table.move_col(1),
            "top" => self.table.top(),
            "bottom" => self.table.bottom(),
            "page_up" => self.table.page(-1),
            "page_down" => self.table.page(1),
            "next_page" => return self.next_page(),
            "prev_page" => return self.prev_page(),
            "first_page" => return self.fetch(Position::First),
            "last_page" => return self.last_page(),
            "sort" => return self.cycle_sort(),
            "filter" => self.filter_input = Some(TextInput::new(self.filter.clone().unwrap_or_default())),
            "inspect" => {
                if !self.table.is_empty() {
                    self.inspector = Some(Inspector {
                        title: format!("{} · ligne {}", self.object, self.table.row + 1),
                        fields: self.table.row_fields(self.table.row),
                        scroll: 0,
                    });
                }
            }
            "follow_fk" => return self.follow_foreign_key(),
            "copy_cell" | "copy_csv" | "copy_json" => return self.copy(action),
            "select" => self.table.toggle_selection(),
            "count" => {
                self.counting = true;
                self.status = Status::info("COUNT(*)…");
                return vec![Effect::Count(count_sql(self.source.engine, &self.object, self.filter.as_deref()))];
            }
            "reload" => return self.fetch(self.position.clone()),
            "console" => {
                let mut request = PaneRequest::new(Action::Console);
                request.source = Some(self.source.id.clone());
                request.schema = Some(self.object.schema.clone());
                return vec![Effect::Open(request)];
            }
            "help" => self.help = true,
            "quit" => self.quit = true,
            _ => {}
        }
        Vec::new()
    }

    fn on_msg(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Detail(result) => {
                match result {
                    Ok(detail) => {
                        self.table.key_columns = detail.primary_key_columns();
                        self.table.fk_columns = detail.foreign_keys.iter().flat_map(|fk| fk.columns.clone()).collect();
                        self.detail = Some(detail);
                    }
                    Err(IntrospectError::Db(DbError::NeedPassword { user, rejected })) => {
                        self.loading = false;
                        self.password = Some(PasswordPrompt::new(&self.source.id, &user, rejected));
                        return Vec::new();
                    }
                    // Still try the data: views without detail stay readable.
                    Err(e) => self.status = Status::warning(format!("structure indisponible : {e}")),
                }
                return self.fetch(Position::First);
            }
            Msg::Page(seq, position, result) => {
                if seq != self.seq {
                    return Vec::new();
                }
                self.loading = false;
                match result {
                    Ok(page) => {
                        let forward = matches!(position, Position::First | Position::Offset(_) | Position::After(_));
                        let (before, after) = match &position {
                            Position::First => (false, page.has_more),
                            Position::Offset(n) => (*n > 0, page.has_more),
                            Position::After(_) => (true, page.has_more),
                            Position::Before(_) => (page.has_more, true),
                            Position::Last => (page.has_more, false),
                        };
                        self.page_index = match (&position, self.page_index) {
                            (Position::First, _) => Some(0),
                            (Position::Offset(n), _) => Some((*n / self.page_size as u64) as usize),
                            (Position::After(_), Some(i)) => Some(i + 1),
                            (Position::Before(_), _) if !before => Some(0),
                            (Position::Before(_), Some(i)) => Some(i.saturating_sub(1)),
                            (Position::Last, _) => {
                                self.exact.map(|c| (c.saturating_sub(1) / self.page_size as u64) as usize)
                            }
                            _ => None,
                        };
                        self.more_before = before;
                        self.more_after = after;
                        self.elapsed = Some(page.elapsed);
                        let keep_cursor = (self.table.row, self.table.col);
                        self.table.set(page.rows.columns, page.rows.rows);
                        self.table.row = if forward { 0 } else { self.table.rows.len().saturating_sub(1) };
                        if matches!(position, Position::Offset(_)) && self.position == position {
                            self.table.row = keep_cursor.0.min(self.table.rows.len().saturating_sub(1));
                        }
                        self.table.col = keep_cursor.1.min(self.table.columns.len().saturating_sub(1));
                        self.position = position;
                        self.status = Status::default();
                    }
                    Err(DbError::NeedPassword { user, rejected }) => {
                        self.password = Some(PasswordPrompt::new(&self.source.id, &user, rejected));
                    }
                    Err(e) if e.is_cancelled() => self.status = Status::warning("requête annulée"),
                    Err(e) => self.status = Status::error(e.to_string()),
                }
            }
            Msg::Count(result) => {
                self.counting = false;
                match result {
                    Ok(count) => {
                        self.exact = Some(count);
                        self.status = Status::success(format!("{} ligne(s)", group_thousands(count)));
                    }
                    Err(e) if e.is_cancelled() => self.status = Status::warning("COUNT annulé"),
                    Err(e) => self.status = Status::error(e.to_string()),
                }
            }
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
            Msg::Opened(result) => {
                if let Err(e) = result {
                    self.status = Status::error(e);
                }
            }
            Msg::BinaryReplaced => self.binary_replaced = true,
        }
        Vec::new()
    }

    fn toolbar(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(self.object.to_string(), Style::new().fg(Color::LightBlue))];
        let count = match (self.exact, self.estimate) {
            (Some(exact), _) => format!("  {} lignes", group_thousands(exact)),
            (None, Some(estimate)) if estimate >= 0 => format!("  ~{} lignes", group_thousands(estimate as u64)),
            _ => String::new(),
        };
        spans.push(Span::styled(count, theme::dim()));
        let page = match self.page_index {
            Some(i) => format!("  page {}", i + 1),
            None => "  dernière page".to_string(),
        };
        spans.push(Span::raw(page));
        if self.keyset() {
            spans.push(Span::styled(" (keyset)", theme::dim()));
        }
        if let Some(filter) = &self.filter {
            spans.push(Span::styled("  WHERE ", Style::new().fg(Color::Yellow)));
            spans.push(Span::raw(herdr_db_core::cell::truncate_line(filter, 60)));
        }
        if let Some(sort) = &self.sort {
            spans.push(Span::styled("  ORDER BY ", Style::new().fg(Color::Yellow)));
            spans.push(Span::raw(format!("{} {}", sort.column, if sort.dir == SortDir::Asc { "▲" } else { "▼" })));
        }
        Line::from(spans)
    }

    fn position_hint(&self) -> String {
        let mut text = String::new();
        if let Some(column) = self.table.current_column() {
            text.push_str(&column.name);
            if let Some(t) = &column.type_name {
                text.push(' ');
                text.push_str(t);
            }
            if let Some(detail) = &self.detail {
                let badges = detail.badges(&column.name);
                if badges.primary_key {
                    text.push_str(" · PK");
                }
                if badges.foreign_key {
                    text.push_str(&format!(" · FK ({})", self.keys.label("follow_fk")));
                }
            }
        }
        if !self.table.is_empty() {
            text.push_str(&format!(" · ligne {}/{}", self.table.row + 1, self.table.rows.len()));
        }
        if let Some(elapsed) = self.elapsed {
            text.push_str(&format!(" · {} ms", elapsed.as_millis()));
        }
        text
    }
}

impl Program for Grid {
    type Msg = Msg;
    type Effect = Effect;

    fn init(&mut self) -> Vec<Effect> {
        vec![Effect::LoadDetail]
    }

    fn update(&mut self, input: Input<Msg>) -> Vec<Effect> {
        match input {
            Input::Key(key) => self.on_key(key),
            Input::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => self.table.move_row(3),
                    MouseEventKind::ScrollUp => self.table.move_row(-3),
                    MouseEventKind::Down(MouseButton::Left) => {
                        self.table.click(self.table_area, mouse.column, mouse.row);
                    }
                    _ => {}
                }
                Vec::new()
            }
            Input::Paste(text) => {
                if let Some(input) = &mut self.filter_input {
                    input.insert_str(&text);
                } else if let Some(prompt) = &mut self.password {
                    prompt.input.insert_str(&text);
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
        let [banner, toolbar, body, status] =
            Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)])
                .areas(frame.area());
        let context = if self.binary_replaced { "binaire mis à jour : relancez" } else { "" };
        widgets::banner(frame, banner, &self.source, context, false, &self.link);
        match &self.filter_input {
            Some(input) => input.render(frame, toolbar, "WHERE › ", Style::new(), true),
            None => frame.render_widget(Paragraph::new(self.toolbar()), toolbar),
        }
        self.table_area = body;
        self.table.render(frame, body, true);
        let status_text = if self.loading || self.counting {
            Status::info(format!("{} {}", spinner(self.tick), self.status.text))
        } else if self.status.text.is_empty() {
            Status::info(self.position_hint())
        } else {
            self.status.clone()
        };
        widgets::status_line(frame, status, &status_text, &format!("{} aide", self.keys.label("help")));

        if let Some(inspector) = &self.inspector {
            inspector.render(frame);
        }
        if self.help {
            let k = |a: &str| self.keys.label(a);
            let entries = vec![
                (format!("{} {} {} {}", k("left"), k("down"), k("up"), k("right")), "se déplacer"),
                (format!("{} / {}", k("next_page"), k("prev_page")), "page suivante / précédente"),
                (format!("{} / {}", k("first_page"), k("last_page")), "première / dernière page"),
                (k("sort"), "trier sur la colonne"),
                (k("filter"), "filtre WHERE"),
                (k("inspect"), "inspecteur de ligne"),
                (k("follow_fk"), "ouvrir la ligne référencée (FK)"),
                (k("copy_cell"), "copier la cellule"),
                (format!("{} / {}", k("copy_csv"), k("copy_json")), "copier ligne(s) en CSV / JSON"),
                (k("select"), "sélection de lignes"),
                (k("count"), "COUNT(*) exact"),
                (k("reload"), "recharger"),
                (k("console"), "console SQL sous la grille"),
                (k("cancel"), "annuler la requête"),
                (k("quit"), "fermer"),
            ];
            widgets::help_popup(frame, "Grille — touche pour fermer", &entries);
        }
        if let Some(prompt) = &self.password {
            prompt.render(frame);
        }
    }

    fn quit(&self) -> bool {
        self.quit
    }
}

/// `300000` → `300 000` (narrow no-break space, French typography).
pub fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
}

fn spinner(tick: usize) -> &'static str {
    ["|", "/", "-", "\\"][tick % 4]
}

// ------------------------------------------------------------------ executor

pub struct GridExec {
    env: Arc<HerdrEnv>,
    source: SourceConfig,
    object: ObjectRef,
    opener: Opener,
    db: Option<DbHandle>,
    stamp: BinaryStamp,
}

impl GridExec {
    pub fn new(env: Arc<HerdrEnv>, source: SourceConfig, object: ObjectRef, opener: Opener) -> GridExec {
        GridExec { env, source, object, opener, db: None, stamp: BinaryStamp::current() }
    }

    fn db(&mut self, tx: &Sender<Msg>) -> DbHandle {
        self.db
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
            .clone()
    }
}

impl Perform<Grid> for GridExec {
    fn perform(&mut self, effect: Effect, tx: &Sender<Msg>) {
        let db = self.db(tx);
        let tx = tx.clone();
        match effect {
            Effect::LoadDetail => {
                let (state_dir, source, object) =
                    (self.env.state_dir.clone(), self.source.clone(), self.object.clone());
                tokio::spawn(async move {
                    let result = introspect::detail(&db, state_dir, source, object, || {}).await;
                    let _ = tx.send(Msg::Detail(result));
                });
            }
            Effect::Fetch(seq, request) => {
                tokio::spawn(async move {
                    let position = request.position.clone();
                    let result = db.fetch_page(*request).await;
                    let _ = tx.send(Msg::Page(seq, position, result));
                });
            }
            Effect::Count(sql) => {
                tokio::spawn(async move {
                    let _ = tx.send(Msg::Count(db.count(sql).await));
                });
            }
            Effect::Cancel => {
                tokio::spawn(async move {
                    let _ = db.cancel().await;
                });
            }
            Effect::Copy(text, label) => {
                tokio::task::spawn_blocking(move || {
                    let ok = clipboard::copy(&text);
                    let _ = tx.send(Msg::Copied(ok, label));
                });
            }
            Effect::Open(request) => {
                let opener = self.opener.clone();
                tokio::spawn(async move {
                    let result = opener.open(request, Origin::Grid).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Opened(result));
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
    use herdr_db_core::cell::{ResultColumn, RowSet};
    use herdr_db_core::config;
    use herdr_db_core::model::{Column, Constraint, ConstraintKind, ForeignKey, ObjectKind};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn source() -> SourceConfig {
        config::load(
            Some((
                std::path::Path::new("t.toml"),
                "[[sources]]\nid = \"db_prod\"\nengine = \"postgres\"\nenvironment = \"production\"\ndatabase = \"app\"\n",
            )),
            None,
        )
        .unwrap()
        .sources
        .remove(0)
    }

    fn detail() -> TableDetail {
        let column = |name: &str, data_type: &str| Column {
            name: name.into(),
            ordinal: 1,
            data_type: data_type.into(),
            nullable: true,
            default: None,
            comment: None,
            generated: None,
            identity: None,
            extra: None,
        };
        TableDetail {
            kind: ObjectKind::Table,
            columns: vec![column("id", "integer"), column("org_id", "integer"), column("note", "text")],
            indexes: vec![],
            foreign_keys: vec![ForeignKey {
                name: "fk".into(),
                columns: vec!["org_id".into()],
                ref_schema: "public".into(),
                ref_table: "orgs".into(),
                ref_columns: vec!["id".into()],
                on_update: None,
                on_delete: None,
                definition: None,
            }],
            constraints: vec![Constraint {
                name: "pk".into(),
                kind: ConstraintKind::PrimaryKey,
                columns: vec!["id".into()],
                definition: None,
            }],
            triggers: vec![],
            comment: None,
            partition_key: None,
            view_definition: None,
            native_ddl: None,
        }
    }

    fn page(ids: std::ops::RangeInclusive<u32>, has_more: bool) -> Page {
        Page {
            rows: RowSet {
                columns: vec![
                    ResultColumn::new("id", Some("integer".into())),
                    ResultColumn::new("org_id", Some("integer".into())),
                    ResultColumn::new("note", Some("text".into())),
                ],
                rows: ids
                    .map(|i| {
                        vec![
                            Cell::Text(i.to_string()),
                            Cell::Text("7".into()),
                            if i % 2 == 0 { Cell::Null } else { Cell::Text(String::new()) },
                        ]
                    })
                    .collect(),
            },
            has_more,
            elapsed: Duration::from_millis(3),
        }
    }

    fn grid() -> Grid {
        let mut grid = Grid::new(
            source(),
            ObjectRef::new("db_prod", "public", "users"),
            None,
            Keymap::new(&BTreeMap::new()),
            3,
            Some(1200),
        );
        let effects = grid.update(Input::Msg(Msg::Detail(Ok(detail()))));
        assert!(
            matches!(effects.as_slice(), [Effect::Fetch(1, r)] if r.position == Position::First && r.primary_key == vec!["id"])
        );
        grid.update(Input::Msg(Msg::Page(1, Position::First, Ok(page(1..=3, true)))));
        grid
    }

    fn key(code: KeyCode) -> Input<Msg> {
        Input::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn keyset_navigation() {
        let mut grid = grid();
        let effects = grid.update(key(KeyCode::Char(']')));
        let [Effect::Fetch(2, request)] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!(request.position, Position::After(vec!["3".into()]));
        grid.update(Input::Msg(Msg::Page(2, Position::After(vec!["3".into()]), Ok(page(4..=6, false)))));
        assert_eq!(grid.page_index, Some(1));
        assert!(grid.update(key(KeyCode::Char(']'))).is_empty(), "no page after the last");
        let effects = grid.update(key(KeyCode::Char('[')));
        assert!(
            matches!(effects.as_slice(), [Effect::Fetch(3, r)] if r.position == Position::Before(vec!["4".into()]))
        );
        // A stale page is ignored.
        grid.update(Input::Msg(Msg::Page(2, Position::First, Ok(page(1..=1, false)))));
        assert_eq!(grid.table.rows.len(), 3);
    }

    #[test]
    fn sort_on_other_column_uses_offset() {
        let mut grid = grid();
        grid.update(key(KeyCode::Char('l')));
        let effects = grid.update(key(KeyCode::Char('s')));
        let [Effect::Fetch(_, request)] = effects.as_slice() else { panic!() };
        assert!(!request.keyset_applicable());
        assert_eq!(request.sort, Some(Sort { column: "org_id".into(), dir: SortDir::Asc }));
    }

    #[test]
    fn foreign_key_navigation_builds_a_filter() {
        let mut grid = grid();
        grid.update(key(KeyCode::Char('l')));
        let effects = grid.update(Input::Key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT)));
        let [Effect::Open(request)] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!(request.object.as_ref().unwrap().name, "orgs");
        assert_eq!(request.filter.as_deref(), Some("\"id\" = '7'"));
    }

    #[test]
    fn filter_and_copy() {
        let mut grid = grid();
        grid.update(key(KeyCode::Char('f')));
        for c in "id > 2".chars() {
            grid.update(key(KeyCode::Char(c)));
        }
        let effects = grid.update(key(KeyCode::Enter));
        assert!(matches!(effects.as_slice(), [Effect::Fetch(_, r)] if r.filter.as_deref() == Some("id > 2")));
        let effects = grid.update(Input::Key(KeyEvent::new(KeyCode::Char('Y'), KeyModifiers::SHIFT)));
        assert!(matches!(effects.as_slice(), [Effect::Copy(t, _)] if t == "id,org_id,note\n1,7,\n"));
    }

    #[test]
    fn thousands() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(300_000), "300\u{202f}000");
        assert_eq!(group_thousands(1_234_567), "1\u{202f}234\u{202f}567");
    }

    #[test]
    fn renders_banner_and_rows() {
        let mut grid = grid();
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal.draw(|f| grid.view(f)).unwrap();
        insta::assert_snapshot!(terminal.backend());
    }
}
