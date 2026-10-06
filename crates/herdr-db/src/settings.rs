//! Reads the team and personal files, and edits the personal file (new data
//! source, schema selection) while keeping its comments and layout.

use crate::paths::HerdrEnv;
use anyhow::{Context, Result};
use herdr_db_core::config::{self, Config};
use std::fs;
use std::path::Path;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

pub fn load(env: &HerdrEnv) -> Result<Config> {
    let team_path = env.team_config();
    let team_text = match &team_path {
        Some(path) => Some(fs::read_to_string(path).with_context(|| format!("lecture de {}", path.display()))?),
        None => None,
    };
    let personal_path = env.personal_config();
    let personal_text = fs::read_to_string(&personal_path).ok();
    let config = config::load(
        team_path.as_deref().zip(team_text.as_deref()),
        personal_text.as_deref().map(|t| (personal_path.as_path(), t)),
    )?;
    Ok(config)
}

/// New personal data source (form `n` of the tree). Never holds a secret.
#[derive(Debug, Clone, Default)]
pub struct NewSource {
    pub id: String,
    pub folder: Option<String>,
    pub engine: String,
    pub environment: String,
    pub host: String,
    pub port: Option<u16>,
    pub database: String,
    pub user: Option<String>,
    pub schemas: Vec<String>,
    pub read_only: Option<bool>,
    pub pre_connect: Option<Vec<String>>,
    pub password_command: Option<Vec<String>>,
}

fn read_document(path: &Path) -> Result<DocumentMut> {
    let text = fs::read_to_string(path).unwrap_or_default();
    text.parse::<DocumentMut>().with_context(|| format!("{} n'est pas un TOML valide", path.display()))
}

fn write_document(path: &Path, document: &DocumentMut) -> Result<()> {
    if let Some(dir) = path.parent() {
        herdr_db_store::create_private_dir(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, document.to_string())?;
    herdr_db_store::set_private(&tmp)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn string_array(values: &[String]) -> Array {
    values.iter().map(String::as_str).collect()
}

fn sources_mut(document: &mut DocumentMut) -> &mut ArrayOfTables {
    if !document.contains_key("sources") {
        document.insert("sources", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    document["sources"].as_array_of_tables_mut().expect("sources is an array of tables")
}

pub fn add_source(path: &Path, source: &NewSource) -> Result<()> {
    let mut document = read_document(path)?;
    if let Some(folder) = &source.folder {
        if !document.contains_key("folders") {
            document.insert("folders", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let folders = document["folders"].as_array_of_tables_mut().expect("folders");
        if !folders.iter().any(|f| f.get("name").and_then(|n| n.as_str()) == Some(folder)) {
            let mut table = Table::new();
            table["name"] = value(folder.as_str());
            folders.push(table);
        }
    }
    let sources = sources_mut(&mut document);
    if sources.iter().any(|s| s.get("id").and_then(|i| i.as_str()) == Some(&source.id)) {
        anyhow::bail!("la source `{}` existe déjà dans {}", source.id, path.display());
    }
    let mut table = Table::new();
    table["id"] = value(source.id.as_str());
    if let Some(folder) = &source.folder {
        table["folder"] = value(folder.as_str());
    }
    table["engine"] = value(source.engine.as_str());
    table["environment"] = value(source.environment.as_str());
    table["host"] = value(source.host.as_str());
    if let Some(port) = source.port {
        table["port"] = value(i64::from(port));
    }
    table["database"] = value(source.database.as_str());
    if let Some(user) = &source.user {
        table["user"] = value(user.as_str());
    }
    if !source.schemas.is_empty() {
        table["schemas"] = value(string_array(&source.schemas));
    }
    if let Some(read_only) = source.read_only {
        table["read_only"] = value(read_only);
    }
    if let Some(command) = &source.pre_connect {
        table["pre_connect"] = value(string_array(command));
    }
    if let Some(command) = &source.password_command {
        table["password_command"] = value(string_array(command));
    }
    sources.push(table);
    write_document(path, &document)
}

/// Schema selection is personal: it overrides the team file field by field.
pub fn set_schemas(path: &Path, source_id: &str, schemas: &[String]) -> Result<()> {
    let mut document = read_document(path)?;
    let sources = sources_mut(&mut document);
    let existing = sources.iter().position(|s| s.get("id").and_then(|i| i.as_str()) == Some(source_id));
    match existing.and_then(|i| sources.get_mut(i)) {
        Some(table) => {
            table["schemas"] = value(string_array(schemas));
        }
        None => {
            let mut table = Table::new();
            table["id"] = value(source_id);
            table["schemas"] = value(string_array(schemas));
            sources.push(table);
        }
    }
    write_document(path, &document)
}

/// Splits a command typed in a form field (`infisical secrets get X --plain`).
/// Quotes group words; no other shell syntax.
pub fn split_command(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut has_word = false;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                has_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if has_word || !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                    has_word = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if has_word || !current.is_empty() {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_source_and_select_schemas_keep_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "# mes réglages\n[settings]\npage_size = 100\n").unwrap();
        add_source(
            &path,
            &NewSource {
                id: "local".into(),
                folder: Some("Perso".into()),
                engine: "postgres".into(),
                environment: "local".into(),
                host: "localhost".into(),
                port: Some(5432),
                database: "scratch".into(),
                user: Some("samir".into()),
                schemas: vec![],
                read_only: None,
                pre_connect: None,
                password_command: Some(vec!["infisical".into(), "secrets".into()]),
            },
        )
        .unwrap();
        set_schemas(&path, "local", &["public".into(), "audit".into()]).unwrap();
        set_schemas(&path, "db_prod", &["public".into()]).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# mes réglages"));
        let team = "[[sources]]\nid = \"db_prod\"\nengine = \"postgres\"\ndatabase = \"app\"\n";
        let config = config::load(Some((Path::new("herdr-db.toml"), team)), Some((&path, &text))).unwrap();
        assert_eq!(config.source("db_prod").unwrap().schemas, vec!["public"]);
        assert_eq!(config.folders, vec!["Perso"]);
        assert_eq!(config.sources.len(), 2);
        let local = config.source("local").unwrap();
        assert_eq!(local.schemas, vec!["public", "audit"]);
        assert_eq!(local.password_command.as_ref().unwrap()[0], "infisical");
        assert!(add_source(&path, &NewSource { id: "local".into(), ..Default::default() }).is_err());
    }

    #[test]
    fn command_splitting() {
        assert_eq!(
            split_command(r#"infisical secrets get DB_PASSWORD --env=prod --path="/db x" --plain"#),
            vec!["infisical", "secrets", "get", "DB_PASSWORD", "--env=prod", "--path=/db x", "--plain"]
        );
        assert_eq!(split_command("  "), Vec::<String>::new());
        assert_eq!(split_command("a ''"), vec!["a", ""]);
    }
}
