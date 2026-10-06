//! One connection per pane, owned by a worker task: a pane does one thing at
//! a time, a pool would add complexity for nothing. The interface never
//! awaits the database in its loop; it sends requests to the worker and gets
//! the replies back as messages. Cancellation goes around the worker through
//! the driver's cancel handle.

use crate::paths::HerdrEnv;
use crate::{secrets, tunnel};
use herdr_db_core::cell::{Page, QueryOutcome};
use herdr_db_core::config::SourceConfig;
use herdr_db_core::model::{ObjectRef, SchemaModel, TableDetail};
use herdr_db_core::paging::PageRequest;
use herdr_db_drivers::{Adapter, AnyAdapter, CancelHandle, ConnectParams, DriverError, SessionSettings};
use secrecy::SecretString;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbStatus {
    TunnelStarting,
    Connecting,
    Connected { server_version: String },
    Disconnected,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DbError {
    #[error("mot de passe requis pour {user}")]
    NeedPassword {
        user: String,
        /// A password was tried and refused.
        rejected: bool,
    },
    #[error("{0}")]
    Setup(String),
    #[error(transparent)]
    Driver(#[from] DriverError),
    #[error("connexion fermée")]
    Closed,
}

impl DbError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, DbError::Driver(DriverError::Cancelled))
    }
}

enum Request {
    ListSchemas,
    IntrospectSchema(String),
    IntrospectTable(ObjectRef),
    FetchPage(Box<PageRequest>),
    Count(String),
    Execute { sql: String, max_rows: usize },
    SetReadOnly(bool),
}

enum Reply {
    Schemas(Vec<String>),
    Schema(SchemaModel),
    Table(Box<TableDetail>),
    Page(Page),
    Count(u64),
    Outcome(QueryOutcome),
    Done,
}

type Job = (Request, oneshot::Sender<Result<Reply, DbError>>);
pub type StatusFn = Box<dyn Fn(DbStatus) + Send + Sync>;

struct PendingPassword {
    password: SecretString,
    save: bool,
}

#[derive(Clone)]
pub struct DbHandle {
    jobs: mpsc::UnboundedSender<Job>,
    cancel: Arc<Mutex<Option<CancelHandle>>>,
    password: Arc<Mutex<Option<PendingPassword>>>,
}

struct Worker {
    source: SourceConfig,
    env: Arc<HerdrEnv>,
    adapter: Option<AnyAdapter>,
    lease: Option<tunnel::Lease>,
    write_mode: bool,
    cancel: Arc<Mutex<Option<CancelHandle>>>,
    password: Arc<Mutex<Option<PendingPassword>>>,
    status: StatusFn,
}

impl DbHandle {
    /// `idle`: close the connection (and release the tunnel) after this long
    /// without requests. The tree uses it; grids and consoles keep theirs.
    pub fn spawn(source: SourceConfig, env: Arc<HerdrEnv>, idle: Option<Duration>, status: StatusFn) -> DbHandle {
        let (jobs, rx) = mpsc::unbounded_channel();
        let cancel = Arc::new(Mutex::new(None));
        let password = Arc::new(Mutex::new(None));
        let worker = Worker {
            source,
            env,
            adapter: None,
            lease: None,
            write_mode: false,
            cancel: cancel.clone(),
            password: password.clone(),
            status,
        };
        tokio::spawn(worker.run(rx, idle));
        DbHandle { jobs, cancel, password }
    }

    /// Password typed by the user, used for the next connection only.
    pub fn provide_password(&self, password: SecretString, save: bool) {
        *self.password.lock().unwrap_or_else(|e| e.into_inner()) = Some(PendingPassword { password, save });
    }

    pub async fn cancel(&self) -> Result<(), DbError> {
        let handle = self.cancel.lock().unwrap_or_else(|e| e.into_inner()).clone();
        match handle {
            Some(handle) => handle.cancel().await.map_err(DbError::from),
            None => Ok(()),
        }
    }

    async fn call(&self, request: Request) -> Result<Reply, DbError> {
        let (tx, rx) = oneshot::channel();
        self.jobs.send((request, tx)).map_err(|_| DbError::Closed)?;
        rx.await.map_err(|_| DbError::Closed)?
    }

    pub async fn list_schemas(&self) -> Result<Vec<String>, DbError> {
        match self.call(Request::ListSchemas).await? {
            Reply::Schemas(s) => Ok(s),
            _ => unreachable!("reply matches request"),
        }
    }

    pub async fn introspect_schema(&self, schema: &str) -> Result<SchemaModel, DbError> {
        match self.call(Request::IntrospectSchema(schema.to_string())).await? {
            Reply::Schema(s) => Ok(s),
            _ => unreachable!("reply matches request"),
        }
    }

    pub async fn introspect_table(&self, object: &ObjectRef) -> Result<TableDetail, DbError> {
        match self.call(Request::IntrospectTable(object.clone())).await? {
            Reply::Table(t) => Ok(*t),
            _ => unreachable!("reply matches request"),
        }
    }

    pub async fn fetch_page(&self, request: PageRequest) -> Result<Page, DbError> {
        match self.call(Request::FetchPage(Box::new(request))).await? {
            Reply::Page(p) => Ok(p),
            _ => unreachable!("reply matches request"),
        }
    }

    pub async fn count(&self, sql: String) -> Result<u64, DbError> {
        match self.call(Request::Count(sql)).await? {
            Reply::Count(n) => Ok(n),
            _ => unreachable!("reply matches request"),
        }
    }

