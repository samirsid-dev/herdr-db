//! Native database adapters. A database client runs arbitrary queries whose
//! shape and types are unknown at compile time, so each engine gets a native
//! driver (tokio-postgres, mysql_async) behind a common interface. The engine
//! is known at runtime: [`AnyAdapter`] dispatches by enum, not trait object.

#![allow(async_fn_in_trait)]

mod mysql;
mod postgres;
mod tls;

pub use mysql::MyAdapter;
pub use postgres::PgAdapter;

use herdr_db_core::cell::{Page, QueryOutcome};
use herdr_db_core::config::TlsMode;
use herdr_db_core::model::{Engine, ObjectRef, SchemaModel, TableDetail};
use herdr_db_core::paging::PageRequest;
use secrecy::SecretString;
use std::time::Duration;

/// Shown in `pg_stat_activity` and the MySQL process list.
pub const APPLICATION_NAME: &str = "herdr-db";
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

pub struct ConnectParams {
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: Option<SecretString>,
    pub tls: TlsMode,
    pub session: SessionSettings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionSettings {
    pub read_only: bool,
    pub statement_timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DriverError {
    #[error("connexion impossible : {0}")]
    Connect(String),
    #[error("authentification refusée : {0}")]
    Auth(String),
    #[error("{}", format_query_error(.code, .message))]
    Query { code: Option<String>, message: String },
    #[error("connexion perdue : {0}")]
    ConnectionLost(String),
    #[error("requête annulée")]
    Cancelled,
    #[error("{0} n'existe plus")]
    NotFound(String),
}

fn format_query_error(code: &Option<String>, message: &str) -> String {
    match code {
        Some(code) => format!("{code} : {message}"),
        None => message.to_string(),
    }
}

impl DriverError {
    pub fn is_connection_lost(&self) -> bool {
        matches!(self, DriverError::ConnectionLost(_))
    }
}

pub type Result<T> = std::result::Result<T, DriverError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// `(a, b) > (x, y)` row comparisons for keyset pagination.
    pub row_comparison: bool,
    /// DDL statements run inside transactions.
    pub transactional_ddl: bool,
    /// The engine produces its own DDL (`SHOW CREATE TABLE`).
    pub native_ddl: bool,
    /// Materialized views exist.
    pub materialized_views: bool,
}

/// Contract every engine fulfils.
pub trait Adapter {
    fn capabilities(&self) -> Capabilities;

    async fn list_schemas(&mut self) -> Result<Vec<String>>;
    async fn introspect_schema(&mut self, schema: &str) -> Result<SchemaModel>;
    async fn introspect_table(&mut self, object: &ObjectRef) -> Result<TableDetail>;

    async fn fetch_page(&mut self, request: &PageRequest) -> Result<Page>;
    async fn count(&mut self, sql: &str) -> Result<u64>;
    async fn execute(&mut self, sql: &str, max_rows: usize) -> Result<QueryOutcome>;
    fn cancel_handle(&self) -> CancelHandle;

    async fn set_read_only(&mut self, read_only: bool) -> Result<()>;
    fn server_version(&self) -> String;
    fn is_closed(&self) -> bool;
}

// One adapter per pane, created once: the size difference is irrelevant.
#[allow(clippy::large_enum_variant)]
pub enum AnyAdapter {
    Postgres(PgAdapter),
    MySql(MyAdapter),
}

macro_rules! dispatch {
    ($self:ident, $a:ident => $body:expr) => {
        match $self {
            AnyAdapter::Postgres($a) => $body,
            AnyAdapter::MySql($a) => $body,
        }
    };
}

impl AnyAdapter {
    /// Connects and initializes the session (read-only, timeout, application name).
    pub async fn connect(params: ConnectParams) -> Result<AnyAdapter> {
        tls::install_crypto_provider();
        match params.engine {
            Engine::Postgres => Ok(AnyAdapter::Postgres(PgAdapter::connect(params).await?)),
            Engine::MySql => Ok(AnyAdapter::MySql(MyAdapter::connect(params).await?)),
        }
    }

    pub fn engine(&self) -> Engine {
        match self {
            AnyAdapter::Postgres(_) => Engine::Postgres,
            AnyAdapter::MySql(_) => Engine::MySql,
        }
    }
}

impl Adapter for AnyAdapter {
    fn capabilities(&self) -> Capabilities {
        dispatch!(self, a => a.capabilities())
    }
    async fn list_schemas(&mut self) -> Result<Vec<String>> {
        dispatch!(self, a => a.list_schemas().await)
    }
    async fn introspect_schema(&mut self, schema: &str) -> Result<SchemaModel> {
        dispatch!(self, a => a.introspect_schema(schema).await)
    }
    async fn introspect_table(&mut self, object: &ObjectRef) -> Result<TableDetail> {
        dispatch!(self, a => a.introspect_table(object).await)
    }
    async fn fetch_page(&mut self, request: &PageRequest) -> Result<Page> {
        dispatch!(self, a => a.fetch_page(request).await)
    }
    async fn count(&mut self, sql: &str) -> Result<u64> {
        dispatch!(self, a => a.count(sql).await)
    }
    async fn execute(&mut self, sql: &str, max_rows: usize) -> Result<QueryOutcome> {
        dispatch!(self, a => a.execute(sql, max_rows).await)
    }
    fn cancel_handle(&self) -> CancelHandle {
        dispatch!(self, a => a.cancel_handle())
    }
    async fn set_read_only(&mut self, read_only: bool) -> Result<()> {
        dispatch!(self, a => a.set_read_only(read_only).await)
    }
    fn server_version(&self) -> String {
        dispatch!(self, a => a.server_version())
    }
    fn is_closed(&self) -> bool {
        dispatch!(self, a => a.is_closed())
    }
}

/// Cancels the statement running on a connection, from any task.
#[derive(Clone)]
pub enum CancelHandle {
    Postgres(postgres::PgCancel),
    MySql(mysql::MyCancel),
}

impl CancelHandle {
    pub async fn cancel(&self) -> Result<()> {
        match self {
            CancelHandle::Postgres(c) => c.cancel().await,
            CancelHandle::MySql(c) => c.cancel().await,
        }
    }
}

/// Keeps the first `max` rows, counting the rest.
pub(crate) fn row_cap_reached(stored: usize, max: usize) -> bool {
    stored >= max
}
