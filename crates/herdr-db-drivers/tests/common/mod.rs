//! Integration test helpers. Adapters are tested against real servers, never
//! mocks: reading real catalogs correctly is their whole value.
//!
//! Connection settings come from the environment, e.g.
//! `HERDR_DB_TEST_POSTGRES="host=localhost port=5432 user=postgres password=postgres dbname=herdr_db_test"`.
//! CI provides the servers as services; locally `scripts/test-databases.sh`
//! starts them with Docker. Without the variable the tests are skipped.

#![allow(dead_code)]

use herdr_db_core::config::TlsMode;
use herdr_db_core::model::Engine;
use herdr_db_drivers::{AnyAdapter, ConnectParams, SessionSettings};
use secrecy::SecretString;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Serializes tests sharing a database.
pub static SERIAL: Mutex<()> = Mutex::new(());

pub fn settings(var: &str) -> Option<HashMap<String, String>> {
    let value = std::env::var(var).ok()?;
    Some(
        value
            .split_whitespace()
            .filter_map(|pair| pair.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

pub fn params(engine: Engine, settings: &HashMap<String, String>, database: Option<&str>) -> ConnectParams {
    ConnectParams {
        engine,
        host: settings.get("host").cloned().unwrap_or_else(|| "localhost".into()),
        port: settings.get("port").and_then(|p| p.parse().ok()).unwrap_or(engine.default_port()),
        database: database
            .map(str::to_string)
            .or_else(|| settings.get("dbname").cloned())
            .unwrap_or_else(|| "herdr_db_test".into()),
        user: settings.get("user").cloned().unwrap_or_else(|| "postgres".into()),
        password: settings.get("password").map(|p| SecretString::from(p.clone())),
        tls: TlsMode::Prefer,
        session: SessionSettings { read_only: false, statement_timeout: None },
    }
}

pub async fn connect(params: ConnectParams) -> AnyAdapter {
    AnyAdapter::connect(params).await.expect("test database reachable")
}

pub fn with_session(mut params: ConnectParams, read_only: bool, timeout: Option<Duration>) -> ConnectParams {
    params.session = SessionSettings { read_only, statement_timeout: timeout };
    params
}

pub fn lock() -> std::sync::MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}
