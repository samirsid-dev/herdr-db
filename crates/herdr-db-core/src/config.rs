//! Configuration: a team file versioned in the repository (`herdr-db.toml`)
//! and a personal file in the plugin config directory. Both share the same
//! format; the personal file wins field by field. Neither ever holds a secret.

use crate::model::Engine;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const TEAM_FILE_NAME: &str = "herdr-db.toml";
pub const PERSONAL_FILE_NAME: &str = "config.toml";

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ConfigError {
    #[error("{file}: {message}")]
    Parse { file: String, message: String },
    #[error("source `{source_id}`: missing field `{field}`")]
    MissingField { source_id: String, field: &'static str },
    #[error("source `{source_id}`: invalid {field}: {message}")]
    Invalid { source_id: String, field: &'static str, message: String },
    #[error("source `{source_id}` is declared twice in {file}")]
    Duplicate { source_id: String, file: String },
    #[error("invalid source id `{0}`: use letters, digits, `-`, `_` or `.`")]
    InvalidId(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Environment {
    Local,
    Development,
    Staging,
    Production,
}

impl Environment {
    pub fn parse(value: &str) -> Option<Environment> {
        Some(match value.to_ascii_lowercase().as_str() {
            "local" => Environment::Local,
            "development" | "dev" => Environment::Development,
            "staging" | "preprod" => Environment::Staging,
            "production" | "prod" => Environment::Production,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Environment::Local => "local",
            Environment::Development => "development",
            Environment::Staging => "staging",
            Environment::Production => "production",
        }
    }

    pub const ALL: [Environment; 4] =
        [Environment::Local, Environment::Development, Environment::Staging, Environment::Production];
}

impl fmt::Display for Environment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TlsMode {
    Disable,
    /// Try TLS, fall back to plain text (libpq default).
    #[default]
    Prefer,
    /// TLS without certificate verification.
    Require,
    /// TLS with certificate and host name verification.
    VerifyFull,
}

impl TlsMode {
    pub fn parse(value: &str) -> Option<TlsMode> {
        Some(match value {
            "disable" => TlsMode::Disable,
            "prefer" => TlsMode::Prefer,
            "require" => TlsMode::Require,
            "verify-full" => TlsMode::VerifyFull,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Team,
    Personal,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceConfig {
    pub id: String,
    pub name: String,
    pub folder: Option<String>,
    pub engine: Engine,
    pub environment: Environment,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: Option<String>,
    pub schemas: Vec<String>,
    pub read_only: bool,
    pub pre_connect: Option<Vec<String>>,
    pub password_command: Option<Vec<String>>,
    pub statement_timeout: Option<Duration>,
    pub tls: TlsMode,
    pub origin: Origin,
}

impl SourceConfig {
    /// Display label: `name` when set, else the id.
    pub fn label(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GridPlacement {
    #[default]
    Split,
    Tab,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IconMode {
    #[default]
    Auto,
    NerdFont,
    Ascii,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub page_size: usize,
    pub console_max_rows: usize,
    pub grid_placement: GridPlacement,
    pub icons: IconMode,
    pub check_updates: bool,
    /// Key overrides, action name -> key description (`"ctrl+r"`, `"K"`).
    pub keys: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            page_size: 200,
            console_max_rows: 1000,
            grid_placement: GridPlacement::Split,
            icons: IconMode::Auto,
            check_updates: true,
            keys: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Config {
    pub folders: Vec<String>,
    pub sources: Vec<SourceConfig>,
    pub settings: Settings,
    pub team_file: Option<PathBuf>,
    pub personal_file: Option<PathBuf>,
}

impl Config {
    pub fn source(&self, id: &str) -> Option<&SourceConfig> {
        self.sources.iter().find(|s| s.id == id)
    }
}

// ---------------------------------------------------------------------------
// File format

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    folders: Vec<RawFolder>,
    #[serde(default)]
    sources: Vec<RawSource>,
    #[serde(default)]
    settings: Option<RawSettings>,
    #[serde(default)]
    keys: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFolder {
    name: String,
}

/// Every field is optional so a personal entry can override a single field
/// of a team source (`user`, `port`, ...).
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSource {
    id: String,
    name: Option<String>,
    folder: Option<String>,
    engine: Option<String>,
    environment: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    user: Option<String>,
    schemas: Option<Vec<String>>,
    read_only: Option<bool>,
    pre_connect: Option<Vec<String>>,
    password_command: Option<Vec<String>>,
    statement_timeout: Option<String>,
    tls: Option<String>,
}

impl RawSource {
    fn overlay(&mut self, other: RawSource) {
        macro_rules! take {
            ($($field:ident),*) => { $( if other.$field.is_some() { self.$field = other.$field; } )* };
        }
        take!(
            name,
            folder,
            engine,
            environment,
            host,
            port,
            database,
            user,
            schemas,
            read_only,
            pre_connect,
            password_command,
            statement_timeout,
            tls
        );
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSettings {
    page_size: Option<usize>,
    console_max_rows: Option<usize>,
    grid_placement: Option<String>,
    icons: Option<String>,
    check_updates: Option<bool>,
}

fn parse_file(path: &Path, text: &str) -> Result<RawFile, ConfigError> {
    let file: RawFile = toml::from_str(text)
        .map_err(|e| ConfigError::Parse { file: path.display().to_string(), message: e.to_string() })?;
    let mut seen = std::collections::HashSet::new();
    for source in &file.sources {
        if !valid_id(&source.id) {
            return Err(ConfigError::InvalidId(source.id.clone()));
        }
        if !seen.insert(source.id.clone()) {
            return Err(ConfigError::Duplicate { source_id: source.id.clone(), file: path.display().to_string() });
        }
    }
    Ok(file)
}

/// Source ids become file names (cache, tunnels): keep them boring.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Parses `"30s"`, `"2m"`, `"500ms"`, `"1h"` or a bare number of seconds.
pub fn parse_duration(value: &str) -> Option<Duration> {
    let value = value.trim();
    let split = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    let (number, unit) = value.split_at(split);
    let number: u64 = number.parse().ok()?;
    Some(match unit.trim() {
        "" | "s" => Duration::from_secs(number),
        "ms" => Duration::from_millis(number),
        "m" | "min" => Duration::from_secs(number * 60),
        "h" => Duration::from_secs(number * 3600),
        _ => return None,
    })
}

/// Default statement timeout on shared environments.
pub const DEFAULT_GUARDED_TIMEOUT: Duration = Duration::from_secs(30);

fn resolve_source(raw: RawSource, origin: Origin) -> Result<SourceConfig, ConfigError> {
    let id = raw.id.clone();
    let missing = |field| ConfigError::MissingField { source_id: id.clone(), field };
    let invalid = |field, message: String| ConfigError::Invalid { source_id: id.clone(), field, message };

    let engine_name = raw.engine.ok_or_else(|| missing("engine"))?;
    let engine = Engine::parse(&engine_name)
        .ok_or_else(|| invalid("engine", format!("`{engine_name}` (expected postgres or mysql)")))?;
    let environment = match raw.environment {
        Some(env) => Environment::parse(&env).ok_or_else(|| {
            invalid("environment", format!("`{env}` (expected local, development, staging or production)"))
        })?,
        None => Environment::Local,
    };
    let database = raw.database.ok_or_else(|| missing("database"))?;
    let schemas = match raw.schemas {
        Some(schemas) if !schemas.is_empty() => schemas,
        _ => match engine {
            Engine::Postgres => vec!["public".to_string()],
            Engine::MySql => vec![database.clone()],
        },
    };
    let guarded = matches!(environment, Environment::Staging | Environment::Production);
    let statement_timeout = match raw.statement_timeout.as_deref() {
        Some("0") | Some("off") | Some("none") => None,
        Some(value) => Some(
            parse_duration(value).ok_or_else(|| invalid("statement_timeout", format!("`{value}` (e.g. \"30s\")")))?,
        ),
        None if guarded => Some(DEFAULT_GUARDED_TIMEOUT),
        None => None,
    };
    let tls = match raw.tls.as_deref() {
        Some(value) => TlsMode::parse(value)
            .ok_or_else(|| invalid("tls", format!("`{value}` (expected disable, prefer, require or verify-full)")))?,
        None => TlsMode::default(),
    };
    for (field, command) in [("pre_connect", &raw.pre_connect), ("password_command", &raw.password_command)] {
        if command.as_ref().is_some_and(|c| c.is_empty()) {
            return Err(invalid(field, "empty command".into()));
        }
    }

    Ok(SourceConfig {
        name: raw.name.unwrap_or_else(|| raw.id.clone()),
        id: raw.id,
        folder: raw.folder,
        engine,
        environment,
        host: raw.host.unwrap_or_else(|| "localhost".to_string()),
        port: raw.port.unwrap_or(engine.default_port()),
        database,
        user: raw.user,
        schemas,
        read_only: raw.read_only.unwrap_or(environment == Environment::Production),
        pre_connect: raw.pre_connect,
        password_command: raw.password_command,
        statement_timeout,
        tls,
        origin,
    })
}

/// Merges the team file and the personal file. Paths are only used in error
/// messages and recorded in the result; reading the files is the caller's job.
pub fn load(team: Option<(&Path, &str)>, personal: Option<(&Path, &str)>) -> Result<Config, ConfigError> {
    let team_raw = team.map(|(p, t)| parse_file(p, t)).transpose()?.unwrap_or_default();
    let personal_raw = personal.map(|(p, t)| parse_file(p, t)).transpose()?.unwrap_or_default();

    let mut folders: Vec<String> = Vec::new();
    for folder in team_raw.folders.iter().chain(&personal_raw.folders) {
        if !folders.contains(&folder.name) {
            folders.push(folder.name.clone());
        }
    }

    let mut merged: Vec<(RawSource, Origin)> = team_raw.sources.into_iter().map(|s| (s, Origin::Team)).collect();
    for source in personal_raw.sources {
        match merged.iter_mut().find(|(s, _)| s.id == source.id) {
            Some((existing, _)) => existing.overlay(source),
            None => merged.push((source, Origin::Personal)),
        }
    }

    let mut sources = Vec::with_capacity(merged.len());
    for (raw, origin) in merged {
        let source = resolve_source(raw, origin)?;
        if let Some(folder) = &source.folder
            && !folders.contains(folder)
        {
            folders.push(folder.clone());
        }
        sources.push(source);
    }

    let mut settings = Settings::default();
    for raw in [team_raw.settings, personal_raw.settings].into_iter().flatten() {
        if let Some(v) = raw.page_size {
            settings.page_size = v.clamp(10, 10_000);
        }
        if let Some(v) = raw.console_max_rows {
            settings.console_max_rows = v.clamp(1, 1_000_000);
        }
        if let Some(v) = raw.grid_placement {
            settings.grid_placement = match v.as_str() {
                "tab" => GridPlacement::Tab,
                _ => GridPlacement::Split,
            };
        }
        if let Some(v) = raw.icons {
            settings.icons = match v.as_str() {
                "nerd-font" | "nerdfont" => IconMode::NerdFont,
                "ascii" => IconMode::Ascii,
                _ => IconMode::Auto,
            };
        }
        if let Some(v) = raw.check_updates {
            settings.check_updates = v;
        }
    }
    settings.keys = team_raw.keys;
    settings.keys.extend(personal_raw.keys);

    Ok(Config {
        folders,
        sources,
        settings,
        team_file: team.map(|(p, _)| p.to_path_buf()),
        personal_file: personal.map(|(p, _)| p.to_path_buf()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEAM: &str = r#"
[[folders]]
name = "Hellocare"

[[sources]]
id = "db_prod"
folder = "Hellocare"
engine = "postgres"
environment = "production"
host = "localhost"
port = 15432
database = "app"
schemas = ["public"]
read_only = true
pre_connect = ["kubectl", "port-forward", "svc/postgres", "15432:5432", "-n", "prod"]

[[sources]]
id = "hc-platform"
folder = "Hellocare"
engine = "mysql"
environment = "development"
host = "localhost"
port = 3306
database = "platform"
"#;

    fn team() -> (&'static Path, &'static str) {
        (Path::new("herdr-db.toml"), TEAM)
    }

    #[test]
    fn team_file_resolves_with_defaults() {
        let config = load(Some(team()), None).unwrap();
        assert_eq!(config.folders, vec!["Hellocare"]);
        let prod = config.source("db_prod").unwrap();
        assert_eq!(prod.engine, Engine::Postgres);
        assert_eq!(prod.environment, Environment::Production);
        assert!(prod.read_only);
        assert_eq!(prod.statement_timeout, Some(DEFAULT_GUARDED_TIMEOUT));
        assert_eq!(prod.origin, Origin::Team);

        let platform = config.source("hc-platform").unwrap();
        assert_eq!(platform.schemas, vec!["platform"]);
        assert!(!platform.read_only);
        assert_eq!(platform.statement_timeout, None);
        assert_eq!(platform.tls, TlsMode::Prefer);
    }

    #[test]
    fn personal_file_overrides_field_by_field() {
        let personal = r#"
[[sources]]
id = "db_prod"
user = "samir_ro"
port = 15433

[[sources]]
id = "local"
engine = "postgres"
database = "scratch"

[settings]
page_size = 500

[keys]
edit_data = "E"
"#;
        let config = load(Some(team()), Some((Path::new("config.toml"), personal))).unwrap();
        let prod = config.source("db_prod").unwrap();
        assert_eq!(prod.user.as_deref(), Some("samir_ro"));
        assert_eq!(prod.port, 15433);
        assert_eq!(prod.database, "app");
        assert_eq!(prod.origin, Origin::Team);

        let local = config.source("local").unwrap();
        assert_eq!(local.origin, Origin::Personal);
        assert_eq!(local.environment, Environment::Local);
        assert_eq!(local.port, 5432);
        assert_eq!(local.schemas, vec!["public"]);
        assert_eq!(config.settings.page_size, 500);
        assert_eq!(config.settings.keys.get("edit_data").map(String::as_str), Some("E"));
    }

    #[test]
    fn production_read_only_can_be_disabled_explicitly() {
        let personal = "[[sources]]\nid = \"db_prod\"\nread_only = false\nstatement_timeout = \"off\"\n";
        let config = load(Some(team()), Some((Path::new("p.toml"), personal))).unwrap();
        let prod = config.source("db_prod").unwrap();
        assert!(!prod.read_only);
        assert_eq!(prod.statement_timeout, None);
    }

    #[test]
    fn errors_name_the_source_and_field() {
        let err =
            load(Some((Path::new("t.toml"), "[[sources]]\nid = \"x\"\nengine = \"postgres\"\n")), None).unwrap_err();
        assert_eq!(err, ConfigError::MissingField { source_id: "x".into(), field: "database" });

        let err =
            load(Some((Path::new("t.toml"), "[[sources]]\nid = \"x\"\nengine = \"oracle\"\ndatabase = \"d\"\n")), None)
                .unwrap_err();
        assert!(err.to_string().contains("oracle"));

        let err = load(Some((Path::new("t.toml"), "[[sources]]\nid = \"../x\"\n")), None).unwrap_err();
        assert_eq!(err, ConfigError::InvalidId("../x".into()));

        let err = load(Some((Path::new("t.toml"), "[[sources]]\nid = \"a\"\npassword = \"x\"\n")), None).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "secrets are rejected as unknown fields");
    }

    #[test]
    fn duplicate_ids_in_one_file_are_rejected() {
        let text = "[[sources]]\nid = \"a\"\n[[sources]]\nid = \"a\"\n";
        assert!(matches!(load(Some((Path::new("t.toml"), text)), None), Err(ConfigError::Duplicate { .. })));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
        assert_eq!(parse_duration("45"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("3 days"), None);
    }
}
