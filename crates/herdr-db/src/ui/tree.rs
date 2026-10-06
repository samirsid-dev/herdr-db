//! Database tree: the single entry point. It only displays the introspected
//! model read from the cache, never the live database. Everything starts
//! from a selected node, on the keyboard.
//!
//! Hierarchy: folder → data source → schema (MySQL database) → Tables /
//! Views → object → Columns / Keys / Foreign keys / Indexes / Checks /
//! Triggers → item.

use super::keys::Keymap;
use super::theme::{self, Icons, env_color};
use super::widgets::{self, InputOutcome, Link, Status, TextInput};
use super::{Input, Perform, Program, Sender};
use crate::db::{DbError, DbHandle, DbStatus};
use crate::introspect::{self, IntrospectError};
use crate::paths::HerdrEnv;
use crate::settings::{self, NewSource};
use crate::update::{self, BinaryStamp};
use crate::views::{Opener, Origin};
use crate::{clipboard, secrets};
use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind};
use herdr_db_core::config::{Config, Environment, SourceConfig, valid_id};
use herdr_db_core::model::{Engine, ObjectKind, ObjectRef, TableDetail};
use herdr_db_core::request::{Action, PaneRequest};
use herdr_db_core::sql::{display_ident, qualified};
use herdr_db_store::{Cache, CachedObject};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use secrecy::SecretString;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

