//! MySQL adapter: mysql_async. Introspection reads `information_schema`,
//! DDL comes from `SHOW CREATE`. Cancellation runs `KILL QUERY` from a control
//! connection opened alongside the main one, so the password is not kept
//! around for later.

use crate::tls::install_crypto_provider;
use crate::{
    APPLICATION_NAME, Adapter, CONNECT_TIMEOUT, CancelHandle, Capabilities, ConnectParams, DriverError, Result,
    SessionSettings, row_cap_reached,
};
use chrono::Utc;
use herdr_db_core::cell::{Cell, Page, QueryOutcome, ResultColumn, RowSet, StatementOutcome};
use herdr_db_core::config::TlsMode;
use herdr_db_core::model::{
    Column, Constraint, ConstraintKind, Engine, ForeignKey, GeneratedColumn, Index, ObjectKind, ObjectRef,
    ObjectSummary, SchemaModel, TableDetail, Trigger,
};
use herdr_db_core::paging::PageRequest;
use herdr_db_core::sql::qualified_quoted;
use herdr_db_core::statements;
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder, Row, SslOpts, Value};
use secrecy::ExposeSecret;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;

pub struct MyAdapter {
    conn: Conn,
    cancel: MyCancel,
    server_version: String,
}

#[derive(Clone)]
pub struct MyCancel {
    control: Arc<Mutex<Option<Conn>>>,
    connection_id: u32,
}

impl MyCancel {
    pub async fn cancel(&self) -> Result<()> {
        let mut control = self.control.lock().await;
        let conn =
            control.as_mut().ok_or_else(|| DriverError::ConnectionLost("connexion de contrôle absente".into()))?;
        conn.query_drop(format!("KILL QUERY {}", self.connection_id)).await.map_err(query_error)
    }
}

fn server_error(code: u16, message: String) -> DriverError {
    match code {
        1045 | 1044 | 1698 => DriverError::Auth(message),
        1317 => DriverError::Cancelled,
        _ => DriverError::Query { code: Some(code.to_string()), message },
    }
}

fn connect_error(e: mysql_async::Error) -> DriverError {
    match e {
        mysql_async::Error::Server(s) => match server_error(s.code, s.message) {
            DriverError::Query { message, .. } => DriverError::Connect(message),
            other => other,
        },
        other => DriverError::Connect(other.to_string()),
    }
}

fn query_error(e: mysql_async::Error) -> DriverError {
    match e {
        mysql_async::Error::Server(s) => server_error(s.code, s.message),
        mysql_async::Error::Io(io) => DriverError::ConnectionLost(io.to_string()),
        other => DriverError::Query { code: None, message: other.to_string() },
    }
}

fn builder(params: &ConnectParams) -> OptsBuilder {
    let mut attributes = HashMap::new();
    attributes.insert("program_name".to_string(), APPLICATION_NAME.to_string());
    OptsBuilder::default()
        .ip_or_hostname(params.host.clone())
        .tcp_port(params.port)
        .user(Some(params.user.clone()))
        .pass(params.password.as_ref().map(|p| p.expose_secret().to_string()))
        .db_name(Some(params.database.clone()))
        .prefer_socket(false)
        .connect_attributes(attributes)
}

async fn open(builder: OptsBuilder) -> std::result::Result<Conn, mysql_async::Error> {
    match tokio::time::timeout(CONNECT_TIMEOUT, Conn::new(builder)).await {
        Ok(result) => result,
        Err(_) => Err(mysql_async::Error::Other("délai de connexion dépassé".into())),
    }
}

