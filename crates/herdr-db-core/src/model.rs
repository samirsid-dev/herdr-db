//! Metadata model: identical for every engine, filled by the adapters,
//! stored in the cache and read by the tree, the DDL view and Quick Documentation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Engine {
    #[serde(rename = "postgres")]
    Postgres,
    #[serde(rename = "mysql")]
    MySql,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::MySql => "mysql",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Engine::Postgres => "PostgreSQL",
            Engine::MySql => "MySQL",
        }
    }

    pub fn default_port(self) -> u16 {
        match self {
            Engine::Postgres => 5432,
            Engine::MySql => 3306,
        }
    }

    pub fn parse(value: &str) -> Option<Engine> {
        match value.to_ascii_lowercase().as_str() {
            "postgres" | "postgresql" | "pg" => Some(Engine::Postgres),
            "mysql" | "mariadb" => Some(Engine::MySql),
            _ => None,
        }
    }
}

impl fmt::Display for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Qualified name of an object, used everywhere (tree, cache, Copy Reference).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ObjectRef {
    pub source: String,
    pub schema: String,
    pub name: String,
}

impl ObjectRef {
    pub fn new(source: impl Into<String>, schema: impl Into<String>, name: impl Into<String>) -> Self {
        Self { source: source.into(), schema: schema.into(), name: name.into() }
    }
}

impl fmt::Display for ObjectRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.schema, self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Table,
    PartitionedTable,
    ForeignTable,
    View,
    MaterializedView,
}

impl ObjectKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Table => "table",
            ObjectKind::PartitionedTable => "partitioned_table",
            ObjectKind::ForeignTable => "foreign_table",
            ObjectKind::View => "view",
            ObjectKind::MaterializedView => "materialized_view",
        }
    }

    pub fn parse(value: &str) -> Option<ObjectKind> {
        Some(match value {
            "table" => ObjectKind::Table,
            "partitioned_table" => ObjectKind::PartitionedTable,
            "foreign_table" => ObjectKind::ForeignTable,
            "view" => ObjectKind::View,
            "materialized_view" => ObjectKind::MaterializedView,
            _ => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            ObjectKind::Table => "table",
            ObjectKind::PartitionedTable => "partitioned table",
            ObjectKind::ForeignTable => "foreign table",
            ObjectKind::View => "view",
            ObjectKind::MaterializedView => "materialized view",
        }
    }

    /// Tree group the object belongs to.
    pub fn is_view(self) -> bool {
        matches!(self, ObjectKind::View | ObjectKind::MaterializedView)
    }
}

/// Level 1: one entry per object of a schema, read when the source opens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectSummary {
    pub name: String,
    pub kind: ObjectKind,
    pub comment: Option<String>,
    /// Catalog estimate (`pg_class.reltuples`, `TABLES.TABLE_ROWS`), never an exact count.
    pub estimated_rows: Option<i64>,
}

/// Level 1: read when the source opens, for the whole schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaModel {
    pub name: String,
    pub introspected_at: DateTime<Utc>,
    pub objects: Vec<ObjectSummary>,
}

/// Level 2: read on demand, table by table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableDetail {
    pub kind: ObjectKind,
    pub columns: Vec<Column>,
    #[serde(default)]
    pub indexes: Vec<Index>,
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKey>,
    #[serde(default)]
    pub constraints: Vec<Constraint>,
    #[serde(default)]
    pub triggers: Vec<Trigger>,
    #[serde(default)]
    pub comment: Option<String>,
    /// PostgreSQL `PARTITION BY` clause of a partitioned table.
    #[serde(default)]
    pub partition_key: Option<String>,
    /// PostgreSQL view or materialized view query.
    #[serde(default)]
    pub view_definition: Option<String>,
    /// DDL produced by the engine itself (MySQL `SHOW CREATE`).
    #[serde(default)]
    pub native_ddl: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ordinal: u32,
    /// As the engine displays it (`character varying(255)`, `int unsigned`).
    pub data_type: String,
    pub nullable: bool,
    pub default: Option<String>,
    pub comment: Option<String>,
    #[serde(default)]
    pub generated: Option<GeneratedColumn>,
    #[serde(default)]
    pub identity: Option<Identity>,
    /// MySQL `EXTRA` (auto_increment, on update ...), kept for display.
    #[serde(default)]
    pub extra: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratedColumn {
    pub expression: String,
    pub stored: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Always,
    ByDefault,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    /// Column names, or expressions for expression indexes.
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
    pub method: Option<String>,
    /// Full `CREATE INDEX` statement when the engine provides it (`pg_get_indexdef`).
    pub definition: Option<String>,
    /// Name of the constraint this index backs (primary key, unique, exclusion).
    #[serde(default)]
    pub constraint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_update: Option<String>,
    pub on_delete: Option<String>,
    /// `pg_get_constraintdef` output on PostgreSQL.
    pub definition: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConstraintKind {
    PrimaryKey,
    Unique,
    Check,
    Exclusion,
}

impl ConstraintKind {
    pub fn label(self) -> &'static str {
        match self {
            ConstraintKind::PrimaryKey => "primary key",
            ConstraintKind::Unique => "unique",
            ConstraintKind::Check => "check",
            ConstraintKind::Exclusion => "exclusion",
        }
    }
}

