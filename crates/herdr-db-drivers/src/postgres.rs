//! PostgreSQL adapter: tokio-postgres. Introspection reads `pg_catalog` with
//! the extended protocol; data goes through the simple query protocol, which
//! returns every value as text and can be cancelled with the native token.

use crate::tls::{unverified_config, verifying_config};
use crate::{
    APPLICATION_NAME, Adapter, CONNECT_TIMEOUT, CancelHandle, Capabilities, ConnectParams, DriverError, Result,
    SessionSettings, row_cap_reached,
};
use chrono::Utc;
use futures_util::StreamExt;
use herdr_db_core::cell::{Cell, Page, QueryOutcome, ResultColumn, RowSet, StatementOutcome};
use herdr_db_core::config::TlsMode;
use herdr_db_core::model::{
    Column, Constraint, ConstraintKind, Engine, ForeignKey, GeneratedColumn, Identity, Index, ObjectKind, ObjectRef,
    ObjectSummary, SchemaModel, TableDetail, Trigger,
};
use herdr_db_core::paging::PageRequest;
use herdr_db_core::statements;
use secrecy::ExposeSecret;
use std::pin::pin;
use std::time::Instant;
use tokio_postgres::config::SslMode;
use tokio_postgres::{CancelToken, Client, NoTls, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;

pub struct PgAdapter {
    client: Client,
    cancel: PgCancel,
    server_version: String,
}

#[derive(Clone)]
pub struct PgCancel {
    token: CancelToken,
    tls: Option<MakeRustlsConnect>,
}

impl PgCancel {
    pub async fn cancel(&self) -> Result<()> {
        let result = match &self.tls {
            Some(tls) => self.token.cancel_query(tls.clone()).await,
            None => self.token.cancel_query(NoTls).await,
        };
        result.map_err(|e| DriverError::Connect(e.to_string()))
    }
}

fn connect_error(e: tokio_postgres::Error) -> DriverError {
    match e.as_db_error() {
        Some(db) if db.code().code().starts_with("28") => DriverError::Auth(db.message().to_string()),
        Some(db) => DriverError::Connect(db.message().to_string()),
        // The server asked for a password and none was configured.
        None if e.to_string().contains("password missing") => DriverError::Auth("mot de passe requis".into()),
        None => DriverError::Connect(source_chain(&e)),
    }
}

/// tokio-postgres errors hide the useful part (refused, timeout) in `source`.
fn source_chain(e: &dyn std::error::Error) -> String {
    let mut message = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        source = s.source();
    }
    message
}

fn query_error(e: tokio_postgres::Error) -> DriverError {
    if let Some(db) = e.as_db_error() {
        let code = db.code().code().to_string();
        if code == "57014" && db.message().contains("user request") {
            return DriverError::Cancelled;
        }
        let mut message = db.message().to_string();
        if let Some(detail) = db.detail() {
            message.push_str(" — ");
            message.push_str(detail);
        }
        if let Some(hint) = db.hint() {
            message.push_str(" (indice : ");
            message.push_str(hint);
            message.push(')');
        }
        if let Some(tokio_postgres::error::ErrorPosition::Original(p)) = db.position() {
            message.push_str(&format!(" [position {p}]"));
        }
        return DriverError::Query { code: Some(code), message };
    }
    if e.is_closed() {
        return DriverError::ConnectionLost(source_chain(&e));
    }
    DriverError::Query { code: None, message: source_chain(&e) }
}