impl MyAdapter {
    pub async fn connect(params: ConnectParams) -> Result<MyAdapter> {
        install_crypto_provider();
        let base = builder(&params);
        let unverified =
            SslOpts::default().with_danger_accept_invalid_certs(true).with_danger_skip_domain_validation(true);
        let (conn, opts) = match params.tls {
            TlsMode::Disable => (open(base.clone()).await.map_err(connect_error)?, base),
            TlsMode::Require => {
                let opts = base.ssl_opts(unverified);
                (open(opts.clone()).await.map_err(connect_error)?, opts)
            }
            TlsMode::VerifyFull => {
                let opts = base.ssl_opts(SslOpts::default());
                (open(opts.clone()).await.map_err(connect_error)?, opts)
            }
            TlsMode::Prefer => {
                let tls = base.clone().ssl_opts(unverified);
                match open(tls.clone()).await {
                    Ok(conn) => (conn, tls),
                    Err(mysql_async::Error::Driver(mysql_async::DriverError::NoClientSslFlagFromServer)) => {
                        (open(base.clone()).await.map_err(connect_error)?, base)
                    }
                    Err(e) => return Err(connect_error(e)),
                }
            }
        };
        // Control connection for KILL QUERY; not critical if it fails.
        let control = open(opts).await.ok();
        let session = params.session;
        drop(params);

        let mut conn = conn;
        let server_version: String = conn.query_first("SELECT VERSION()").await.ok().flatten().unwrap_or_default();
        let cancel = MyCancel { connection_id: conn.id(), control: Arc::new(Mutex::new(control)) };
        let mut adapter = MyAdapter { conn, cancel, server_version };
        adapter.apply_session(session).await?;
        Ok(adapter)
    }

    fn is_mariadb(&self) -> bool {
        self.server_version.to_ascii_lowercase().contains("mariadb")
    }

    async fn apply_session(&mut self, session: SessionSettings) -> Result<()> {
        if session.read_only {
            self.conn.query_drop("SET SESSION TRANSACTION READ ONLY").await.map_err(query_error)?;
        }
        if let Some(timeout) = session.statement_timeout {
            let sql = if self.is_mariadb() {
                format!("SET SESSION max_statement_time = {}", timeout.as_secs_f64())
            } else {
                // Only applies to SELECT statements.
                format!("SET SESSION max_execution_time = {}", timeout.as_millis())
            };
            self.conn.query_drop(sql).await.map_err(query_error)?;
        }
        Ok(())
    }

    async fn rows(&mut self, sql: &str, params: Vec<Value>) -> Result<Vec<Row>> {
        self.conn.exec(sql, params).await.map_err(query_error)
    }

    /// Streams `sql` with the text protocol, keeping at most `max_rows` rows
    /// per result set; past the cap the query is killed server side.
    async fn run(&mut self, sql: &str, max_rows: usize) -> Result<Vec<StatementOutcome>> {
        let cancel = self.cancel.clone();
        let pieces = statements::split(Engine::MySql, sql);
        let mut result = self.conn.query_iter(sql).await.map_err(query_error)?;
        let mut outcomes = Vec::new();
        let mut cancelled = false;
        'sets: loop {
            match result.columns().filter(|c| !c.is_empty()) {
                Some(columns) => {
                    let set_columns: Vec<ResultColumn> =
                        columns.iter().map(|c| ResultColumn::new(c.name_str(), Some(type_name(c)))).collect();
                    let mut rows = Vec::new();
                    let mut truncated = false;
                    loop {
                        match result.next().await {
                            Ok(Some(row)) => {
                                if !row_cap_reached(rows.len(), max_rows) {
                                    rows.push(
                                        row.unwrap().iter().zip(columns.iter()).map(|(v, c)| cell(v, c)).collect(),
                                    );
                                } else if !truncated {
                                    truncated = true;
                                    cancelled = true;
                                    let _ = cancel.cancel().await;
                                }
                            }
                            Ok(None) => break,
                            Err(e) => {
                                let error = query_error(e);
                                if cancelled && error == DriverError::Cancelled {
                                    outcomes.push(StatementOutcome::Rows {
                                        set: RowSet { columns: set_columns, rows },
                                        truncated: true,
                                    });
                                    break 'sets;
                                }
                                return Err(error);
                            }
                        }
                    }
                    outcomes.push(StatementOutcome::Rows { set: RowSet { columns: set_columns, rows }, truncated });
                }
                None => {
                    outcomes.push(StatementOutcome::Command {
                        tag: command_tag(sql, pieces.get(outcomes.len()).cloned()),
                        affected: Some(result.affected_rows()),
                    });
                    if result.is_empty() {
                        break;
                    }
                    result.next().await.map_err(query_error)?;
                    continue;
                }
            }
            if result.is_empty() {
                break;
            }
        }
        if cancelled {
            // The killed query may still report its interruption: drain it.
            let _ = result.drop_result().await;
        } else {
            result.drop_result().await.map_err(query_error)?;
        }
        Ok(outcomes)
    }
}

