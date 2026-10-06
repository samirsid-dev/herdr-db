//! Introspection: the only writer of the cache. Level 1 (object lists of the
//! selected schemas) when a source opens, level 2 (one table's details) when
//! it is expanded or documented. One process at a time per source: the file
//! lock makes the others wait, and they then read what was written.

use crate::db::{DbError, DbHandle};
use chrono::Utc;
use herdr_db_core::config::SourceConfig;
use herdr_db_core::model::{ObjectRef, TableDetail};
use herdr_db_store::{Cache, IntrospectionLock};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntrospectError {
    #[error(transparent)]
    Db(#[from] DbError),
    #[error("cache : {0}")]
    Cache(String),
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, IntrospectError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| IntrospectError::Cache(e.to_string()))?
        .map_err(IntrospectError::Cache)
}

/// Takes the lock, calling `waiting` when another process holds it.
async fn lock(state_dir: &Path, source: &str, waiting: impl FnOnce()) -> Result<IntrospectionLock, IntrospectError> {
    let (dir, id) = (state_dir.to_path_buf(), source.to_string());
    if let Some(lock) = blocking(move || IntrospectionLock::try_acquire(&dir, &id).map_err(|e| e.to_string())).await? {
        return Ok(lock);
    }
    waiting();
    let (dir, id) = (state_dir.to_path_buf(), source.to_string());
    blocking(move || IntrospectionLock::acquire(&dir, &id).map_err(|e| e.to_string())).await
}

fn open(state_dir: &Path, source: &SourceConfig) -> Result<Cache, String> {
    Cache::open(state_dir, &source.id, source.engine).map_err(|e| e.to_string())
}

/// Level 1 for every displayed schema. `force` also drops cached details.
pub async fn introspect_source(
    db: &DbHandle,
    state_dir: PathBuf,
    source: SourceConfig,
    force: bool,
    waiting: impl FnOnce(),
) -> Result<(), IntrospectError> {
    let _lock = lock(&state_dir, &source.id, waiting).await?;
    let available = db.list_schemas().await?;
    let mut models = Vec::new();
    for schema in &source.schemas {
        if available.iter().any(|s| s == schema) {
            models.push(db.introspect_schema(schema).await?);
        }
    }
    let schemas = source.schemas.clone();
    blocking(move || {
        let mut cache = open(&state_dir, &source)?;
        cache.set_available_schemas(&available).map_err(|e| e.to_string())?;
        cache.retain_schemas(&schemas).map_err(|e| e.to_string())?;
        for model in &models {
            if force {
                cache.clear_details(&model.name).map_err(|e| e.to_string())?;
            }
            cache.write_schema(model).map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .await
}

/// Level 1 for one schema.
pub async fn introspect_schema(
    db: &DbHandle,
    state_dir: PathBuf,
    source: SourceConfig,
    schema: String,
    force: bool,
    waiting: impl FnOnce(),
) -> Result<(), IntrospectError> {
    let _lock = lock(&state_dir, &source.id, waiting).await?;
    let model = db.introspect_schema(&schema).await?;
    blocking(move || {
        let mut cache = open(&state_dir, &source)?;
        if force {
            cache.clear_details(&schema).map_err(|e| e.to_string())?;
        }
        cache.write_schema(&model).map_err(|e| e.to_string())
    })
    .await
}

/// Level 2 for one object.
pub async fn introspect_table(
    db: &DbHandle,
    state_dir: PathBuf,
    source: SourceConfig,
    object: ObjectRef,
    waiting: impl FnOnce(),
) -> Result<TableDetail, IntrospectError> {
    let _lock = lock(&state_dir, &source.id, waiting).await?;
    let detail = db.introspect_table(&object).await?;
    let stored = detail.clone();
    blocking(move || {
        let mut cache = open(&state_dir, &source)?;
        cache.write_detail(&object, &stored, Utc::now()).map_err(|e| e.to_string())
    })
    .await?;
    Ok(detail)
}

/// Schemas of the server for the selector.
pub async fn available_schemas(
    db: &DbHandle,
    state_dir: PathBuf,
    source: SourceConfig,
) -> Result<Vec<String>, IntrospectError> {
    let available = db.list_schemas().await?;
    let stored = available.clone();
    blocking(move || open(&state_dir, &source)?.set_available_schemas(&stored).map_err(|e| e.to_string())).await?;
    Ok(available)
}

/// Cached detail, introspecting it first when missing.
pub async fn detail(
    db: &DbHandle,
    state_dir: PathBuf,
    source: SourceConfig,
    object: ObjectRef,
    waiting: impl FnOnce(),
) -> Result<TableDetail, IntrospectError> {
    let (dir, src, obj) = (state_dir.clone(), source.clone(), object.clone());
    let cached =
        blocking(move || Ok(open(&dir, &src)?.detail(&obj).map_err(|e| e.to_string())?.map(|(d, _)| d))).await?;
    match cached {
        Some(detail) => Ok(detail),
        None => introspect_table(db, state_dir, source, object, waiting).await,
    }
}

/// Catalog row estimate of an object, from the cache.
pub fn cached_estimate(state_dir: &Path, source: &SourceConfig, object: &ObjectRef) -> Option<i64> {
    open(state_dir, source).ok()?.object(object).ok()??.summary.estimated_rows
}