impl PgAdapter {
    pub async fn connect(params: ConnectParams) -> Result<PgAdapter> {
        let mut config = tokio_postgres::Config::new();
        config
            .host(&params.host)
            .port(params.port)
            .dbname(&params.database)
            .user(&params.user)
            .application_name(APPLICATION_NAME)
            .connect_timeout(CONNECT_TIMEOUT);
        if let Some(password) = &params.password {
            config.password(password.expose_secret());
        }

        let (client, tls) = match params.tls {
            TlsMode::Disable => {
                config.ssl_mode(SslMode::Disable);
                let (client, connection) = config.connect(NoTls).await.map_err(connect_error)?;
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        tracing::debug!(error = %e, "postgres connection closed");
                    }
                });
                (client, None)
            }
            mode => {
                config.ssl_mode(if mode == TlsMode::Prefer { SslMode::Prefer } else { SslMode::Require });
                let rustls = if mode == TlsMode::VerifyFull { verifying_config() } else { unverified_config() };
                let tls = MakeRustlsConnect::new(rustls);
                let (client, connection) = config.connect(tls.clone()).await.map_err(connect_error)?;
                tokio::spawn(async move {
                    if let Err(e) = connection.await {
                        tracing::debug!(error = %e, "postgres connection closed");
                    }
                });
                (client, Some(tls))
            }
        };
        drop(config);

        let server_version = client
            .query_one("SELECT current_setting('server_version')", &[])
            .await
            .ok()
            .and_then(|row| row.try_get::<_, String>(0).ok())
            .unwrap_or_default();
        let cancel = PgCancel { token: client.cancel_token(), tls };
        let adapter = PgAdapter { client, cancel, server_version };
        adapter.init_session(params.session).await?;
        Ok(adapter)
    }

    async fn init_session(&self, session: SessionSettings) -> Result<()> {
        let mut init = String::new();
        if session.read_only {
            init.push_str("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY;");
        }
        if let Some(timeout) = session.statement_timeout {
            init.push_str(&format!("SET statement_timeout = {};", timeout.as_millis()));
        }
        if !init.is_empty() {
            self.client.batch_execute(&init).await.map_err(query_error)?;
        }
        Ok(())
    }

    /// Runs `sql` with the simple protocol, keeping at most `max_rows` rows per
    /// statement; past the cap the statement is cancelled server side.
    async fn run_simple(&self, sql: &str, max_rows: usize, types: TypeHint<'_>) -> Result<Vec<StatementOutcome>> {
        let pieces = statements::split(Engine::Postgres, sql);
        let stream = self.client.simple_query_raw(sql).await.map_err(query_error)?;
        let mut stream = pin!(stream);
        let mut outcomes = Vec::new();
        let mut current: Option<(RowSet, bool)> = None;
        let mut cancelled = false;
        while let Some(message) = stream.next().await {
            match message {
                Ok(SimpleQueryMessage::RowDescription(columns)) => {
                    let columns = columns
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ResultColumn::new(c.name(), types.lookup(i, c.name(), columns.len())))
                        .collect();
                    current = Some((RowSet { columns, rows: Vec::new() }, false));
                }
                Ok(SimpleQueryMessage::Row(row)) => {
                    let Some((set, truncated)) = current.as_mut() else {
                        continue;
                    };
                    if !row_cap_reached(set.rows.len(), max_rows) {
                        let cells = set
                            .columns
                            .iter()
                            .enumerate()
                            .map(|(i, column)| match row.get(i) {
                                Some(text) => Cell::from_text(column.type_name.as_deref(), text.to_string()),
                                None => Cell::Null,
                            })
                            .collect();
                        set.rows.push(cells);
                    } else if !*truncated {
                        *truncated = true;
                        cancelled = true;
                        let _ = self.cancel.cancel().await;
                    }
                }
                Ok(SimpleQueryMessage::CommandComplete(affected)) => match current.take() {
                    Some((set, truncated)) => outcomes.push(StatementOutcome::Rows { set, truncated }),
                    None => outcomes.push(StatementOutcome::Command {
                        tag: command_tag(sql, pieces.get(outcomes.len()).cloned()),
                        affected: Some(affected),
                    }),
                },
                Ok(_) => {}
                Err(e) => {
                    let error = query_error(e);
                    if cancelled && error == DriverError::Cancelled {
                        if let Some((set, _)) = current.take() {
                            outcomes.push(StatementOutcome::Rows { set, truncated: true });
                        }
                        break;
                    }
                    return Err(error);
                }
            }
        }
        if let Some((set, truncated)) = current.take() {
            outcomes.push(StatementOutcome::Rows { set, truncated });
        }
        Ok(outcomes)
    }
}

/// Column types for the cells: from `prepare` (console) or the cached detail (grid).
enum TypeHint<'a> {
    None,
    Positional(Vec<String>),
    ByName(&'a std::collections::BTreeMap<String, String>),
}

impl TypeHint<'_> {
    fn lookup(&self, index: usize, name: &str, count: usize) -> Option<String> {
        match self {
            TypeHint::None => None,
            TypeHint::Positional(types) if types.len() == count => types.get(index).cloned(),
            TypeHint::Positional(_) => None,
            TypeHint::ByName(map) => map.get(name).cloned(),
        }
    }
}