fn command_tag(sql: &str, range: Option<std::ops::Range<usize>>) -> String {
    let text = range.map_or(sql, |r| &sql[r]);
    let words: Vec<String> = text.split_whitespace().take(2).map(|w| w.to_ascii_uppercase()).collect();
    match words.first().map(String::as_str) {
        Some("CREATE" | "DROP" | "ALTER") => words.join(" "),
        Some(word) => word.to_string(),
        None => "OK".to_string(),
    }
}

/// Binary character set (`binary`, id 63) on a string or blob column.
fn is_binary(column: &mysql_async::Column) -> bool {
    column.character_set() == 63
        && matches!(
            column.column_type(),
            ColumnType::MYSQL_TYPE_TINY_BLOB
                | ColumnType::MYSQL_TYPE_MEDIUM_BLOB
                | ColumnType::MYSQL_TYPE_LONG_BLOB
                | ColumnType::MYSQL_TYPE_BLOB
                | ColumnType::MYSQL_TYPE_VAR_STRING
                | ColumnType::MYSQL_TYPE_STRING
                | ColumnType::MYSQL_TYPE_VARCHAR
                | ColumnType::MYSQL_TYPE_GEOMETRY
        )
}

fn type_name(column: &mysql_async::Column) -> String {
    use ColumnType::*;
    let unsigned = column.flags().contains(ColumnFlags::UNSIGNED_FLAG);
    let base =
        match column.column_type() {
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => "decimal",
            MYSQL_TYPE_TINY => "tinyint",
            MYSQL_TYPE_SHORT => "smallint",
            MYSQL_TYPE_INT24 => "mediumint",
            MYSQL_TYPE_LONG => "int",
            MYSQL_TYPE_LONGLONG => "bigint",
            MYSQL_TYPE_FLOAT => "float",
            MYSQL_TYPE_DOUBLE => "double",
            MYSQL_TYPE_NULL => "null",
            MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => "timestamp",
            MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => "date",
            MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => "time",
            MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => "datetime",
            MYSQL_TYPE_YEAR => "year",
            MYSQL_TYPE_BIT => "bit",
            MYSQL_TYPE_JSON => "json",
            MYSQL_TYPE_ENUM => "enum",
            MYSQL_TYPE_SET => "set",
            MYSQL_TYPE_GEOMETRY => "geometry",
            MYSQL_TYPE_VECTOR => "vector",
            MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
                if is_binary(column) { "blob" } else { "text" }
            }
            MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_VARCHAR => {
                if is_binary(column) {
                    "varbinary"
                } else {
                    "varchar"
                }
            }
            MYSQL_TYPE_STRING => {
                if is_binary(column) {
                    "binary"
                } else {
                    "char"
                }
            }
            _ => "unknown",
        };
    if unsigned { format!("{base} unsigned") } else { base.to_string() }
}

fn cell(value: &Value, column: &mysql_async::Column) -> Cell {
    match value {
        Value::NULL => Cell::Null,
        Value::Bytes(bytes) => {
            if column.column_type() == ColumnType::MYSQL_TYPE_JSON {
                Cell::Json(String::from_utf8_lossy(bytes).into_owned())
            } else if column.column_type() == ColumnType::MYSQL_TYPE_BIT {
                let n = bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
                Cell::Text(n.to_string())
            } else if is_binary(column) {
                Cell::Binary { len: bytes.len() }
            } else {
                Cell::Text(String::from_utf8_lossy(bytes).into_owned())
            }
        }
        Value::Int(i) => Cell::Text(i.to_string()),
        Value::UInt(u) => Cell::Text(u.to_string()),
        Value::Float(f) => Cell::Text(f.to_string()),
        Value::Double(d) => Cell::Text(d.to_string()),
        Value::Date(y, m, d, h, mi, s, us) => Cell::Text(if *us > 0 {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}.{us:06}")
        } else {
            format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}")
        }),
        Value::Time(negative, days, h, m, s, us) => {
            let hours = u32::from(*h) + days * 24;
            let sign = if *negative { "-" } else { "" };
            Cell::Text(if *us > 0 {
                format!("{sign}{hours:02}:{m:02}:{s:02}.{us:06}")
            } else {
                format!("{sign}{hours:02}:{m:02}:{s:02}")
            })
        }
    }
}