/// The tree connects to introspect, then closes after this idle time.
const IDLE: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Group {
    Tables,
    Views,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DetailGroup {
    Columns,
    Keys,
    ForeignKeys,
    Indexes,
    Checks,
    Triggers,
}

impl DetailGroup {
    const ALL: [DetailGroup; 6] = [
        DetailGroup::Columns,
        DetailGroup::Keys,
        DetailGroup::ForeignKeys,
        DetailGroup::Indexes,
        DetailGroup::Checks,
        DetailGroup::Triggers,
    ];

    fn label(self) -> &'static str {
        match self {
            DetailGroup::Columns => "colonnes",
            DetailGroup::Keys => "clés",
            DetailGroup::ForeignKeys => "clés étrangères",
            DetailGroup::Indexes => "index",
            DetailGroup::Checks => "checks",
            DetailGroup::Triggers => "triggers",
        }
    }

    fn len(self, detail: &TableDetail) -> usize {
        match self {
            DetailGroup::Columns => detail.columns.len(),
            DetailGroup::Keys => detail.keys().count(),
            DetailGroup::ForeignKeys => detail.foreign_keys.len(),
            DetailGroup::Indexes => detail.indexes.len(),
            DetailGroup::Checks => detail.checks().count(),
            DetailGroup::Triggers => detail.triggers.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum NodeId {
    Folder(String),
    Source(String),
    Schema(String, String),
    Group(String, String, Group),
    Object(ObjectRef),
    Detail(ObjectRef, DetailGroup),
    Item(ObjectRef, DetailGroup, usize),
    Placeholder(Box<NodeId>),
}

impl NodeId {
    fn source(&self) -> Option<&str> {
        match self {
            NodeId::Folder(_) => None,
            NodeId::Source(s) | NodeId::Schema(s, _) | NodeId::Group(s, _, _) => Some(s),
            NodeId::Object(o) | NodeId::Detail(o, _) | NodeId::Item(o, _, _) => Some(&o.source),
            NodeId::Placeholder(parent) => parent.source(),
        }
    }

    fn schema(&self) -> Option<&str> {
        match self {
            NodeId::Schema(_, s) | NodeId::Group(_, s, _) => Some(s),
            NodeId::Object(o) | NodeId::Detail(o, _) | NodeId::Item(o, _, _) => Some(&o.schema),
            NodeId::Placeholder(parent) => parent.schema(),
            _ => None,
        }
    }

    fn object(&self) -> Option<&ObjectRef> {
        match self {
            NodeId::Object(o) | NodeId::Detail(o, _) | NodeId::Item(o, _, _) => Some(o),
            NodeId::Placeholder(parent) => parent.object(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SchemaNode {
    pub name: String,
    pub introspected_at: DateTime<Utc>,
    pub objects: Vec<CachedObject>,
}

/// What the cache holds for a source, as loaded by the executor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub schemas: Vec<SchemaNode>,
    pub available: Option<Vec<String>>,
    pub details: HashMap<ObjectRef, TableDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Busy {
    Introspecting,
    Waiting,
}

#[derive(Debug, Clone)]
struct SourceNode {
    config: SourceConfig,
    snapshot: Snapshot,
    loaded: bool,
    /// Automatic introspection already tried (avoids loops after an error).
    attempted: bool,
    busy: Option<Busy>,
    error: Option<String>,
    link: Link,
}

impl SourceNode {
    fn new(config: SourceConfig) -> SourceNode {
        SourceNode {
            config,
            snapshot: Snapshot::default(),
            loaded: false,
            attempted: false,
            busy: None,
            error: None,
            link: Link::Idle,
        }
    }

    fn missing_schemas(&self) -> bool {
        self.config.schemas.iter().any(|s| !self.snapshot.schemas.iter().any(|n| &n.name == s))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Row {
    id: NodeId,
    depth: usize,
    expandable: bool,
    name: String,
    /// Spans before the name (icon, badges), the name style, text on the right.
    prefix: Vec<Span<'static>>,
    name_style: Style,
    right: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Source,
    Schema(String),
    Table(ObjectRef),
}

#[derive(Debug, Clone)]
struct SchemaPicker {
    source: String,
    items: Vec<(String, bool)>,
    cursor: usize,
}

#[derive(Debug, Clone)]
struct PasswordPrompt {
    source: String,
    user: String,
    rejected: bool,
    input: TextInput,
    save: bool,
    retry: (Scope, bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Choice(&'static [&'static str]),
    Secret,
}

#[derive(Debug, Clone)]
struct FormField {
    label: &'static str,
    kind: FieldKind,
    input: TextInput,
    choice: usize,
}

#[derive(Debug, Clone)]
struct SourceForm {
    fields: Vec<FormField>,
    focus: usize,
    error: Option<String>,
}

const ENGINES: &[&str] = &["postgres", "mysql"];
const ENVIRONMENTS: &[&str] = &["local", "development", "staging", "production"];
const READ_ONLY: &[&str] = &["auto", "oui", "non"];

impl SourceForm {
    fn new(folder: Option<&str>) -> SourceForm {
        let text =
            |label, value: &str| FormField { label, kind: FieldKind::Text, input: TextInput::new(value), choice: 0 };
        let choice = |label, options| FormField {
            label,
            kind: FieldKind::Choice(options),
            input: TextInput::default(),
            choice: 0,
        };
        SourceForm {
            fields: vec![
                text("id", ""),
                text("dossier", folder.unwrap_or("")),
                choice("moteur", ENGINES),
                choice("environnement", ENVIRONMENTS),
                text("hôte", "localhost"),
                text("port", ""),
                text("base", ""),
                text("utilisateur", ""),
                text("schémas", ""),
                choice("lecture seule", READ_ONLY),
                text("pré-connexion", ""),
                text("password_command", ""),
                FormField { label: "mot de passe", kind: FieldKind::Secret, input: TextInput::masked(), choice: 0 },
            ],
            focus: 0,
            error: None,
        }
    }

    fn value(&self, label: &str) -> String {
        let field = self.fields.iter().find(|f| f.label == label).expect("known field");
        match field.kind {
            FieldKind::Choice(options) => options[field.choice].to_string(),
            _ => field.input.text.trim().to_string(),
        }
    }

    /// Validates and builds the entry; the password goes to the keyring.
    fn build(&self, config: &Config) -> Result<(NewSource, Option<SecretString>), String> {
        let id = self.value("id");
        if !valid_id(&id) {
            return Err("id invalide : lettres, chiffres, `-`, `_` ou `.`".into());
        }
        if config.source(&id).is_some() {
            return Err(format!("la source `{id}` existe déjà"));
        }
        let database = self.value("base");
        if database.is_empty() {
            return Err("la base est obligatoire".into());
        }
        let port = match self.value("port") {
            p if p.is_empty() => None,
            p => Some(p.parse::<u16>().map_err(|_| "port invalide".to_string())?),
        };
        let optional = |v: String| Some(v).filter(|v| !v.is_empty());
        let command = |v: String| Some(settings::split_command(&v)).filter(|c| !c.is_empty());
        let password = self
            .fields
            .iter()
            .find(|f| f.kind == FieldKind::Secret)
            .map(|f| f.input.text.clone())
            .filter(|p| !p.is_empty())
            .map(SecretString::from);
        let source = NewSource {
            id,
            folder: optional(self.value("dossier")),
            engine: self.value("moteur"),
            environment: self.value("environnement"),
            host: optional(self.value("hôte")).unwrap_or_else(|| "localhost".into()),
            port,
            database,
            user: optional(self.value("utilisateur")),
            schemas: self
                .value("schémas")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect(),
            read_only: match self.value("lecture seule").as_str() {
                "oui" => Some(true),
                "non" => Some(false),
                _ => None,
            },
            pre_connect: command(self.value("pré-connexion")),
            password_command: command(self.value("password_command")),
        };
        Ok((source, password))
    }
}

#[derive(Debug, Clone)]
enum Popup {
    Help,
    Schemas(SchemaPicker),
    Password(PasswordPrompt),
    NewSource(Box<SourceForm>),
}

pub struct Tree {
    config: Config,
    icons: Icons,
    keys: Keymap,
    sources: Vec<SourceNode>,
    expanded: HashSet<NodeId>,
    rows: Vec<Row>,
    selected: usize,
    scroll: usize,
    search: Option<TextInput>,
    popup: Option<Popup>,
    status: Status,
    tick: usize,
    now: DateTime<Utc>,
    update_available: Option<String>,
    binary_replaced: bool,
    quit: bool,
    list_area: Rect,
    last_click: Option<(usize, std::time::Instant)>,
    /// Refresh asked by the user: its completion is announced.
    announced: Option<Scope>,
}

#[derive(Debug)]
pub enum Msg {
    CacheLoaded(String, Result<Snapshot, String>),
    Waiting(String),
    Introspected(String, Scope, Result<(), IntrospectError>),
    Available(String, Result<Vec<String>, IntrospectError>),
    Link(String, DbStatus),
    CacheChanged(Vec<String>),
    Opened(Result<(), String>),
    Copied(bool, String),
    ConfigReloaded(Result<Config, String>, String),
    UpdateAvailable(Option<String>),
    BinaryReplaced,
}

#[derive(Debug)]
pub enum Effect {
    LoadCache(SourceConfig, Vec<ObjectRef>),
    Introspect(SourceConfig, Scope, bool),
    LoadAvailable(SourceConfig),
    SaveSchemas(String, Vec<String>),
    AddSource(NewSource, Option<SecretString>),
    ProvidePassword(SourceConfig, SecretString, bool),
    Open(PaneRequest),
    Copy(String),
    PollCache,
    CheckUpdate,
    CheckBinary,
}

impl Tree {
    pub fn new(config: Config, icons: Icons, keys: Keymap) -> Tree {
        let sources = config.sources.iter().cloned().map(SourceNode::new).collect();
        let mut expanded = HashSet::new();
        for folder in &config.folders {
            expanded.insert(NodeId::Folder(folder.clone()));
        }
        let mut tree = Tree {
            config,
            icons,
            keys,
            sources,
            expanded,
            rows: Vec::new(),
            selected: 0,
            scroll: 0,
            search: None,
            popup: None,
            status: Status::default(),
            tick: 0,
            now: Utc::now(),
            update_available: None,
            binary_replaced: false,
            quit: false,
            list_area: Rect::default(),
            last_click: None,
            announced: None,
        };
        tree.rebuild();
        tree
    }

    fn source(&self, id: &str) -> Option<&SourceNode> {
        self.sources.iter().find(|s| s.config.id == id)
    }

    fn source_mut(&mut self, id: &str) -> Option<&mut SourceNode> {
        self.sources.iter_mut().find(|s| s.config.id == id)
    }

    fn selected_id(&self) -> Option<&NodeId> {
        self.rows.get(self.selected).map(|r| &r.id)
    }

    // ---------------------------------------------------------------- rows

    fn rebuild(&mut self) {
        let previous = self.selected_id().cloned();
        self.rows = match &self.search {
            Some(input) if !input.text.is_empty() => self.filtered_rows(&input.text),
            _ => self.build_rows(false),
        };
        if let Some(id) = previous
            && let Some(index) = self.rows.iter().position(|r| r.id == id)
        {
            self.selected = index;
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn is_open(&self, id: &NodeId, all: bool) -> bool {
        all && !matches!(id, NodeId::Object(_) | NodeId::Detail(..)) || self.expanded.contains(id)
    }

    /// `all`: expand every loaded level down to objects (speed search).
    fn build_rows(&self, all: bool) -> Vec<Row> {
        let mut rows = Vec::new();
        let in_folder: HashSet<&str> = self.sources.iter().filter_map(|s| s.config.folder.as_deref()).collect();
        for folder in self.config.folders.iter().filter(|f| in_folder.contains(f.as_str())) {
            let id = NodeId::Folder(folder.clone());
            let open = self.is_open(&id, all);
            rows.push(Row {
                id,
                depth: 0,
                expandable: true,
                name: folder.clone(),
                prefix: vec![Span::styled(format!("{} ", self.icons.folder(open)), Style::new().fg(Color::Yellow))],
                name_style: Style::new().add_modifier(Modifier::BOLD),
                right: String::new(),
            });
            if open {
                for source in self.sources.iter().filter(|s| s.config.folder.as_deref() == Some(folder)) {
                    self.push_source(&mut rows, source, 1, all);
                }
            }
        }
        for source in self.sources.iter().filter(|s| s.config.folder.is_none()) {
            self.push_source(&mut rows, source, 0, all);
        }
        rows
    }

    fn push_source(&self, rows: &mut Vec<Row>, source: &SourceNode, depth: usize, all: bool) {
        let config = &source.config;
        let id = NodeId::Source(config.id.clone());
        let open = self.is_open(&id, all && source.loaded);
        let color = env_color(config.environment);
        let mut right = match (&source.busy, &source.link) {
            (Some(Busy::Waiting), _) => format!("{} introspection en cours", self.icons.spinner(self.tick)),
            (Some(Busy::Introspecting), _) => format!("{} introspection", self.icons.spinner(self.tick)),
            (None, Link::Tunnel | Link::Connecting) => {
                format!("{} {}", self.icons.spinner(self.tick), source.link.label())
            }
            _ => String::new(),
        };
        if right.is_empty() && source.error.is_some() {
            right = "erreur".into();
        }
        if right.is_empty() {
            right = format!("{} · {}", config.engine.label(), short_env(config.environment));
        }
        rows.push(Row {
            id: id.clone(),
            depth,
            expandable: true,
            name: config.label().to_string(),
            prefix: vec![Span::styled(format!("{} ", self.icons.source()), Style::new().fg(color))],
            name_style: Style::new().fg(color).add_modifier(Modifier::BOLD),
            right,
        });
        if !open {
            return;
        }
        let placeholder = |text: &str| Row {
            id: NodeId::Placeholder(Box::new(id.clone())),
            depth: depth + 1,
            expandable: false,
            name: text.to_string(),
            prefix: Vec::new(),
            name_style: theme::dim(),
            right: String::new(),
        };
        if let Some(error) = &source.error {
            rows.push(Row { name_style: theme::error(), ..placeholder(error) });
        }
        if !source.loaded {
            rows.push(placeholder("chargement…"));
            return;
        }
        if source.snapshot.schemas.is_empty() {
            if source.busy.is_some() {
                rows.push(placeholder("introspection…"));
            } else if source.error.is_none() {
                rows.push(placeholder("non introspecté : r pour rafraîchir"));
            }
            return;
        }
        for schema_name in &config.schemas {
            let Some(schema) = source.snapshot.schemas.iter().find(|s| &s.name == schema_name) else {
                continue;
            };
            self.push_schema(rows, source, schema, depth + 1, all);
        }
    }

    fn push_schema(&self, rows: &mut Vec<Row>, source: &SourceNode, schema: &SchemaNode, depth: usize, all: bool) {
        let source_id = &source.config.id;
        let id = NodeId::Schema(source_id.clone(), schema.name.clone());
        let open = self.is_open(&id, all);
        rows.push(Row {
            id,
            depth,
            expandable: true,
            name: schema.name.clone(),
            prefix: vec![Span::styled(format!("{} ", self.icons.schema()), Style::new().fg(Color::Magenta))],
            name_style: Style::new(),
            right: widgets::age(schema.introspected_at, self.now),
        });
        if !open {
            return;
        }
        for group in [Group::Tables, Group::Views] {
            let objects: Vec<&CachedObject> =
                schema.objects.iter().filter(|o| o.summary.kind.is_view() == (group == Group::Views)).collect();
            if objects.is_empty() && group == Group::Views {
                continue;
            }
            let id = NodeId::Group(source_id.clone(), schema.name.clone(), group);
            let open = self.is_open(&id, all);
            rows.push(Row {
                id,
                depth: depth + 1,
                expandable: !objects.is_empty(),
                name: if group == Group::Tables { "tables".into() } else { "vues".into() },
                prefix: vec![Span::styled(format!("{} ", self.icons.group()), theme::dim())],
                name_style: Style::new(),
                right: objects.len().to_string(),
            });
            if !open {
                continue;
            }
            for object in objects {
                let object_ref = ObjectRef::new(source_id.clone(), schema.name.clone(), object.summary.name.clone());
                self.push_object(rows, source, object_ref, object, depth + 2);
            }
        }
    }

    fn push_object(
        &self,
        rows: &mut Vec<Row>,
        source: &SourceNode,
        object: ObjectRef,
        cached: &CachedObject,
        depth: usize,
    ) {
        let id = NodeId::Object(object.clone());
        let open = self.expanded.contains(&id);
        let kind = cached.summary.kind;
        let color = if kind.is_view() { Color::LightGreen } else { Color::LightBlue };
        let right = match kind {
            ObjectKind::Table | ObjectKind::PartitionedTable | ObjectKind::MaterializedView => {
                cached.summary.estimated_rows.map(|n| format!("~{}", compact(n))).unwrap_or_default()
            }
            _ => kind.label().to_string(),
        };
        rows.push(Row {
            id: id.clone(),
            depth,
            expandable: true,
            name: object.name.clone(),
            prefix: vec![Span::styled(format!("{} ", self.icons.object(kind)), Style::new().fg(color))],
            name_style: Style::new(),
            right,
        });
        if !open {
            return;
        }
        let Some(detail) = source.snapshot.details.get(&object) else {
            rows.push(Row {
                id: NodeId::Placeholder(Box::new(id)),
                depth: depth + 1,
                expandable: false,
                name: "chargement…".into(),
                prefix: Vec::new(),
                name_style: theme::dim(),
                right: String::new(),
            });
            return;
        };
        for group in DetailGroup::ALL {
            let count = group.len(detail);
            if count == 0 {
                continue;
            }
            let group_id = NodeId::Detail(object.clone(), group);
            let group_open = self.expanded.contains(&group_id);
            rows.push(Row {
                id: group_id,
                depth: depth + 1,
                expandable: true,
                name: group.label().to_string(),
                prefix: vec![Span::styled(format!("{} ", self.icons.group()), theme::dim())],
                name_style: Style::new(),
                right: count.to_string(),
            });
            if group_open {
                self.push_items(rows, &object, detail, group, depth + 2);
            }
        }
    }

    fn push_items(
        &self,
        rows: &mut Vec<Row>,
        object: &ObjectRef,
        detail: &TableDetail,
        group: DetailGroup,
        depth: usize,
    ) {
        let item = |index: usize, name: String, prefix: Vec<Span<'static>>, right: String| Row {
            id: NodeId::Item(object.clone(), group, index),
            depth,
            expandable: false,
            name,
            prefix,
            name_style: Style::new(),
            right,
        };
        let icon = |text: &'static str, color: Color| vec![Span::styled(format!("{text} "), Style::new().fg(color))];
        match group {
            DetailGroup::Columns => {
                // Badges combine; names stay aligned after the widest set.
                let prefixes: Vec<Vec<Span<'static>>> = detail
                    .columns
                    .iter()
                    .map(|c| {
                        let badges = self.icons.badges(detail.badges(&c.name));
                        if badges.is_empty() { vec![Span::styled(self.icons.column(), theme::dim())] } else { badges }
                    })
                    .collect();
                let width = |p: &Vec<Span<'static>>| p.iter().map(|s| s.content.width()).sum::<usize>();
                let widest = prefixes.iter().map(width).max().unwrap_or(0);
                for ((i, column), mut prefix) in detail.columns.iter().enumerate().zip(prefixes) {
                    let pad = widest - width(&prefix) + 1;
                    prefix.push(Span::raw(" ".repeat(pad)));
                    rows.push(item(i, column.name.clone(), prefix, column.data_type.clone()));
                }
            }
            DetailGroup::Keys => {
                for (i, key) in detail.keys().enumerate() {
                    rows.push(item(
                        i,
                        key.name.clone(),
                        icon(self.icons.key(), theme::GOLD),
                        format!("{} ({})", key.kind.label(), key.columns.join(", ")),
                    ));
                }
            }
            DetailGroup::ForeignKeys => {
                for (i, fk) in detail.foreign_keys.iter().enumerate() {
                    rows.push(item(
                        i,
                        fk.name.clone(),
                        icon(self.icons.key(), theme::FK_BLUE),
                        format!("({}) → {}.{}", fk.columns.join(", "), fk.ref_schema, fk.ref_table),
                    ));
                }
            }
            DetailGroup::Indexes => {
                for (i, index) in detail.indexes.iter().enumerate() {
                    let unique = if index.unique { "unique " } else { "" };
                    rows.push(item(
                        i,
                        index.name.clone(),
                        icon(self.icons.index(), Color::Cyan),
                        format!("{unique}({})", index.columns.join(", ")),
                    ));
                }
            }
            DetailGroup::Checks => {
                for (i, check) in detail.checks().enumerate() {
                    rows.push(item(
                        i,
                        check.name.clone(),
                        icon(self.icons.check(), Color::Green),
                        check.definition.clone().unwrap_or_default(),
                    ));
                }
            }
            DetailGroup::Triggers => {
                for (i, trigger) in detail.triggers.iter().enumerate() {
                    rows.push(item(
                        i,
                        trigger.name.clone(),
                        icon(self.icons.trigger(), Color::Magenta),
                        format!("{} {}", trigger.timing, trigger.events.join(" OR ")),
                    ));
                }
            }
        }
    }

    /// Speed search: matching nodes plus their ancestors.
    fn filtered_rows(&self, query: &str) -> Vec<Row> {
        let query = query.to_lowercase();
        let all = self.build_rows(true);
        let mut keep = vec![false; all.len()];
        let mut ancestors: Vec<usize> = Vec::new();
        for (i, row) in all.iter().enumerate() {
            while ancestors.last().is_some_and(|&a| all[a].depth >= row.depth) {
                ancestors.pop();
            }
            if !matches!(row.id, NodeId::Placeholder(_)) && row.name.to_lowercase().contains(&query) {
                keep[i] = true;
                for &a in &ancestors {
                    keep[a] = true;
                }
            }
            ancestors.push(i);
        }
        all.into_iter().zip(keep).filter(|(_, k)| *k).map(|(r, _)| r).collect()
    }

    // -------------------------------------------------------------- update

    fn object_kind(&self, object: &ObjectRef) -> Option<ObjectKind> {
        self.source(&object.source)?
            .snapshot
            .schemas
            .iter()
            .find(|s| s.name == object.schema)?
            .objects
            .iter()
            .find(|o| o.summary.name == object.name)
            .map(|o| o.summary.kind)
    }

    fn expanded_objects(&self, source: &str) -> Vec<ObjectRef> {
        self.expanded
            .iter()
            .filter_map(|id| match id {
                NodeId::Object(o) if o.source == source => Some(o.clone()),
                _ => None,
            })
            .collect()
    }

    fn load_cache(&self, source: &str) -> Vec<Effect> {
        match self.source(source) {
            Some(node) => vec![Effect::LoadCache(node.config.clone(), self.expanded_objects(source))],
            None => Vec::new(),
        }
    }

    fn introspect(&mut self, source: &str, scope: Scope, force: bool) -> Vec<Effect> {
        let Some(node) = self.source_mut(source) else {
            return Vec::new();
        };
        if node.busy.is_some() {
            return Vec::new();
        }
        node.busy = Some(Busy::Introspecting);
        node.attempted = true;
        node.error = None;
        let config = node.config.clone();
        self.rebuild();
        vec![Effect::Introspect(config, scope, force)]
    }

    fn expand(&mut self, id: NodeId) -> Vec<Effect> {
        if !self.expanded.insert(id.clone()) {
            return Vec::new();
        }
        let mut effects = Vec::new();
        match &id {
            NodeId::Source(source) => {
                if let Some(node) = self.source(source) {
                    if !node.loaded {
                        effects.extend(self.load_cache(source));
                    } else if node.missing_schemas() && !node.attempted {
                        effects.extend(self.introspect(&source.clone(), Scope::Source, false));
                    }
                }
            }
            NodeId::Object(object) => {
                let has_detail = self.source(&object.source).is_some_and(|s| s.snapshot.details.contains_key(object));
                if !has_detail {
                    effects.extend(self.load_cache(&object.source));
                }
            }
            _ => {}
        }
        self.rebuild();
        effects
    }

    fn collapse(&mut self, id: &NodeId) {
        self.expanded.remove(id);
        self.rebuild();
    }

    fn parent_index(&self) -> Option<usize> {
        let depth = self.rows.get(self.selected)?.depth;
        (0..self.selected).rev().find(|&i| self.rows[i].depth < depth)
    }

    fn open_request(&mut self, action: Action) -> Vec<Effect> {
        let Some(id) = self.selected_id().cloned() else {
            return Vec::new();
        };
        let request = match action {
            Action::Console => {
                let Some(source) = id.source() else {
                    self.status = Status::warning("sélectionnez une data source");
                    return Vec::new();
                };
                let mut request = PaneRequest::new(Action::Console);
                request.source = Some(source.to_string());
                request.schema = id.schema().map(str::to_string);
                request
            }
            _ => {
                let Some(object) = id.object().cloned() else {
                    self.status = Status::warning("sélectionnez une table ou une vue");
                    return Vec::new();
                };
                let mut request = PaneRequest::for_object(action, object.clone());
                if action == Action::QuickDoc
                    && let NodeId::Item(o, DetailGroup::Columns, index) = &id
                {
                    request.column = self
                        .source(&o.source)
                        .and_then(|s| s.snapshot.details.get(o))
                        .and_then(|d| d.columns.get(*index))
                        .map(|c| c.name.clone());
                }
                request
            }
        };
        self.status = Status::info(match action {
            Action::EditData => "ouverture des données…",
            Action::GoToDdl => "ouverture du DDL…",
            Action::QuickDoc => "ouverture de la documentation…",
            Action::Console => "ouverture de la console…",
        });
        vec![Effect::Open(request)]
    }

    /// Qualified name copied by Copy Reference.
    fn reference(&self, id: &NodeId) -> Option<String> {
        let engine = |source: &str| self.source(source).map_or(Engine::Postgres, |s| s.config.engine);
        Some(match id {
            NodeId::Folder(name) => name.clone(),
            NodeId::Source(source) => source.clone(),
            NodeId::Schema(source, schema) | NodeId::Group(source, schema, _) => display_ident(engine(source), schema),
            NodeId::Object(o) | NodeId::Detail(o, _) => qualified(engine(&o.source), &o.schema, &o.name),
            NodeId::Item(o, group, index) => {
                let engine = engine(&o.source);
                let base = qualified(engine, &o.schema, &o.name);
                let detail = self.source(&o.source)?.snapshot.details.get(o)?;
                let name = match group {
                    DetailGroup::Columns => detail.columns.get(*index)?.name.clone(),
                    DetailGroup::Keys => detail.keys().nth(*index)?.name.clone(),
                    DetailGroup::ForeignKeys => detail.foreign_keys.get(*index)?.name.clone(),
                    DetailGroup::Indexes => return Some(display_ident(engine, &detail.indexes.get(*index)?.name)),
                    DetailGroup::Checks => detail.checks().nth(*index)?.name.clone(),
                    DetailGroup::Triggers => detail.triggers.get(*index)?.name.clone(),
                };
                format!("{base}.{}", display_ident(engine, &name))
            }
            NodeId::Placeholder(_) => return None,
        })
    }

    fn refresh(&mut self, force: bool) -> Vec<Effect> {
        let Some(id) = self.selected_id().cloned() else {
            return Vec::new();
        };
        let Some(source) = id.source().map(str::to_string) else {
            return Vec::new();
        };
        let scope = if force {
            Scope::Source
        } else {
            match &id {
                NodeId::Schema(_, s) | NodeId::Group(_, s, _) => Scope::Schema(s.clone()),
                NodeId::Object(o) | NodeId::Detail(o, _) | NodeId::Item(o, _, _) => Scope::Table(o.clone()),
                _ => Scope::Source,
            }
        };
        if let NodeId::Source(_) = id {
            self.expanded.insert(id.clone());
        }
        self.announced = Some(scope.clone());
        self.status = Status::info("rafraîchissement…");
        self.introspect(&source, scope, force)
    }

    fn on_key(&mut self, key: KeyEvent) -> Vec<Effect> {
        if let Some(popup) = self.popup.take() {
            return self.on_popup_key(popup, key);
        }
        if let Some(mut input) = self.search.take() {
            match input.handle(&key) {
                InputOutcome::Submit => {
                    // Keep the found node visible once the filter is gone.
                    if let Some(id) = self.selected_id().cloned() {
                        let mut ancestors = Vec::new();
                        let depth = self.rows[self.selected].depth;
                        let mut d = depth;
                        for row in self.rows[..self.selected].iter().rev() {
                            if row.depth < d {
                                ancestors.push(row.id.clone());
                                d = row.depth;
                            }
                        }
                        self.expanded.extend(ancestors);
                        self.rebuild();
                        if let Some(i) = self.rows.iter().position(|r| r.id == id) {
                            self.selected = i;
                        }
                    }
                    return Vec::new();
                }
                InputOutcome::Cancel => {
                    self.rebuild();
                    return Vec::new();
                }
                InputOutcome::Changed => {
                    self.search = Some(input);
                    self.selected = 0;
                    self.rebuild();
                    // Select the first actual match.
                    if let Some(query) = self.search.as_ref().map(|i| i.text.to_lowercase())
                        && let Some(i) = self.rows.iter().position(|r| r.name.to_lowercase().contains(&query))
                    {
                        self.selected = i;
                    }
                    return Vec::new();
                }
                InputOutcome::Ignored => {
                    self.search = Some(input);
                    // Arrows still move in the filtered list.
                    match key.code {
                        KeyCode::Up => self.selected = self.selected.saturating_sub(1),
                        KeyCode::Down => self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1)),
                        _ => {}
                    }
                    return Vec::new();
                }
            }
        }

        let actions = [
            "up",
            "down",
            "top",
            "bottom",
            "page_up",
            "page_down",
            "expand",
            "collapse",
            "search",
            "edit_data",
            "ddl",
            "quickdoc",
            "console",
            "copy_reference",
            "refresh",
            "force_refresh",
            "schemas",
            "new_source",
            "help",
            "quit",
        ];
        let Some(action) = self.keys.find(&actions, &key) else {
            return Vec::new();
        };
        let id = self.selected_id().cloned();
        let row = self.rows.get(self.selected).cloned();
        match action {
            "up" => self.selected = self.selected.saturating_sub(1),
            "down" => self.selected = (self.selected + 1).min(self.rows.len().saturating_sub(1)),
            "top" => self.selected = 0,
            "bottom" => self.selected = self.rows.len().saturating_sub(1),
            "page_up" => self.selected = self.selected.saturating_sub(self.list_area.height.max(1) as usize),
            "page_down" => {
                self.selected =
                    (self.selected + self.list_area.height.max(1) as usize).min(self.rows.len().saturating_sub(1))
            }
            "expand" => {
                let Some(row) = row else { return Vec::new() };
                if !row.expandable {
                    return Vec::new();
                }
                if self.expanded.contains(&row.id) {
                    if key.code == KeyCode::Enter {
                        self.collapse(&row.id);
                    } else if self.selected + 1 < self.rows.len() {
                        self.selected += 1;
                    }
                    return Vec::new();
                }
                return self.expand(row.id);
            }
            "collapse" => {
                if let Some(row) = row
                    && row.expandable
                    && self.expanded.contains(&row.id)
                {
                    self.collapse(&row.id);
                } else if let Some(parent) = self.parent_index() {
                    self.selected = parent;
                }
            }
            "search" => {
                self.search = Some(TextInput::default());
            }
            "edit_data" => return self.open_request(Action::EditData),
            "ddl" => return self.open_request(Action::GoToDdl),
            "quickdoc" => return self.open_request(Action::QuickDoc),
            "console" => return self.open_request(Action::Console),
            "copy_reference" => {
                if let Some(text) = id.as_ref().and_then(|id| self.reference(id)) {
                    return vec![Effect::Copy(text)];
                }
            }
            "refresh" => return self.refresh(false),
            "force_refresh" => return self.refresh(true),
            "schemas" => {
                let Some(source) = id.as_ref().and_then(|i| i.source()).map(str::to_string) else {
                    self.status = Status::warning("sélectionnez une data source");
                    return Vec::new();
                };
                return self.open_schema_picker(&source);
            }
            "new_source" => {
                let folder = match &id {
                    Some(NodeId::Folder(f)) => Some(f.clone()),
                    Some(other) => other.source().and_then(|s| self.source(s)).and_then(|s| s.config.folder.clone()),
                    None => None,
                };
                self.popup = Some(Popup::NewSource(Box::new(SourceForm::new(folder.as_deref()))));
            }
            "help" => self.popup = Some(Popup::Help),
            "quit" => self.quit = true,
            _ => {}
        }
        Vec::new()
    }

    fn open_schema_picker(&mut self, source: &str) -> Vec<Effect> {
        let Some(node) = self.source(source) else {
            return Vec::new();
        };
        match &node.snapshot.available {
            Some(available) => {
                let items = available.iter().map(|s| (s.clone(), node.config.schemas.contains(s))).collect();
                self.popup = Some(Popup::Schemas(SchemaPicker { source: source.to_string(), items, cursor: 0 }));
                Vec::new()
            }
            None => {
                let config = node.config.clone();
                self.status = Status::info("lecture des schémas…");
                vec![Effect::LoadAvailable(config)]
            }
        }
    }

    fn on_popup_key(&mut self, popup: Popup, key: KeyEvent) -> Vec<Effect> {
        match popup {
            Popup::Help => Vec::new(),
            Popup::Schemas(mut picker) => {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => return Vec::new(),
                    KeyCode::Up | KeyCode::Char('k') => picker.cursor = picker.cursor.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => {
                        picker.cursor = (picker.cursor + 1).min(picker.items.len().saturating_sub(1))
                    }
                    KeyCode::Char(' ') | KeyCode::Char('x') => {
                        if let Some(item) = picker.items.get_mut(picker.cursor) {
                            item.1 = !item.1;
                        }
                    }
                    KeyCode::Char('a') => {
                        let all = picker.items.iter().all(|i| i.1);
                        for item in &mut picker.items {
                            item.1 = !all;
                        }
                    }
                    KeyCode::Enter => {
                        let chosen: Vec<String> = picker.items.iter().filter(|i| i.1).map(|i| i.0.clone()).collect();
                        if chosen.is_empty() {
                            self.status = Status::warning("sélectionnez au moins un schéma");
                        } else {
                            self.status = Status::info("enregistrement des schémas…");
                            return vec![Effect::SaveSchemas(picker.source, chosen)];
                        }
                    }
                    _ => {}
                }
                self.popup = Some(Popup::Schemas(picker));
                Vec::new()
            }
            Popup::Password(mut prompt) => {
                if key.code == KeyCode::Tab {
                    prompt.save = !prompt.save;
                    self.popup = Some(Popup::Password(prompt));
                    return Vec::new();
                }
                match prompt.input.handle(&key) {
                    InputOutcome::Submit => {
                        let Some(node) = self.source(&prompt.source) else {
                            return Vec::new();
                        };
                        let config = node.config.clone();
                        let password = SecretString::from(std::mem::take(&mut prompt.input.text));
                        let (scope, force) = prompt.retry;
                        let mut effects = vec![Effect::ProvidePassword(config, password, prompt.save)];
                        effects.extend(self.introspect(&prompt.source, scope, force));
                        effects
                    }
                    InputOutcome::Cancel => {
                        self.status = Status::warning("connexion annulée");
                        Vec::new()
                    }
                    _ => {
                        self.popup = Some(Popup::Password(prompt));
                        Vec::new()
                    }
                }
            }
            Popup::NewSource(mut form) => {
                let field = &mut form.fields[form.focus];
                let save = key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    _ if save => match form.build(&self.config) {
                        Ok((source, password)) => {
                            self.status = Status::info(format!("ajout de {}…", source.id));
                            return vec![Effect::AddSource(source, password)];
                        }
                        Err(e) => form.error = Some(e),
                    },
                    KeyCode::Esc => return Vec::new(),
                    KeyCode::Tab | KeyCode::Down => form.focus = (form.focus + 1) % form.fields.len(),
                    KeyCode::BackTab | KeyCode::Up => {
                        form.focus = (form.focus + form.fields.len() - 1) % form.fields.len()
                    }
                    KeyCode::Enter => {
                        if form.focus + 1 < form.fields.len() {
                            form.focus += 1;
                        } else {
                            match form.build(&self.config) {
                                Ok((source, password)) => {
                                    self.status = Status::info(format!("ajout de {}…", source.id));
                                    return vec![Effect::AddSource(source, password)];
                                }
                                Err(e) => form.error = Some(e),
                            }
                        }
                    }
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                        if matches!(field.kind, FieldKind::Choice(_)) =>
                    {
                        if let FieldKind::Choice(options) = field.kind {
                            field.choice = if key.code == KeyCode::Left {
                                (field.choice + options.len() - 1) % options.len()
                            } else {
                                (field.choice + 1) % options.len()
                            };
                        }
                    }
                    _ if !matches!(field.kind, FieldKind::Choice(_)) => {
                        field.input.handle(&key);
                    }
                    _ => {}
                }
                self.popup = Some(Popup::NewSource(form));
                Vec::new()
            }
        }
    }

    fn on_mouse(&mut self, kind: MouseEventKind, column: u16, row: u16) -> Vec<Effect> {
        let area = self.list_area;
        match kind {
            MouseEventKind::ScrollDown => self.selected = (self.selected + 3).min(self.rows.len().saturating_sub(1)),
            MouseEventKind::ScrollUp => self.selected = self.selected.saturating_sub(3),
            MouseEventKind::Down(MouseButton::Left)
                if self.popup.is_none() && row >= area.y && row < area.y + area.height && column >= area.x =>
            {
                let index = self.scroll + (row - area.y) as usize;
                if index >= self.rows.len() {
                    return Vec::new();
                }
                let now = std::time::Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|(i, at)| i == index && now.duration_since(at) < Duration::from_millis(400));
                self.last_click = Some((index, now));
                let was_selected = self.selected == index;
                self.selected = index;
                let row = self.rows[index].clone();
                if row.expandable && (was_selected || double) {
                    if self.expanded.contains(&row.id) {
                        self.collapse(&row.id);
                    } else {
                        return self.expand(row.id);
                    }
                } else if double && matches!(row.id, NodeId::Object(_)) {
                    return self.open_request(Action::EditData);
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn on_msg(&mut self, msg: Msg) -> Vec<Effect> {
        let mut effects = Vec::new();
        match msg {
            Msg::CacheLoaded(source, result) => {
                let expanded = self.expanded.contains(&NodeId::Source(source.clone()));
                let mut wanted_details = Vec::new();
                if let Some(node) = self.source_mut(&source) {
                    node.loaded = true;
                    match result {
                        Ok(snapshot) => node.snapshot = snapshot,
                        Err(e) => node.error = Some(e),
                    }
                    let needs_introspection = expanded && node.missing_schemas() && !node.attempted;
                    if needs_introspection {
                        effects.extend(self.introspect(&source, Scope::Source, false));
                    }
                }
                // Expanded objects whose details are still missing: introspect them.
                if let Some(node) = self.source(&source) {
                    for object in self.expanded_objects(&source) {
                        if !node.snapshot.details.contains_key(&object) && self.object_kind(&object).is_some() {
                            wanted_details.push(object);
                        }
                    }
                }
                for object in wanted_details {
                    effects.extend(self.introspect(&source, Scope::Table(object), false));
                }
            }
            Msg::Waiting(source) => {
                if let Some(node) = self.source_mut(&source) {
                    node.busy = Some(Busy::Waiting);
                }
            }
            Msg::Introspected(source, scope, result) => {
                if let Some(node) = self.source_mut(&source) {
                    node.busy = None;
                    match result {
                        Ok(()) => {
                            node.error = None;
                            // Automatic detail loads stay silent.
                            if scope == Scope::Source || self.announced.as_ref() == Some(&scope) {
                                self.announced = None;
                                self.status = Status::success(match &scope {
                                    Scope::Source => format!("{source} introspecté"),
                                    Scope::Schema(s) => format!("{s} rafraîchi"),
                                    Scope::Table(o) => format!("{o} rafraîchi"),
                                });
                            }
                        }
                        Err(IntrospectError::Db(DbError::NeedPassword { user, rejected })) => {
                            self.popup = Some(Popup::Password(PasswordPrompt {
                                source: source.clone(),
                                user,
                                rejected,
                                input: TextInput::masked(),
                                save: true,
                                retry: (scope, false),
                            }));
                        }
                        Err(e) => {
                            let text = e.to_string();
                            node.error = Some(text.clone());
                            self.status = Status::error(text);
                        }
                    }
                }
                effects.extend(self.load_cache(&source));
            }
            Msg::Available(source, result) => match result {
                Ok(available) => {
                    if let Some(node) = self.source_mut(&source) {
                        node.snapshot.available = Some(available);
                    }
                    self.status = Status::default();
                    effects.extend(self.open_schema_picker(&source));
                }
                Err(IntrospectError::Db(DbError::NeedPassword { user, rejected })) => {
                    self.popup = Some(Popup::Password(PasswordPrompt {
                        source: source.clone(),
                        user,
                        rejected,
                        input: TextInput::masked(),
                        save: true,
                        retry: (Scope::Source, false),
                    }));
                }
                Err(e) => self.status = Status::error(e.to_string()),
            },
            Msg::Link(source, status) => {
                if let Some(node) = self.source_mut(&source) {
                    node.link = match status {
                        DbStatus::TunnelStarting => Link::Tunnel,
                        DbStatus::Connecting => Link::Connecting,
                        DbStatus::Connected { .. } => Link::Connected,
                        DbStatus::Disconnected => Link::Idle,
                    };
                }
            }
            Msg::CacheChanged(sources) => {
                for source in sources {
                    effects.extend(self.load_cache(&source));
                }
            }
            Msg::Opened(result) => {
                self.status = match result {
                    Ok(()) => Status::default(),
                    Err(e) => Status::error(e),
                }
            }
            Msg::Copied(ok, text) => {
                self.status = if ok {
                    Status::success(format!("copié : {text}"))
                } else {
                    Status::warning(format!("presse-papier indisponible : {text}"))
                }
            }
            Msg::ConfigReloaded(result, source) => match result {
                Ok(config) => {
                    self.apply_config(config);
                    self.status = Status::success(format!("{source} enregistré"));
                    if let Some(node) = self.source_mut(&source) {
                        node.attempted = false;
                        node.loaded = false;
                    }
                    self.expanded.insert(NodeId::Source(source.clone()));
                    if let Some(folder) = self.source(&source).and_then(|s| s.config.folder.clone()) {
                        self.expanded.insert(NodeId::Folder(folder));
                    }
                    effects.extend(self.load_cache(&source));
                }
                Err(e) => self.status = Status::error(e),
            },
            Msg::UpdateAvailable(version) => self.update_available = version,
            Msg::BinaryReplaced => self.binary_replaced = true,
        }
        self.rebuild();
        effects
    }

    fn apply_config(&mut self, config: Config) {
        let mut previous: HashMap<String, SourceNode> =
            self.sources.drain(..).map(|s| (s.config.id.clone(), s)).collect();
        self.sources = config
            .sources
            .iter()
            .map(|c| match previous.remove(&c.id) {
                Some(mut node) => {
                    node.config = c.clone();
                    node
                }
                None => SourceNode::new(c.clone()),
            })
            .collect();
        self.config = config;
    }

    // ---------------------------------------------------------------- view

    fn header(&self) -> Line<'static> {
        let mut spans = vec![Span::styled(" Database", Style::new().add_modifier(Modifier::BOLD))];
        if self.binary_replaced {
            spans.push(Span::styled("  binaire mis à jour : relancez ce pane", Style::new().fg(Color::Yellow)));
        } else if let Some(version) = &self.update_available {
            spans.push(Span::styled(
                format!("  v{version} disponible (action « Mettre à jour Herdr DB »)"),
                Style::new().fg(Color::Yellow),
            ));
        }
        Line::from(spans)
    }

    fn render_rows(&mut self, frame: &mut Frame, area: Rect) {
        let height = area.height as usize;
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if height > 0 && self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(height.max(1)));
        let query = self.search.as_ref().map(|s| s.text.to_lowercase()).filter(|q| !q.is_empty());
        let width = area.width as usize;
        let mut lines = Vec::new();
        for (i, row) in self.rows.iter().enumerate().skip(self.scroll).take(height) {
            let mut spans = vec![Span::raw("  ".repeat(row.depth))];
            let arrow = if !row.expandable {
                "  ".to_string()
            } else if self.expanded.contains(&row.id) || query.is_some() {
                format!("{} ", self.icons.expanded())
            } else {
                format!("{} ", self.icons.collapsed())
            };
            spans.push(Span::styled(arrow, theme::dim()));
            spans.extend(row.prefix.iter().cloned());
            match &query {
                Some(q) => spans.extend(highlight_match(&row.name, q, row.name_style)),
                None => spans.push(Span::styled(row.name.clone(), row.name_style)),
            }
            let used: usize = spans.iter().map(|s| s.content.width()).sum();
            if !row.right.is_empty() && used + 2 < width {
                let room = width - used - 1;
                let right: String = if row.right.width() > room {
                    row.right.chars().take(room.saturating_sub(1)).collect::<String>() + "…"
                } else {
                    row.right.clone()
                };
                spans.push(Span::raw(" ".repeat(width - used - right.width())));
                spans.push(Span::styled(right, theme::dim()));
            }
            let mut line = Line::from(spans);
            if i == self.selected {
                line = line.style(Style::new().bg(Color::Indexed(237)).add_modifier(Modifier::BOLD));
            }
            lines.push(line);
        }
        if self.rows.is_empty() {
            lines.push(Line::styled(
                if self.search.is_some() { " aucun résultat" } else { " aucune data source : n pour en créer une" },
                theme::dim(),
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_popup(&self, frame: &mut Frame) {
        let Some(popup) = &self.popup else { return };
        let area = frame.area();
        match popup {
            Popup::Help => {
                let k = |a: &str| self.keys.label(a);
                let entries = vec![
                    (format!("{} {}", k("up"), k("down")), "naviguer"),
                    (format!("{} {}", k("expand"), k("collapse")), "déplier / replier"),
                    (k("search"), "speed search"),
                    (k("edit_data"), "Edit Data (lecture seule)"),
                    (k("ddl"), "Go to DDL"),
                    (k("quickdoc"), "Quick Documentation"),
                    (k("console"), "console SQL"),
                    (k("copy_reference"), "Copy Reference"),
                    (format!("{} / {}", k("refresh"), k("force_refresh")), "Refresh / Force Refresh"),
                    (k("schemas"), "sélecteur de schémas"),
                    (k("new_source"), "nouvelle data source"),
                    (k("quit"), "fermer l'arbre"),
                ];
                widgets::help_popup(frame, "Raccourcis — Échap pour fermer", &entries);
            }
            Popup::Schemas(picker) => {
                let height = (picker.items.len() as u16 + 3).min(area.height.saturating_sub(2));
                let rect = widgets::centered(area, 44, height);
                let inner = widgets::popup(frame, rect, &format!("Schémas de {}", picker.source));
                let visible = inner.height.saturating_sub(1) as usize;
                let skip = picker.cursor.saturating_sub(visible.saturating_sub(1));
                let mut lines: Vec<Line> = picker
                    .items
                    .iter()
                    .enumerate()
                    .skip(skip)
                    .take(visible)
                    .map(|(i, (name, on))| {
                        let mark = if *on { "[x] " } else { "[ ] " };
                        let style = if i == picker.cursor { theme::selected() } else { Style::new() };
                        Line::styled(format!("{mark}{name}"), style)
                    })
                    .collect();
                lines.push(Line::styled("espace cocher · a tout · Entrée valider", theme::dim()));
                frame.render_widget(Paragraph::new(lines), inner);
            }
            Popup::Password(prompt) => {
                let rect = widgets::centered(area, 52, 6);
                let inner = widgets::popup(frame, rect, &format!("Mot de passe · {}", prompt.source));
                let [message, input, save] =
                    Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Length(1)])
                        .areas(inner);
                let text = if prompt.rejected {
                    format!("Mot de passe refusé pour {}.", prompt.user)
                } else {
                    format!("Le serveur demande un mot de passe pour {}.", prompt.user)
                };
                frame.render_widget(
                    Paragraph::new(text).style(if prompt.rejected { theme::error() } else { Style::new() }),
                    message,
                );
                prompt.input.render(frame, input, "› ", Style::new(), true);
                let mark = if prompt.save { "[x]" } else { "[ ]" };
                frame.render_widget(
                    Paragraph::new(Line::styled(format!("{mark} enregistrer dans le trousseau (Tab)"), theme::dim())),
                    save,
                );
            }
            Popup::NewSource(form) => {
                let height = form.fields.len() as u16 + 4;
                let rect = widgets::centered(area, 64, height);
                let inner = widgets::popup(frame, rect, "Nouvelle data source (fichier personnel)");
                let label_width = 18u16;
                for (i, field) in form.fields.iter().enumerate() {
                    let y = inner.y + i as u16;
                    if y >= inner.y + inner.height {
                        break;
                    }
                    let focused = i == form.focus;
                    let label_style = if focused { Style::new().fg(Color::Cyan) } else { theme::dim() };
                    frame.render_widget(
                        Paragraph::new(Span::styled(format!("{:>17} ", field.label), label_style)),
                        Rect::new(inner.x, y, label_width, 1),
                    );
                    let value_area = Rect::new(inner.x + label_width, y, inner.width.saturating_sub(label_width), 1);
                    match field.kind {
                        FieldKind::Choice(options) => {
                            let text = format!("‹ {} ›", options[field.choice]);
                            let style = if focused { theme::selected() } else { Style::new() };
                            frame.render_widget(Paragraph::new(Span::styled(text, style)), value_area);
                        }
                        _ => field.input.render(frame, value_area, "", Style::new(), focused),
                    }
                }
                let footer_y = inner.y + form.fields.len() as u16 + 1;
                if footer_y < inner.y + inner.height {
                    let footer = match &form.error {
                        Some(e) => Line::styled(e.clone(), theme::error()),
                        None => Line::styled(
                            "Tab/↑↓ champ · ←→ choix · ctrl+s enregistrer · Échap annuler · secret → trousseau",
                            theme::dim(),
                        ),
                    };
                    frame.render_widget(Paragraph::new(footer), Rect::new(inner.x, footer_y, inner.width, 1));
                }
            }
        }
    }
}

fn short_env(environment: Environment) -> &'static str {
    match environment {
        Environment::Local => "local",
        Environment::Development => "dev",
        Environment::Staging => "staging",
        Environment::Production => "prod",
    }
}

fn compact(n: i64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        n if n >= 10_000 => format!("{}k", n / 1000),
        n if n >= 1000 => format!("{:.1}k", n as f64 / 1000.0),
        n => n.to_string(),
    }
}

fn highlight_match(name: &str, query: &str, style: Style) -> Vec<Span<'static>> {
    let lower = name.to_lowercase();
    match lower.find(query) {
        Some(start) if lower.len() == name.len() => {
            let end = start + query.len();
            vec![
                Span::styled(name[..start].to_string(), style),
                Span::styled(name[start..end].to_string(), style.fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                Span::styled(name[end..].to_string(), style),
            ]
        }
        _ => vec![Span::styled(name.to_string(), style)],
    }
}

impl Program for Tree {
    type Msg = Msg;
    type Effect = Effect;

    fn init(&mut self) -> Vec<Effect> {
        let mut effects: Vec<Effect> =
            self.sources.iter().map(|s| Effect::LoadCache(s.config.clone(), Vec::new())).collect();
        if self.config.settings.check_updates {
            effects.push(Effect::CheckUpdate);
        }
        effects
    }

    fn update(&mut self, input: Input<Msg>) -> Vec<Effect> {
        match input {
            Input::Key(key) => {
                if !matches!(self.popup, Some(Popup::Password(_)) | Some(Popup::NewSource(_))) {
                    self.status = Status::default();
                }
                let effects = self.on_key(key);
                self.rebuild();
                effects
            }
            Input::Mouse(mouse) => self.on_mouse(mouse.kind, mouse.column, mouse.row),
            Input::Paste(text) => {
                match &mut self.popup {
                    Some(Popup::Password(prompt)) => prompt.input.insert_str(&text),
                    Some(Popup::NewSource(form)) => {
                        let focus = form.focus;
                        form.fields[focus].input.insert_str(&text)
                    }
                    _ => {
                        if let Some(search) = &mut self.search {
                            search.insert_str(&text);
                            self.rebuild();
                        }
                    }
                }
                Vec::new()
            }
            Input::Resize => Vec::new(),
            Input::Tick => {
                self.tick += 1;
                let mut effects = Vec::new();
                if self.tick.is_multiple_of(4) {
                    self.now = Utc::now();
                    effects.push(Effect::PollCache);
                    self.rebuild();
                } else if self
                    .sources
                    .iter()
                    .any(|s| s.busy.is_some() || matches!(s.link, Link::Tunnel | Link::Connecting))
                {
                    self.rebuild();
                }
                if self.tick.is_multiple_of(120) {
                    effects.push(Effect::CheckBinary);
                }
                effects
            }
            Input::Msg(msg) => self.on_msg(msg),
        }
    }

    fn view(&mut self, frame: &mut Frame) {
        let [header, list, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(frame.area());
        frame.render_widget(Paragraph::new(self.header()), header);
        self.list_area = list;
        self.render_rows(frame, list);
        match &self.search {
            Some(input) => input.render(frame, footer, "/", Style::new().fg(Color::Yellow), self.popup.is_none()),
            None => widgets::status_line(frame, footer, &self.status, &format!("{} aide", self.keys.label("help"))),
        }
        self.render_popup(frame);
    }

    fn quit(&self) -> bool {
        self.quit
    }
}

// ------------------------------------------------------------------ executor

pub struct TreeExec {
    env: Arc<HerdrEnv>,
    opener: Opener,
    handles: HashMap<String, (SourceConfig, DbHandle)>,
    watched: Arc<Mutex<BTreeMap<String, (Cache, i64)>>>,
    stamp: BinaryStamp,
}

impl TreeExec {
    pub fn new(env: Arc<HerdrEnv>, opener: Opener) -> TreeExec {
        TreeExec {
            env,
            opener,
            handles: HashMap::new(),
            watched: Arc::new(Mutex::new(BTreeMap::new())),
            stamp: BinaryStamp::current(),
        }
    }

    fn handle(&mut self, source: &SourceConfig, tx: &Sender<Msg>) -> DbHandle {
        if let Some((config, handle)) = self.handles.get(&source.id)
            && config == source
        {
            return handle.clone();
        }
        let id = source.id.clone();
        let tx = tx.clone();
        let handle = DbHandle::spawn(
            source.clone(),
            self.env.clone(),
            Some(IDLE),
            Box::new(move |status| {
                let _ = tx.send(Msg::Link(id.clone(), status));
            }),
        );
        self.handles.insert(source.id.clone(), (source.clone(), handle.clone()));
        handle
    }
}

fn load_snapshot(
    state_dir: &std::path::Path,
    source: &SourceConfig,
    objects: &[ObjectRef],
) -> Result<(Snapshot, Cache), String> {
    let cache = Cache::open(state_dir, &source.id, source.engine).map_err(|e| e.to_string())?;
    let mut snapshot =
        Snapshot { available: cache.available_schemas().map_err(|e| e.to_string())?, ..Snapshot::default() };
    for info in cache.schemas().map_err(|e| e.to_string())? {
        if !source.schemas.contains(&info.name) {
            continue;
        }
        snapshot.schemas.push(SchemaNode {
            objects: cache.objects(&info.name).map_err(|e| e.to_string())?,
            name: info.name,
            introspected_at: info.introspected_at,
        });
    }
    for object in objects {
        if let Ok(Some((detail, _))) = cache.detail(object) {
            snapshot.details.insert(object.clone(), detail);
        }
    }
    Ok((snapshot, cache))
}

impl Perform<Tree> for TreeExec {
    fn perform(&mut self, effect: Effect, tx: &Sender<Msg>) {
        let tx = tx.clone();
        let state_dir = self.env.state_dir.clone();
        match effect {
            Effect::LoadCache(source, objects) => {
                let watched = self.watched.clone();
                tokio::task::spawn_blocking(move || {
                    let result = load_snapshot(&state_dir, &source, &objects).map(|(snapshot, cache)| {
                        let version = cache.data_version().unwrap_or(0);
                        watched
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .entry(source.id.clone())
                            .or_insert((cache, version));
                        snapshot
                    });
                    let _ = tx.send(Msg::CacheLoaded(source.id.clone(), result));
                });
            }
            Effect::PollCache => {
                let watched = self.watched.clone();
                tokio::task::spawn_blocking(move || {
                    let mut changed = Vec::new();
                    for (source, (cache, version)) in watched.lock().unwrap_or_else(|e| e.into_inner()).iter_mut() {
                        if let Ok(current) = cache.data_version()
                            && current != *version
                        {
                            *version = current;
                            changed.push(source.clone());
                        }
                    }
                    if !changed.is_empty() {
                        let _ = tx.send(Msg::CacheChanged(changed));
                    }
                });
            }
            Effect::Introspect(source, scope, force) => {
                let db = self.handle(&source, &tx);
                tokio::spawn(async move {
                    let id = source.id.clone();
                    let wait_tx = tx.clone();
                    let waiting = move || {
                        let _ = wait_tx.send(Msg::Waiting(id));
                    };
                    let result = match &scope {
                        Scope::Source => {
                            introspect::introspect_source(&db, state_dir, source.clone(), force, waiting).await
                        }
                        Scope::Schema(schema) => {
                            introspect::introspect_schema(
                                &db,
                                state_dir,
                                source.clone(),
                                schema.clone(),
                                force,
                                waiting,
                            )
                            .await
                        }
                        Scope::Table(object) => {
                            introspect::introspect_table(&db, state_dir, source.clone(), object.clone(), waiting)
                                .await
                                .map(|_| ())
                        }
                    };
                    let _ = tx.send(Msg::Introspected(source.id, scope, result));
                });
            }
            Effect::LoadAvailable(source) => {
                let db = self.handle(&source, &tx);
                tokio::spawn(async move {
                    let result = introspect::available_schemas(&db, state_dir, source.clone()).await;
                    let _ = tx.send(Msg::Available(source.id, result));
                });
            }
            Effect::ProvidePassword(source, password, save) => {
                self.handle(&source, &tx).provide_password(password, save);
            }
            Effect::SaveSchemas(source, schemas) => {
                let env = self.env.clone();
                tokio::task::spawn_blocking(move || {
                    let result = settings::set_schemas(&env.personal_config(), &source, &schemas)
                        .and_then(|()| settings::load(&env))
                        .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::ConfigReloaded(result, source));
                });
            }
            Effect::AddSource(source, password) => {
                let env = self.env.clone();
                tokio::task::spawn_blocking(move || {
                    let mut result = settings::add_source(&env.personal_config(), &source)
                        .and_then(|()| settings::load(&env))
                        .map_err(|e| format!("{e:#}"));
                    if let (Ok(config), Some(password)) = (&result, password)
                        && let Some(added) = config.source(&source.id)
                        && let Err(e) = secrets::save_keyring(&added.id, &secrets::user_of(added), &password)
                    {
                        result = Err(format!("source ajoutée, mais le trousseau a refusé le mot de passe : {e:#}"));
                    }
                    let _ = tx.send(Msg::ConfigReloaded(result, source.id));
                });
            }
            Effect::Open(request) => {
                let opener = self.opener.clone();
                tokio::spawn(async move {
                    let result = opener.open(request, Origin::Tree).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Msg::Opened(result));
                });
            }
            Effect::Copy(text) => {
                tokio::task::spawn_blocking(move || {
                    let ok = clipboard::copy(&text);
                    let _ = tx.send(Msg::Copied(ok, text));
                });
            }
            Effect::CheckUpdate => {
                tokio::spawn(async move {
                    let _ = tx.send(Msg::UpdateAvailable(update::newer_release(&state_dir).await));
                });
            }
            Effect::CheckBinary => {
                if self.stamp.replaced() {
                    let _ = tx.send(Msg::BinaryReplaced);
                }
            }
        }
    }
}

/// Registry of open tree panes, used by `toggle-tree`.
pub fn registry_dir(state_dir: &std::path::Path) -> PathBuf {
    state_dir.join("panes").join("tree")
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_db_core::config;
    use herdr_db_core::model::{Column, ObjectSummary};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn config() -> Config {
        config::load(
            Some((
                std::path::Path::new("herdr-db.toml"),
                r#"
[[folders]]
name = "Hellocare"

[[sources]]
id = "db_prod"
folder = "Hellocare"
engine = "postgres"
environment = "production"
database = "app"

[[sources]]
id = "local"
engine = "postgres"
database = "scratch"
"#,
            )),
            None,
        )
        .unwrap()
    }

    fn key(code: KeyCode) -> Input<Msg> {
        Input::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn snapshot() -> Snapshot {
        let object = |name: &str, kind| CachedObject {
            summary: ObjectSummary { name: name.into(), kind, comment: None, estimated_rows: Some(1200) },
            detail_at: None,
        };
        Snapshot {
            schemas: vec![SchemaNode {
                name: "public".into(),
                introspected_at: Utc::now() - chrono::Duration::minutes(3),
                objects: vec![
                    object("orders", ObjectKind::Table),
                    object("users", ObjectKind::Table),
                    object("v_users", ObjectKind::View),
                ],
            }],
            available: Some(vec!["audit".into(), "public".into()]),
            details: HashMap::new(),
        }
    }

    fn detail() -> TableDetail {
        let column = |name: &str, ordinal, nullable| Column {
            name: name.into(),
            ordinal,
            data_type: "integer".into(),
            nullable,
            default: None,
            comment: None,
            generated: None,
            identity: None,
            extra: None,
        };
        TableDetail {
            kind: ObjectKind::Table,
            columns: vec![column("id", 1, false), column("email", 2, true)],
            indexes: vec![],
            foreign_keys: vec![],
            constraints: vec![herdr_db_core::model::Constraint {
                name: "users_pkey".into(),
                kind: herdr_db_core::model::ConstraintKind::PrimaryKey,
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

    fn tree() -> Tree {
        Tree::new(config(), Icons { nerd: false }, Keymap::new(&BTreeMap::new()))
    }

    fn names(tree: &Tree) -> Vec<String> {
        tree.rows.iter().map(|r| format!("{}{}", "  ".repeat(r.depth), r.name)).collect()
    }

    #[test]
    fn expanding_a_source_loads_the_cache_then_shows_objects() {
        let mut tree = tree();
        assert_eq!(names(&tree), vec!["Hellocare", "  db_prod", "local"]);
        tree.update(key(KeyCode::Down));
        let effects = tree.update(key(KeyCode::Enter));
        assert!(matches!(effects.as_slice(), [Effect::LoadCache(s, _)] if s.id == "db_prod"));
        assert_eq!(names(&tree)[2], "    chargement…");

        let effects = tree.update(Input::Msg(Msg::CacheLoaded("db_prod".into(), Ok(snapshot()))));
        assert!(effects.is_empty(), "schema already cached: no introspection");
        tree.update(key(KeyCode::Down));
        tree.update(key(KeyCode::Char('l')));
        tree.update(key(KeyCode::Down));
        tree.update(key(KeyCode::Char('l')));
        assert_eq!(
            names(&tree),
            vec![
                "Hellocare",
                "  db_prod",
                "    public",
                "      tables",
                "        orders",
                "        users",
                "      vues",
                "local"
            ]
        );
    }

    #[test]
    fn uncached_source_is_introspected_once() {
        let mut tree = tree();
        tree.selected = 2;
        tree.update(key(KeyCode::Enter));
        let effects = tree.update(Input::Msg(Msg::CacheLoaded("local".into(), Ok(Snapshot::default()))));
        assert!(matches!(effects.as_slice(), [Effect::Introspect(s, Scope::Source, false)] if s.id == "local"));
        let effects = tree.update(Input::Msg(Msg::Introspected(
            "local".into(),
            Scope::Source,
            Err(IntrospectError::Db(DbError::Setup("refused".into()))),
        )));
        assert!(matches!(effects.as_slice(), [Effect::LoadCache(..)]));
        let effects = tree.update(Input::Msg(Msg::CacheLoaded("local".into(), Ok(Snapshot::default()))));
        assert!(effects.is_empty(), "no loop after an error");
        assert!(names(&tree).iter().any(|n| n.contains("refused")));
    }

    #[test]
    fn password_prompt_retries_the_introspection() {
        let mut tree = tree();
        tree.update(Input::Msg(Msg::Introspected(
            "local".into(),
            Scope::Source,
            Err(IntrospectError::Db(DbError::NeedPassword { user: "samir".into(), rejected: false })),
        )));
        assert!(matches!(tree.popup, Some(Popup::Password(_))));
        tree.update(key(KeyCode::Char('x')));
        let effects = tree.update(key(KeyCode::Enter));
        assert!(matches!(
            effects.as_slice(),
            [Effect::ProvidePassword(_, _, true), Effect::Introspect(_, Scope::Source, false)]
        ));
    }

    fn open_users(tree: &mut Tree) {
        tree.update(Input::Msg(Msg::CacheLoaded("db_prod".into(), Ok(snapshot()))));
        for id in [
            NodeId::Source("db_prod".into()),
            NodeId::Schema("db_prod".into(), "public".into()),
            NodeId::Group("db_prod".into(), "public".into(), Group::Tables),
        ] {
            tree.expanded.insert(id);
        }
        let users = ObjectRef::new("db_prod", "public", "users");
        tree.sources[0].snapshot.details.insert(users.clone(), detail());
        tree.expanded.insert(NodeId::Object(users.clone()));
        tree.expanded.insert(NodeId::Detail(users, DetailGroup::Columns));
        tree.now = tree.sources[0].snapshot.schemas[0].introspected_at + chrono::Duration::minutes(2);
        tree.rebuild();
    }

    #[test]
    fn actions_on_columns() {
        let mut tree = tree();
        open_users(&mut tree);
        let email = tree.rows.iter().position(|r| r.name == "email").unwrap();
        tree.selected = email;
        let effects = tree.update(key(KeyCode::Char('y')));
        assert!(matches!(effects.as_slice(), [Effect::Copy(t)] if t == "public.users.email"));
        let effects = tree.update(Input::Key(KeyEvent::new(KeyCode::Char('K'), KeyModifiers::SHIFT)));
        let [Effect::Open(request)] = effects.as_slice() else { panic!() };
        assert_eq!(request.action, Action::QuickDoc);
        assert_eq!(request.column.as_deref(), Some("email"));
        let effects = tree.update(key(KeyCode::Char('e')));
        assert!(
            matches!(effects.as_slice(), [Effect::Open(r)] if r.action == Action::EditData && r.object.as_ref().unwrap().name == "users")
        );
        let effects = tree.update(key(KeyCode::Char('r')));
        assert!(matches!(effects.as_slice(), [Effect::Introspect(_, Scope::Table(o), false)] if o.name == "users"));
    }

    #[test]
    fn speed_search_filters_and_keeps_ancestors() {
        let mut tree = tree();
        open_users(&mut tree);
        tree.expanded.remove(&NodeId::Schema("db_prod".into(), "public".into()));
        tree.rebuild();
        tree.update(key(KeyCode::Char('/')));
        for c in "ema".chars() {
            tree.update(key(KeyCode::Char(c)));
        }
        assert_eq!(
            names(&tree),
            vec![
                "Hellocare",
                "  db_prod",
                "    public",
                "      tables",
                "        users",
                "          colonnes",
                "            email"
            ]
        );
        assert_eq!(tree.rows[tree.selected].name, "email");
        tree.update(key(KeyCode::Enter));
        assert!(tree.search.is_none());
        assert_eq!(tree.rows[tree.selected].name, "email");
        assert!(tree.expanded.contains(&NodeId::Schema("db_prod".into(), "public".into())));
    }

    #[test]
    fn renders_badges_and_environment() {
        let mut tree = tree();
        open_users(&mut tree);
        let mut terminal = Terminal::new(TestBackend::new(44, 14)).unwrap();
        terminal.draw(|f| tree.view(f)).unwrap();
        insta::assert_snapshot!(terminal.backend());
    }

    #[test]
    fn schema_picker_saves_selection() {
        let mut tree = tree();
        tree.update(Input::Msg(Msg::CacheLoaded("db_prod".into(), Ok(snapshot()))));
        tree.selected = 1;
        tree.update(key(KeyCode::Char('s')));
        assert!(matches!(tree.popup, Some(Popup::Schemas(_))));
        tree.update(key(KeyCode::Char(' ')));
        let effects = tree.update(key(KeyCode::Enter));
        assert!(
            matches!(effects.as_slice(), [Effect::SaveSchemas(s, list)] if s == "db_prod" && list == &vec!["audit".to_string(), "public".to_string()])
        );
    }

    #[test]
    fn new_source_form_validates() {
        let mut tree = tree();
        tree.update(key(KeyCode::Char('n')));
        let Some(Popup::NewSource(form)) = &tree.popup else { panic!() };
        assert_eq!(form.value("dossier"), "Hellocare");
        for c in "scratch2".chars() {
            tree.update(key(KeyCode::Char(c)));
        }
        let effects = tree.update(Input::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)));
        assert!(effects.is_empty());
        let Some(Popup::NewSource(form)) = &tree.popup else { panic!() };
        assert_eq!(form.error.as_deref(), Some("la base est obligatoire"));
        for _ in 0..6 {
            tree.update(key(KeyCode::Tab));
        }
        for c in "db".chars() {
            tree.update(key(KeyCode::Char(c)));
        }
        let effects = tree.update(Input::Key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)));
        let [Effect::AddSource(source, None)] = effects.as_slice() else { panic!("{effects:?}") };
        assert_eq!((source.id.as_str(), source.database.as_str()), ("scratch2", "db"));
    }

    #[test]
    fn compact_counts() {
        assert_eq!(compact(999), "999");
        assert_eq!(compact(1200), "1.2k");
        assert_eq!(compact(45_000), "45k");
        assert_eq!(compact(3_400_000), "3.4M");
    }
}