    pub async fn execute(&self, sql: String, max_rows: usize) -> Result<QueryOutcome, DbError> {
        match self.call(Request::Execute { sql, max_rows }).await? {
            Reply::Outcome(o) => Ok(o),
            _ => unreachable!("reply matches request"),
        }
    }

    pub async fn set_read_only(&self, read_only: bool) -> Result<(), DbError> {
        self.call(Request::SetReadOnly(read_only)).await.map(|_| ())
    }
}

impl Worker {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Job>, idle: Option<Duration>) {
        loop {
            let job = match idle.filter(|_| self.adapter.is_some()) {
                Some(idle) => match tokio::time::timeout(idle, rx.recv()).await {
                    Ok(job) => job,
                    Err(_) => {
                        self.disconnect();
                        continue;
                    }
                },
                None => rx.recv().await,
            };
            let Some((request, reply)) = job else { break };
            let result = self.handle(request).await;
            let _ = reply.send(result);
        }
        self.disconnect();
    }

    fn disconnect(&mut self) {
        if self.adapter.take().is_some() {
            (self.status)(DbStatus::Disconnected);
        }
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(lease) = self.lease.take() {
            lease.release();
        }
    }

    async fn handle(&mut self, request: Request) -> Result<Reply, DbError> {
        // Reads are retried once on a fresh connection; statements are not.
        let retryable = !matches!(request, Request::Execute { .. } | Request::SetReadOnly(_));
        let mut attempt = 0;
        loop {
            attempt += 1;
            self.ensure_connected().await?;
            let adapter = self.adapter.as_mut().expect("connected");
            let result = match &request {
                Request::ListSchemas => adapter.list_schemas().await.map(Reply::Schemas),
                Request::IntrospectSchema(s) => adapter.introspect_schema(s).await.map(Reply::Schema),
                Request::IntrospectTable(o) => adapter.introspect_table(o).await.map(|t| Reply::Table(Box::new(t))),
                Request::FetchPage(p) => adapter.fetch_page(p).await.map(Reply::Page),
                Request::Count(sql) => adapter.count(sql).await.map(Reply::Count),
                Request::Execute { sql, max_rows } => adapter.execute(sql, *max_rows).await.map(Reply::Outcome),
                Request::SetReadOnly(read_only) => {
                    let result = adapter.set_read_only(*read_only).await;
                    if result.is_ok() {
                        self.write_mode = !*read_only;
                    }
                    result.map(|()| Reply::Done)
                }
            };
            match result {
                Err(e) if e.is_connection_lost() || self.adapter.as_ref().is_some_and(|a| a.is_closed()) => {
                    tracing::warn!(source = %self.source.id, error = %e, "connection lost");
                    self.adapter = None;
                    *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    (self.status)(DbStatus::Disconnected);
                    if retryable && attempt < 2 {
                        continue;
                    }
                    return Err(e.into());
                }
                other => return other.map_err(Into::into),
            }
        }
    }

    async fn ensure_connected(&mut self) -> Result<(), DbError> {
        if self.adapter.as_ref().is_some_and(|a| !a.is_closed()) {
            return Ok(());
        }
        self.adapter = None;
        let source = self.source.clone();
        if let Some(command) = &source.pre_connect
            && self.lease.is_none()
        {
            (self.status)(DbStatus::TunnelStarting);
            let lease = tunnel::acquire(
                self.env.state_dir.clone(),
                source.id.clone(),
                command.clone(),
                source.host.clone(),
                source.port,
                self.env.holder_id(),
            )
            .await
            .map_err(|e| DbError::Setup(e.to_string()))?;
            self.lease = Some(lease);
        }
        (self.status)(DbStatus::Connecting);
        let user = secrets::user_of(&source);
        let pending = self.password.lock().unwrap_or_else(|e| e.into_inner()).take();
        let (password, save) = match pending {
            Some(p) => (Some(p.password), p.save),
            None => (secrets::resolve(&source).await.map_err(|e| DbError::Setup(e.to_string()))?, false),
        };
        let tried = password.is_some();
        let to_save = if save { password.clone() } else { None };
        let params = ConnectParams {
            engine: source.engine,
            host: source.host.clone(),
            port: source.port,
            database: source.database.clone(),
            user: user.clone(),
            password,
            tls: source.tls,
            session: SessionSettings {
                read_only: source.read_only && !self.write_mode,
                statement_timeout: source.statement_timeout,
            },
        };
        match AnyAdapter::connect(params).await {
            Ok(adapter) => {
                *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = Some(adapter.cancel_handle());
                (self.status)(DbStatus::Connected { server_version: adapter.server_version() });
                self.adapter = Some(adapter);
                if let Some(password) = to_save {
                    let (id, user) = (source.id.clone(), user.clone());
                    let saved = tokio::task::spawn_blocking(move || secrets::save_keyring(&id, &user, &password)).await;
                    if let Ok(Err(e)) = saved {
                        tracing::warn!(error = %e, "keyring save failed");
                    }
                }
                Ok(())
            }
            Err(DriverError::Auth(message)) if source.password_command.is_none() => {
                tracing::info!(source = %source.id, %message, "password required");
                Err(DbError::NeedPassword { user, rejected: tried })
            }
            Err(e) => {
                (self.status)(DbStatus::Disconnected);
                Err(e.into())
            }
        }
    }
}