/// Text of a catalog value; information_schema columns come back as bytes.
fn text(row: &Row, index: usize) -> Option<String> {
    match row.as_ref(index)? {
        Value::NULL => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::Int(i) => Some(i.to_string()),
        Value::UInt(u) => Some(u.to_string()),
        other => Some(format!("{other:?}")),
    }
}

fn number(row: &Row, index: usize) -> Option<i64> {
    text(row, index).and_then(|t| t.parse().ok())
}

impl Adapter for MyAdapter {
    fn capabilities(&self) -> Capabilities {
        Capabilities { row_comparison: true, transactional_ddl: false, native_ddl: true, materialized_views: false }
    }

    async fn list_schemas(&mut self) -> Result<Vec<String>> {
        let rows = self
            .rows(
                "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA
                 WHERE SCHEMA_NAME NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys')
                 ORDER BY SCHEMA_NAME",
                vec![],
            )
            .await?;
        Ok(rows.iter().filter_map(|r| text(r, 0)).collect())
    }

    async fn introspect_schema(&mut self, schema: &str) -> Result<SchemaModel> {
        let rows = self
            .rows(
                "SELECT TABLE_NAME, TABLE_TYPE, TABLE_COMMENT, TABLE_ROWS FROM information_schema.TABLES
                 WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME",
                vec![schema.into()],
            )
            .await?;
        let objects = rows
            .iter()
            .filter_map(|r| {
                let name = text(r, 0)?;
                let view = text(r, 1).is_some_and(|t| t.contains("VIEW"));
                Some(ObjectSummary {
                    name,
                    kind: if view { ObjectKind::View } else { ObjectKind::Table },
                    comment: if view { None } else { text(r, 2).filter(|c| !c.is_empty()) },
                    estimated_rows: if view { None } else { number(r, 3) },
                })
            })
            .collect();
        Ok(SchemaModel { name: schema.to_string(), introspected_at: Utc::now(), objects })
    }