/// `UPDATE`, `CREATE TABLE`... from the statement text (the simple protocol
/// only reports the affected row count).
fn command_tag(sql: &str, range: Option<std::ops::Range<usize>>) -> String {
    let text = range.map_or(sql, |r| &sql[r]);
    let words: Vec<String> =
        text.split_whitespace().filter(|w| !w.starts_with("--")).take(2).map(|w| w.to_ascii_uppercase()).collect();
    match words.first().map(String::as_str) {
        Some("CREATE" | "DROP" | "ALTER") => words.join(" "),
        Some(word) => word.to_string(),
        None => "OK".to_string(),
    }
}

fn relkind(kind: &str) -> ObjectKind {
    match kind {
        "p" => ObjectKind::PartitionedTable,
        "v" => ObjectKind::View,
        "m" => ObjectKind::MaterializedView,
        "f" => ObjectKind::ForeignTable,
        _ => ObjectKind::Table,
    }
}

fn fk_action(code: &str) -> Option<String> {
    Some(
        match code {
            "r" => "RESTRICT",
            "c" => "CASCADE",
            "n" => "SET NULL",
            "d" => "SET DEFAULT",
            _ => return None,
        }
        .to_string(),
    )
}

fn trigger_shape(tgtype: i32) -> (String, Vec<String>) {
    let timing = if tgtype & 2 != 0 {
        "BEFORE"
    } else if tgtype & 64 != 0 {
        "INSTEAD OF"
    } else {
        "AFTER"
    };
    let mut events = Vec::new();
    for (bit, name) in [(4, "INSERT"), (16, "UPDATE"), (8, "DELETE"), (32, "TRUNCATE")] {
        if tgtype & bit != 0 {
            events.push(name.to_string());
        }
    }
    (timing.to_string(), events)
}

impl Adapter for PgAdapter {
    fn capabilities(&self) -> Capabilities {
        Capabilities { row_comparison: true, transactional_ddl: true, native_ddl: false, materialized_views: true }
    }

