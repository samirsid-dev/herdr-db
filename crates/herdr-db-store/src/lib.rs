//! Local metadata cache: one SQLite file per data source in
//! `$HERDR_PLUGIN_STATE_DIR/cache/<source_id>.sqlite`.
//!
//! The cache is derived data, never a source of truth: deleting it is always
//! safe ("Forget Cache"), an introspection rebuilds it. Introspection is the
//! only writer; every pane reads the last committed version (WAL mode) and
//! polls `PRAGMA data_version` to notice another process's commits.

use chrono::{DateTime, Utc};
use herdr_db_core::model::{Engine, ObjectKind, ObjectRef, ObjectSummary, SchemaModel, TableDetail};
use rusqlite::{Connection, OptionalExtension, params};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bump when the schema below or the serialized model changes incompatibly:
/// an outdated file is deleted and the source re-introspected.
pub const FORMAT_VERSION: &str = "1";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("cache SQLite : {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("cache : {0}")]
    Io(#[from] std::io::Error),
    #[error("cache : détail illisible pour {0} ({1})")]
    Corrupt(String, serde_json::Error),
}

pub type Result<T> = std::result::Result<T, StoreError>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS schemas (
  name            TEXT PRIMARY KEY,
  introspected_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS objects (
  schema         TEXT NOT NULL,
  name           TEXT NOT NULL,
  kind           TEXT NOT NULL,
  comment        TEXT,
  estimated_rows INTEGER,
  detail_json    TEXT,
  detail_at      TEXT,
  PRIMARY KEY (schema, name)
);
";

#[derive(Debug, Clone, PartialEq)]
pub struct SchemaInfo {
    pub name: String,
    pub introspected_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CachedObject {
    pub summary: ObjectSummary,
    pub detail_at: Option<DateTime<Utc>>,
}

pub fn cache_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("cache")
}

pub fn cache_path(state_dir: &Path, source: &str) -> PathBuf {
    cache_dir(state_dir).join(format!("{source}.sqlite"))
}

/// Forget Cache: removes the database and its WAL files.
pub fn forget(state_dir: &Path, source: &str) -> Result<()> {
    let path = cache_path(state_dir, source);
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.clone().into_os_string();
        file.push(suffix);
        match fs::remove_file(PathBuf::from(file)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

pub struct Cache {
    conn: Connection,
    path: PathBuf,
}

impl Cache {
    pub fn open(state_dir: &Path, source: &str, engine: Engine) -> Result<Cache> {
        let dir = cache_dir(state_dir);
        create_private_dir(&dir)?;
        Self::open_at(&cache_path(state_dir, source), engine)
    }

    pub fn open_at(path: &Path, engine: Engine) -> Result<Cache> {
        let cache = Self::open_raw(path)?;
        let version = cache.meta("format_version")?;
        let stored_engine = cache.meta("engine")?;
        let outdated = version.as_deref().is_some_and(|v| v != FORMAT_VERSION)
            || stored_engine.as_deref().is_some_and(|e| e != engine.as_str());
        if !outdated {
            if version.is_none() {
                cache.set_meta("format_version", FORMAT_VERSION)?;
                cache.set_meta("engine", engine.as_str())?;
            }
            return Ok(cache);
        }
        drop(cache);
        for suffix in ["", "-wal", "-shm"] {
            let mut file = path.as_os_str().to_owned();
            file.push(suffix);
            let _ = fs::remove_file(PathBuf::from(file));
        }
        let cache = Self::open_raw(path)?;
        cache.set_meta("format_version", FORMAT_VERSION)?;
        cache.set_meta("engine", engine.as_str())?;
        Ok(cache)
    }

    fn open_raw(path: &Path) -> Result<Cache> {
        let existed = path.exists();
        let conn = Connection::open(path)?;
        if !existed {
            set_private(path)?;
        }
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Cache { conn, path: path.to_path_buf() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Every schema of the server, for the schema selector.
    pub fn available_schemas(&self) -> Result<Option<Vec<String>>> {
        Ok(self.meta("available_schemas")?.and_then(|json| serde_json::from_str(&json).ok()))
    }

    pub fn set_available_schemas(&self, schemas: &[String]) -> Result<()> {
        self.set_meta("available_schemas", &serde_json::to_string(schemas).expect("strings serialize"))
    }

    /// Changes whenever another connection commits to the database.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    pub fn schemas(&self) -> Result<Vec<SchemaInfo>> {
        let mut stmt = self.conn.prepare("SELECT name, introspected_at FROM schemas ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(SchemaInfo { name: r.get(0)?, introspected_at: parse_time(&r.get::<_, String>(1)?) })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn schema(&self, name: &str) -> Result<Option<SchemaInfo>> {
        Ok(self
            .conn
            .query_row("SELECT name, introspected_at FROM schemas WHERE name = ?1", [name], |r| {
                Ok(SchemaInfo { name: r.get(0)?, introspected_at: parse_time(&r.get::<_, String>(1)?) })
            })
            .optional()?)
    }

    pub fn objects(&self, schema: &str) -> Result<Vec<CachedObject>> {
        let mut stmt = self.conn.prepare(
            "SELECT name, kind, comment, estimated_rows, detail_at FROM objects
             WHERE schema = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map([schema], |r| {
            let kind: String = r.get(1)?;
            Ok(CachedObject {
                summary: ObjectSummary {
                    name: r.get(0)?,
                    kind: ObjectKind::parse(&kind).unwrap_or(ObjectKind::Table),
                    comment: r.get(2)?,
                    estimated_rows: r.get(3)?,
                },
                detail_at: r.get::<_, Option<String>>(4)?.as_deref().map(parse_time),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn object(&self, object: &ObjectRef) -> Result<Option<CachedObject>> {
        Ok(self.objects(&object.schema)?.into_iter().find(|o| o.summary.name == object.name))
    }

    pub fn detail(&self, object: &ObjectRef) -> Result<Option<(TableDetail, DateTime<Utc>)>> {
        let row: Option<(Option<String>, Option<String>)> = self
            .conn
            .query_row(
                "SELECT detail_json, detail_at FROM objects WHERE schema = ?1 AND name = ?2",
                params![object.schema, object.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((Some(json), at)) = row else {
            return Ok(None);
        };
        let detail = serde_json::from_str(&json).map_err(|e| StoreError::Corrupt(object.to_string(), e))?;
        Ok(Some((detail, at.as_deref().map(parse_time).unwrap_or_default())))
    }

    /// Writes a schema's object list in one transaction: readers see the old
    /// model or the new one, never a mix. Details of objects that still exist
    /// with the same kind are kept.
    pub fn write_schema(&mut self, model: &SchemaModel) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO schemas (name, introspected_at) VALUES (?1, ?2)
             ON CONFLICT (name) DO UPDATE SET introspected_at = excluded.introspected_at",
            params![model.name, model.introspected_at.to_rfc3339()],
        )?;
        tx.execute("CREATE TEMP TABLE IF NOT EXISTS incoming (name TEXT PRIMARY KEY, kind TEXT NOT NULL)", [])?;
        tx.execute("DELETE FROM temp.incoming", [])?;
        {
            let mut insert = tx.prepare("INSERT OR REPLACE INTO temp.incoming (name, kind) VALUES (?1, ?2)")?;
            for object in &model.objects {
                insert.execute(params![object.name, object.kind.as_str()])?;
            }
        }
        tx.execute(
            "DELETE FROM objects WHERE schema = ?1 AND NOT EXISTS (
               SELECT 1 FROM temp.incoming i WHERE i.name = objects.name AND i.kind = objects.kind)",
            [&model.name],
        )?;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO objects (schema, name, kind, comment, estimated_rows)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT (schema, name) DO UPDATE SET
                   kind = excluded.kind, comment = excluded.comment,
                   estimated_rows = excluded.estimated_rows",
            )?;
            for object in &model.objects {
                upsert.execute(params![
                    model.name,
                    object.name,
                    object.kind.as_str(),
                    object.comment,
                    object.estimated_rows
                ])?;
            }
        }
        tx.execute("DELETE FROM temp.incoming", [])?;
        tx.commit()?;
        Ok(())
    }

    pub fn write_detail(&mut self, object: &ObjectRef, detail: &TableDetail, at: DateTime<Utc>) -> Result<()> {
        let json = serde_json::to_string(detail).expect("detail serializes");
        let changed = self.conn.execute(
            "UPDATE objects SET detail_json = ?3, detail_at = ?4, kind = ?5, comment = ?6
             WHERE schema = ?1 AND name = ?2",
            params![object.schema, object.name, json, at.to_rfc3339(), detail.kind.as_str(), detail.comment],
        )?;
        if changed == 0 {
            // Object opened before its schema was introspected (direct request).
            self.conn.execute(
                "INSERT INTO objects (schema, name, kind, comment, detail_json, detail_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![object.schema, object.name, detail.kind.as_str(), detail.comment, json, at.to_rfc3339()],
            )?;
        }
        Ok(())
    }

    /// Drops every cached detail of a schema (Force Refresh keeps nothing).
    pub fn clear_details(&self, schema: &str) -> Result<()> {
        self.conn.execute("UPDATE objects SET detail_json = NULL, detail_at = NULL WHERE schema = ?1", [schema])?;
        Ok(())
    }

    /// Removes schemas no longer displayed.
    pub fn retain_schemas(&mut self, keep: &[String]) -> Result<()> {
        let tx = self.conn.transaction()?;
        let existing: Vec<String> = {
            let mut stmt = tx.prepare("SELECT name FROM schemas")?;
            stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for schema in existing.iter().filter(|s| !keep.contains(s)) {
            tx.execute("DELETE FROM objects WHERE schema = ?1", [schema])?;
            tx.execute("DELETE FROM schemas WHERE name = ?1", [schema])?;
        }
        tx.commit()?;
        Ok(())
    }
}

/// Prevents two introspections of the same source from running in parallel.
/// Backed by an OS file lock: released when dropped or when the process dies.
pub struct IntrospectionLock {
    _file: File,
}

impl IntrospectionLock {
    fn lock_file(state_dir: &Path, source: &str) -> Result<File> {
        let dir = cache_dir(state_dir);
        create_private_dir(&dir)?;
        let path = dir.join(format!("{source}.lock"));
        let file = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
        set_private(&path)?;
        Ok(file)
    }

    /// `None` when another process is introspecting this source.
    pub fn try_acquire(state_dir: &Path, source: &str) -> Result<Option<IntrospectionLock>> {
        let file = Self::lock_file(state_dir, source)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(IntrospectionLock { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }

    /// Waits for the other introspection to finish.
    pub fn acquire(state_dir: &Path, source: &str) -> Result<IntrospectionLock> {
        let file = Self::lock_file(state_dir, source)?;
        file.lock()?;
        Ok(IntrospectionLock { _file: file })
    }
}

fn parse_time(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value).map(|t| t.with_timezone(&Utc)).unwrap_or_default()
}

/// State dir content is private to the user: directories 0700, files 0600.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)
    }
}

/// Restricts an existing directory to the user (0700).
pub fn make_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

pub fn set_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use herdr_db_core::model::Column;

    fn model(objects: &[(&str, ObjectKind)]) -> SchemaModel {
        SchemaModel {
            name: "public".into(),
            introspected_at: Utc::now(),
            objects: objects
                .iter()
                .map(|(name, kind)| ObjectSummary {
                    name: name.to_string(),
                    kind: *kind,
                    comment: None,
                    estimated_rows: Some(10),
                })
                .collect(),
        }
    }

    fn detail() -> TableDetail {
        TableDetail {
            kind: ObjectKind::Table,
            columns: vec![Column {
                name: "id".into(),
                ordinal: 1,
                data_type: "integer".into(),
                nullable: false,
                default: None,
                comment: None,
                generated: None,
                identity: None,
                extra: None,
            }],
            indexes: vec![],
            foreign_keys: vec![],
            constraints: vec![],
            triggers: vec![],
            comment: Some("users".into()),
            partition_key: None,
            view_definition: None,
            native_ddl: None,
        }
    }

    #[test]
    fn schema_write_keeps_details_of_surviving_objects() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(dir.path(), "src", Engine::Postgres).unwrap();
        cache.write_schema(&model(&[("users", ObjectKind::Table), ("orders", ObjectKind::Table)])).unwrap();
        let users = ObjectRef::new("src", "public", "users");
        cache.write_detail(&users, &detail(), Utc::now()).unwrap();
        assert!(cache.detail(&users).unwrap().is_some());

        cache.write_schema(&model(&[("users", ObjectKind::Table), ("v", ObjectKind::View)])).unwrap();
        let names: Vec<String> = cache.objects("public").unwrap().into_iter().map(|o| o.summary.name).collect();
        assert_eq!(names, vec!["users", "v"]);
        assert!(cache.detail(&users).unwrap().is_some());
        assert_eq!(cache.schemas().unwrap().len(), 1);

        cache.clear_details("public").unwrap();
        assert!(cache.detail(&users).unwrap().is_none());
    }

    #[test]
    fn other_connections_see_commits_through_data_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Cache::open(dir.path(), "src", Engine::Postgres).unwrap();
        let reader = Cache::open(dir.path(), "src", Engine::Postgres).unwrap();
        let before = reader.data_version().unwrap();
        writer.write_schema(&model(&[("t", ObjectKind::Table)])).unwrap();
        assert_ne!(reader.data_version().unwrap(), before);
        assert_eq!(reader.objects("public").unwrap().len(), 1);
    }

    #[test]
    fn outdated_format_is_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(dir.path(), "src", Engine::Postgres).unwrap();
        cache.write_schema(&model(&[("t", ObjectKind::Table)])).unwrap();
        cache.set_meta("format_version", "0").unwrap();
        drop(cache);
        let cache = Cache::open(dir.path(), "src", Engine::Postgres).unwrap();
        assert!(cache.schemas().unwrap().is_empty());
        assert_eq!(cache.meta("format_version").unwrap().as_deref(), Some(FORMAT_VERSION));
    }

    #[test]
    fn detail_for_unknown_object_is_inserted() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = Cache::open(dir.path(), "src", Engine::MySql).unwrap();
        let object = ObjectRef::new("src", "app", "t");
        cache.write_detail(&object, &detail(), Utc::now()).unwrap();
        assert_eq!(cache.object(&object).unwrap().unwrap().summary.kind, ObjectKind::Table);
    }

    #[test]
    fn introspection_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let first = IntrospectionLock::try_acquire(dir.path(), "src").unwrap();
        assert!(first.is_some());
        assert!(IntrospectionLock::try_acquire(dir.path(), "src").unwrap().is_none());
        assert!(IntrospectionLock::try_acquire(dir.path(), "other").unwrap().is_some());
        drop(first);
        assert!(IntrospectionLock::try_acquire(dir.path(), "src").unwrap().is_some());
    }

    #[test]
    fn forget_removes_files_and_permissions_are_private() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path(), "src", Engine::Postgres).unwrap();
        let path = cache.path().to_path_buf();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(fs::metadata(cache_dir(dir.path())).unwrap().permissions().mode() & 0o777, 0o700);
        }
        drop(cache);
        forget(dir.path(), "src").unwrap();
        assert!(!path.exists());
        forget(dir.path(), "src").unwrap();
    }
}