/// Primary key, unique, check and exclusion constraints. Foreign keys live in
/// [`TableDetail::foreign_keys`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Constraint {
    pub name: String,
    pub kind: ConstraintKind,
    pub columns: Vec<String>,
    /// `pg_get_constraintdef` output, or the check clause on MySQL.
    pub definition: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    pub name: String,
    pub timing: String,
    pub events: Vec<String>,
    /// `pg_get_triggerdef` output, or the trigger body on MySQL.
    pub definition: Option<String>,
}

/// Column badges shown in the tree and Quick Documentation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Badges {
    pub primary_key: bool,
    pub foreign_key: bool,
    pub indexed: bool,
    pub not_null: bool,
}

impl TableDetail {
    pub fn primary_key(&self) -> Option<&Constraint> {
        self.constraints.iter().find(|c| c.kind == ConstraintKind::PrimaryKey)
    }

    /// Primary key columns, in key order. Empty when the table has none.
    pub fn primary_key_columns(&self) -> Vec<String> {
        if let Some(pk) = self.primary_key() {
            return pk.columns.clone();
        }
        self.indexes.iter().find(|i| i.primary).map(|i| i.columns.clone()).unwrap_or_default()
    }

    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    pub fn badges(&self, column: &str) -> Badges {
        let Some(col) = self.column(column) else {
            return Badges::default();
        };
        let pk = self.primary_key_columns();
        Badges {
            primary_key: pk.iter().any(|c| c == column),
            foreign_key: self.foreign_keys.iter().any(|fk| fk.columns.iter().any(|c| c == column)),
            indexed: self.indexes.iter().any(|i| i.columns.iter().any(|c| c == column)),
            not_null: !col.nullable,
        }
    }

    /// The foreign key a column takes part in, with the referenced column
    /// matching it. Multi-column keys return the matching position.
    pub fn foreign_key_for(&self, column: &str) -> Option<(&ForeignKey, &str)> {
        self.foreign_keys.iter().find_map(|fk| {
            let pos = fk.columns.iter().position(|c| c == column)?;
            Some((fk, fk.ref_columns.get(pos)?.as_str()))
        })
    }

    pub fn checks(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints.iter().filter(|c| c.kind == ConstraintKind::Check)
    }

    /// Primary key, unique and exclusion constraints.
    pub fn keys(&self) -> impl Iterator<Item = &Constraint> {
        self.constraints.iter().filter(|c| c.kind != ConstraintKind::Check)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str, nullable: bool) -> Column {
        Column {
            name: name.into(),
            ordinal: 1,
            data_type: "integer".into(),
            nullable,
            default: None,
            comment: None,
            generated: None,
            identity: None,
            extra: None,
        }
    }

    fn detail() -> TableDetail {
        TableDetail {
            kind: ObjectKind::Table,
            columns: vec![column("id", false), column("org_id", false), column("note", true)],
            indexes: vec![Index {
                name: "orders_org_idx".into(),
                columns: vec!["org_id".into()],
                unique: false,
                primary: false,
                method: Some("btree".into()),
                definition: None,
                constraint: None,
            }],
            foreign_keys: vec![ForeignKey {
                name: "orders_org_fk".into(),
                columns: vec!["org_id".into()],
                ref_schema: "public".into(),
                ref_table: "orgs".into(),
                ref_columns: vec!["uid".into()],
                on_update: None,
                on_delete: Some("CASCADE".into()),
                definition: None,
            }],
            constraints: vec![Constraint {
                name: "orders_pkey".into(),
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

    #[test]
    fn badges_combine() {
        let d = detail();
        assert_eq!(d.badges("id"), Badges { primary_key: true, foreign_key: false, indexed: false, not_null: true });
        assert_eq!(d.badges("org_id"), Badges { primary_key: false, foreign_key: true, indexed: true, not_null: true });
        assert_eq!(d.badges("note"), Badges::default());
        assert_eq!(d.badges("missing"), Badges::default());
    }

    #[test]
    fn foreign_key_lookup_maps_position() {
        let d = detail();
        let (fk, target) = d.foreign_key_for("org_id").unwrap();
        assert_eq!(fk.ref_table, "orgs");
        assert_eq!(target, "uid");
        assert!(d.foreign_key_for("id").is_none());
    }

    #[test]
    fn detail_roundtrips_through_json() {
        let d = detail();
        let json = serde_json::to_string(&d).unwrap();
        assert_eq!(serde_json::from_str::<TableDetail>(&json).unwrap(), d);
    }
}