    async fn list_schemas(&mut self) -> Result<Vec<String>> {
        let rows = self
            .client
            .query(
                "SELECT nspname::text FROM pg_namespace
                 WHERE nspname !~ '^pg_' AND nspname <> 'information_schema'
                 ORDER BY nspname",
                &[],
            )
            .await
            .map_err(query_error)?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    async fn introspect_schema(&mut self, schema: &str) -> Result<SchemaModel> {
        let rows = self
            .client
            .query(
                "SELECT c.relname::text, c.relkind::text, obj_description(c.oid, 'pg_class'),
                        CASE WHEN c.relkind = 'p' THEN
                               (SELECT sum(GREATEST(ch.reltuples, 0))::int8 FROM pg_inherits i
                                JOIN pg_class ch ON ch.oid = i.inhrelid WHERE i.inhparent = c.oid)
                             WHEN c.reltuples < 0 THEN NULL
                             ELSE c.reltuples::int8 END
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relkind IN ('r', 'p', 'v', 'm', 'f') AND NOT c.relispartition
                 ORDER BY c.relname",
                &[&schema],
            )
            .await
            .map_err(query_error)?;
        let objects = rows
            .iter()
            .map(|r| {
                let kind = relkind(r.get::<_, &str>(1));
                ObjectSummary {
                    name: r.get(0),
                    kind,
                    comment: r.get(2),
                    estimated_rows: if kind.is_view() && kind != ObjectKind::MaterializedView {
                        None
                    } else {
                        r.get(3)
                    },
                }
            })
            .collect();
        Ok(SchemaModel { name: schema.to_string(), introspected_at: Utc::now(), objects })
    }

    async fn introspect_table(&mut self, object: &ObjectRef) -> Result<TableDetail> {
        // Qualify every name the deparsers print: deterministic DDL whatever
        // the user's search_path.
        let tx = self.client.transaction().await.map_err(query_error)?;
        tx.batch_execute("SET LOCAL search_path = pg_catalog").await.map_err(query_error)?;
        let row = tx
            .query_opt(
                "SELECT c.oid, c.relkind::text, obj_description(c.oid, 'pg_class'),
                        CASE WHEN c.relkind = 'p' THEN pg_get_partkeydef(c.oid) END,
                        CASE WHEN c.relkind IN ('v', 'm') THEN pg_get_viewdef(c.oid, true) END
                 FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                 WHERE n.nspname = $1 AND c.relname = $2",
                &[&object.schema, &object.name],
            )
            .await
            .map_err(query_error)?
            .ok_or_else(|| DriverError::NotFound(object.to_string()))?;
        let oid: u32 = row.get(0);
        let kind = relkind(row.get::<_, &str>(1));
        let comment: Option<String> = row.get(2);
        let partition_key: Option<String> = row.get(3);
        let view_definition: Option<String> = row.get(4);

        let columns = tx
            .query(
                "SELECT a.attname::text, a.attnum::int4, format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                        pg_get_expr(d.adbin, d.adrelid), col_description(a.attrelid, a.attnum),
                        a.attgenerated::text, a.attidentity::text
                 FROM pg_attribute a
                 LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
                 WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped
                 ORDER BY a.attnum",
                &[&oid],
            )
            .await
            .map_err(query_error)?
            .iter()
            .map(|r| {
                let default: Option<String> = r.get(4);
                let generated: &str = r.get(6);
                let identity: &str = r.get(7);
                let generated = match generated {
                    "s" | "v" => Some(GeneratedColumn {
                        expression: default.clone().unwrap_or_default(),
                        stored: generated == "s",
                    }),
                    _ => None,
                };
                Column {
                    name: r.get(0),
                    ordinal: r.get::<_, i32>(1) as u32,
                    data_type: r.get(2),
                    nullable: r.get(3),
                    default: if generated.is_some() { None } else { default },
                    comment: r.get(5),
                    generated,
                    identity: match identity {
                        "a" => Some(Identity::Always),
                        "d" => Some(Identity::ByDefault),
                        _ => None,
                    },
                    extra: None,
                }
            })
            .collect();

        let indexes = tx
            .query(
                "SELECT ic.relname::text, i.indisunique, i.indisprimary, am.amname::text,
                        pg_get_indexdef(i.indexrelid), con.conname::text,
                        ARRAY(SELECT CASE WHEN i.indkey[k - 1] = 0
                                          THEN pg_get_indexdef(i.indexrelid, k, true)
                                          ELSE (SELECT a.attname::text FROM pg_attribute a
                                                WHERE a.attrelid = i.indrelid AND a.attnum = i.indkey[k - 1])
                                     END
                              FROM generate_series(1, i.indnkeyatts) AS k ORDER BY k)
                 FROM pg_index i
                 JOIN pg_class ic ON ic.oid = i.indexrelid
                 JOIN pg_am am ON am.oid = ic.relam
                 LEFT JOIN pg_constraint con ON con.conindid = i.indexrelid AND con.conrelid = i.indrelid
                                            AND con.contype IN ('p', 'u', 'x')
                 WHERE i.indrelid = $1
                 ORDER BY ic.relname",
                &[&oid],
            )
            .await
            .map_err(query_error)?
            .iter()
            .map(|r| Index {
                name: r.get(0),
                unique: r.get(1),
                primary: r.get(2),
                method: r.get(3),
                definition: r.get(4),
                constraint: r.get(5),
                columns: r.get(6),
            })
            .collect();

        let constraints = tx
            .query(
                "SELECT con.conname::text, con.contype::text, pg_get_constraintdef(con.oid),
                        ARRAY(SELECT a.attname::text
                              FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
                              JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum
                              ORDER BY k.ord)
                 FROM pg_constraint con
                 WHERE con.conrelid = $1 AND con.contype IN ('p', 'u', 'c', 'x')
                 ORDER BY con.conname",
                &[&oid],
            )
            .await
            .map_err(query_error)?
            .iter()
            .map(|r| Constraint {
                name: r.get(0),
                kind: match r.get::<_, &str>(1) {
                    "p" => ConstraintKind::PrimaryKey,
                    "u" => ConstraintKind::Unique,
                    "x" => ConstraintKind::Exclusion,
                    _ => ConstraintKind::Check,
                },
                definition: r.get(2),
                columns: r.get(3),
            })
            .collect();

        let foreign_keys = tx
            .query(
                "SELECT con.conname::text, pg_get_constraintdef(con.oid),
                        ARRAY(SELECT a.attname::text
                              FROM unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord)
                              JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = k.attnum
                              ORDER BY k.ord),
                        rn.nspname::text, rc.relname::text,
                        ARRAY(SELECT a.attname::text
                              FROM unnest(con.confkey) WITH ORDINALITY AS k(attnum, ord)
                              JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = k.attnum
                              ORDER BY k.ord),
                        con.confupdtype::text, con.confdeltype::text
                 FROM pg_constraint con
                 JOIN pg_class rc ON rc.oid = con.confrelid
                 JOIN pg_namespace rn ON rn.oid = rc.relnamespace
                 WHERE con.conrelid = $1 AND con.contype = 'f'
                 ORDER BY con.conname",
                &[&oid],
            )
            .await
            .map_err(query_error)?
            .iter()
            .map(|r| ForeignKey {
                name: r.get(0),
                definition: r.get(1),
                columns: r.get(2),
                ref_schema: r.get(3),
                ref_table: r.get(4),
                ref_columns: r.get(5),
                on_update: fk_action(r.get(6)),
                on_delete: fk_action(r.get(7)),
            })
            .collect();

        let triggers = tx
            .query(
                "SELECT t.tgname::text, pg_get_triggerdef(t.oid), t.tgtype::int4
                 FROM pg_trigger t
                 WHERE t.tgrelid = $1 AND NOT t.tgisinternal
                 ORDER BY t.tgname",
                &[&oid],
            )
            .await
            .map_err(query_error)?
            .iter()
            .map(|r| {
                let (timing, events) = trigger_shape(r.get(2));
                Trigger { name: r.get(0), definition: r.get(1), timing, events }
            })
            .collect();
        tx.commit().await.map_err(query_error)?;

        Ok(TableDetail {
            kind,
            columns,
            indexes,
            foreign_keys,
            constraints,
            triggers,
            comment,
            partition_key,
            view_definition,
            native_ddl: None,
        })
    }