    async fn introspect_table(&mut self, object: &ObjectRef) -> Result<TableDetail> {
        let key = || vec![Value::from(object.schema.as_str()), Value::from(object.name.as_str())];
        let info = self
            .rows(
                "SELECT TABLE_TYPE, TABLE_COMMENT FROM information_schema.TABLES
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?",
                key(),
            )
            .await?;
        let info = info.first().ok_or_else(|| DriverError::NotFound(object.to_string()))?;
        let view = text(info, 0).is_some_and(|t| t.contains("VIEW"));
        let comment = if view { None } else { text(info, 1).filter(|c| !c.is_empty()) };

        let columns = self
            .rows(
                "SELECT COLUMN_NAME, ORDINAL_POSITION, COLUMN_TYPE, IS_NULLABLE, COLUMN_DEFAULT,
                        COLUMN_COMMENT, EXTRA, GENERATION_EXPRESSION
                 FROM information_schema.COLUMNS
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
                key(),
            )
            .await?
            .iter()
            .map(|r| {
                let extra = text(r, 6).unwrap_or_default();
                let expression = text(r, 7).filter(|e| !e.is_empty());
                let generated = if extra.contains("GENERATED") && !extra.contains("DEFAULT_GENERATED") {
                    expression.map(|expression| GeneratedColumn { expression, stored: extra.contains("STORED") })
                } else {
                    None
                };
                Column {
                    name: text(r, 0).unwrap_or_default(),
                    ordinal: number(r, 1).unwrap_or(0) as u32,
                    data_type: text(r, 2).unwrap_or_default(),
                    nullable: text(r, 3).as_deref() == Some("YES"),
                    default: text(r, 4),
                    comment: text(r, 5).filter(|c| !c.is_empty()),
                    generated,
                    identity: None,
                    extra: Some(extra).filter(|e| !e.is_empty()),
                }
            })
            .collect();

        let mut indexes: Vec<Index> = Vec::new();
        for r in self
            .rows(
                "SELECT INDEX_NAME, NON_UNIQUE, COLUMN_NAME, INDEX_TYPE FROM information_schema.STATISTICS
                 WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY INDEX_NAME, SEQ_IN_INDEX",
                key(),
            )
            .await?
        {
            let name = text(&r, 0).unwrap_or_default();
            let column = text(&r, 2).unwrap_or_else(|| "<expression>".to_string());
            match indexes.iter_mut().find(|i| i.name == name) {
                Some(index) => index.columns.push(column),
                None => {
                    let primary = name == "PRIMARY";
                    indexes.push(Index {
                        unique: number(&r, 1) == Some(0),
                        primary,
                        method: text(&r, 3),
                        definition: None,
                        constraint: primary.then(|| name.clone()),
                        columns: vec![column],
                        name,
                    });
                }
            }
        }

        let mut constraints: Vec<Constraint> = Vec::new();
        for r in self
            .rows(
                "SELECT tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, k.COLUMN_NAME
                 FROM information_schema.TABLE_CONSTRAINTS tc
                 LEFT JOIN information_schema.KEY_COLUMN_USAGE k
                   ON k.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA AND k.CONSTRAINT_NAME = tc.CONSTRAINT_NAME
                  AND k.TABLE_SCHEMA = tc.TABLE_SCHEMA AND k.TABLE_NAME = tc.TABLE_NAME
                 WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ?
                   AND tc.CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE', 'CHECK')
                 ORDER BY tc.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                key(),
            )
            .await?
        {
            let name = text(&r, 0).unwrap_or_default();
            let kind = match text(&r, 1).as_deref() {
                Some("PRIMARY KEY") => ConstraintKind::PrimaryKey,
                Some("UNIQUE") => ConstraintKind::Unique,
                _ => ConstraintKind::Check,
            };
            let column = text(&r, 2);
            match constraints.iter_mut().find(|c| c.name == name && c.kind == kind) {
                Some(c) => c.columns.extend(column),
                None => {
                    constraints.push(Constraint { name, kind, columns: column.into_iter().collect(), definition: None })
                }
            }
        }
        // CHECK_CONSTRAINTS exists from MySQL 8.0.16 / MariaDB 10.2.
        if constraints.iter().any(|c| c.kind == ConstraintKind::Check)
            && let Ok(rows) = self
                .rows(
                    "SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS cc
                     JOIN information_schema.TABLE_CONSTRAINTS tc
                       ON tc.CONSTRAINT_SCHEMA = cc.CONSTRAINT_SCHEMA AND tc.CONSTRAINT_NAME = cc.CONSTRAINT_NAME
                     WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? AND tc.CONSTRAINT_TYPE = 'CHECK'",
                    key(),
                )
                .await
        {
            for r in rows {
                let name = text(&r, 0).unwrap_or_default();
                if let Some(c) = constraints.iter_mut().find(|c| c.kind == ConstraintKind::Check && c.name == name) {
                    c.definition = text(&r, 1).map(|clause| format!("CHECK ({clause})"));
                }
            }
        }

        let mut foreign_keys: Vec<ForeignKey> = Vec::new();
        for r in self
            .rows(
                "SELECT k.CONSTRAINT_NAME, k.COLUMN_NAME, k.REFERENCED_TABLE_SCHEMA, k.REFERENCED_TABLE_NAME,
                        k.REFERENCED_COLUMN_NAME, rc.UPDATE_RULE, rc.DELETE_RULE
                 FROM information_schema.KEY_COLUMN_USAGE k
                 JOIN information_schema.REFERENTIAL_CONSTRAINTS rc
                   ON rc.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND rc.CONSTRAINT_NAME = k.CONSTRAINT_NAME
                  AND rc.TABLE_NAME = k.TABLE_NAME
                 WHERE k.TABLE_SCHEMA = ? AND k.TABLE_NAME = ? AND k.REFERENCED_TABLE_NAME IS NOT NULL
                 ORDER BY k.CONSTRAINT_NAME, k.ORDINAL_POSITION",
                key(),
            )
            .await?
        {
            let name = text(&r, 0).unwrap_or_default();
            let column = text(&r, 1).unwrap_or_default();
            let ref_column = text(&r, 4).unwrap_or_default();
            match foreign_keys.iter_mut().find(|f| f.name == name) {
                Some(fk) => {
                    fk.columns.push(column);
                    fk.ref_columns.push(ref_column);
                }
                None => {
                    let rule = |i| text(&r, i).filter(|v| v != "NO ACTION" && v != "RESTRICT");
                    foreign_keys.push(ForeignKey {
                        name,
                        columns: vec![column],
                        ref_schema: text(&r, 2).unwrap_or_default(),
                        ref_table: text(&r, 3).unwrap_or_default(),
                        ref_columns: vec![ref_column],
                        on_update: rule(5),
                        on_delete: rule(6),
                        definition: None,
                    })
                }
            }
        }

        let triggers = self
            .rows(
                "SELECT TRIGGER_NAME, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT
                 FROM information_schema.TRIGGERS
                 WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY TRIGGER_NAME",
                key(),
            )
            .await?
            .iter()
            .map(|r| Trigger {
                name: text(r, 0).unwrap_or_default(),
                timing: text(r, 1).unwrap_or_default(),
                events: text(r, 2).into_iter().collect(),
                definition: text(r, 3),
            })
            .collect();

        let show = format!(
            "SHOW CREATE {} {}",
            if view { "VIEW" } else { "TABLE" },
            qualified_quoted(Engine::MySql, &object.schema, &object.name)
        );
        let native_ddl = match self.conn.query_first::<Row, _>(show).await {
            Ok(Some(row)) => text(&row, 1),
            _ => None,
        };

        Ok(TableDetail {
            kind: if view { ObjectKind::View } else { ObjectKind::Table },
            columns,
            indexes,
            foreign_keys,
            constraints,
            triggers,
            comment,
            partition_key: None,
            view_definition: None,
            native_ddl,
        })
    }