    async fn fetch_page(&mut self, request: &PageRequest) -> Result<Page> {
        let query = request.to_sql();
        let start = Instant::now();
        let outcomes =
            self.run_simple(&query.sql, request.page_size + 1, TypeHint::ByName(&request.column_types)).await?;
        let mut set = match outcomes.into_iter().next() {
            Some(StatementOutcome::Rows { set, .. }) => set,
            _ => RowSet::default(),
        };
        let has_more = set.rows.len() > request.page_size;
        set.rows.truncate(request.page_size);
        if query.reversed {
            set.rows.reverse();
        }
        Ok(Page { rows: set, has_more, elapsed: start.elapsed() })
    }

    async fn count(&mut self, sql: &str) -> Result<u64> {
        let outcomes = self.run_simple(sql, 1, TypeHint::None).await?;
        match outcomes.first() {
            Some(StatementOutcome::Rows { set, .. }) => Ok(set
                .rows
                .first()
                .and_then(|r| r.first())
                .and_then(|c| c.copy_text())
                .and_then(|t| t.parse().ok())
                .unwrap_or(0)),
            _ => Ok(0),
        }
    }

    async fn execute(&mut self, sql: &str, max_rows: usize) -> Result<QueryOutcome> {
        let start = Instant::now();
        // Column types: the simple protocol only returns names. A lone
        // statement is prepared first (parsed, not executed) to learn them.
        let types = if statements::split(Engine::Postgres, sql).len() == 1 {
            match self.client.prepare(sql).await {
                Ok(statement) => {
                    TypeHint::Positional(statement.columns().iter().map(|c| c.type_().name().to_string()).collect())
                }
                Err(_) => TypeHint::None,
            }
        } else {
            TypeHint::None
        };
        let statements = self.run_simple(sql, max_rows, types).await?;
        Ok(QueryOutcome { statements, elapsed: start.elapsed() })
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::Postgres(self.cancel.clone())
    }

    async fn set_read_only(&mut self, read_only: bool) -> Result<()> {
        let sql = if read_only {
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"
        } else {
            "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"
        };
        self.client.batch_execute(sql).await.map_err(query_error)
    }

    fn server_version(&self) -> String {
        self.server_version.clone()
    }

    fn is_closed(&self) -> bool {
        self.client.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_and_trigger_shapes() {
        assert_eq!(command_tag("update t set a = 1", None), "UPDATE");
        assert_eq!(command_tag("create table x (a int)", None), "CREATE TABLE");
        assert_eq!(trigger_shape(2 | 16 | 1), ("BEFORE".into(), vec!["UPDATE".into()]));
        assert_eq!(trigger_shape(4 | 8), ("AFTER".into(), vec!["INSERT".into(), "DELETE".into()]));
    }
}