    async fn fetch_page(&mut self, request: &PageRequest) -> Result<Page> {
        let query = request.to_sql();
        let start = Instant::now();
        let outcomes = self.run(&query.sql, request.page_size + 1).await?;
        let mut set = match outcomes.into_iter().next() {
            Some(StatementOutcome::Rows { set, .. }) => set,
            _ => RowSet::default(),
        };
        // Prefer the declared column type (`varchar(255)`) over the wire type.
        for column in &mut set.columns {
            if let Some(declared) = request.column_types.get(&column.name) {
                column.type_name = Some(declared.clone());
            }
        }
        let has_more = set.rows.len() > request.page_size;
        set.rows.truncate(request.page_size);
        if query.reversed {
            set.rows.reverse();
        }
        Ok(Page { rows: set, has_more, elapsed: start.elapsed() })
    }

    async fn count(&mut self, sql: &str) -> Result<u64> {
        let value: Option<u64> = self.conn.query_first(sql).await.map_err(query_error)?;
        Ok(value.unwrap_or(0))
    }

    async fn execute(&mut self, sql: &str, max_rows: usize) -> Result<QueryOutcome> {
        let start = Instant::now();
        let statements = self.run(sql, max_rows).await?;
        Ok(QueryOutcome { statements, elapsed: start.elapsed() })
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::MySql(self.cancel.clone())
    }

    async fn set_read_only(&mut self, read_only: bool) -> Result<()> {
        let sql = if read_only { "SET SESSION TRANSACTION READ ONLY" } else { "SET SESSION TRANSACTION READ WRITE" };
        self.conn.query_drop(sql).await.map_err(query_error)
    }

    fn server_version(&self) -> String {
        self.server_version.clone()
    }

    fn is_closed(&self) -> bool {
        false
    }
}
