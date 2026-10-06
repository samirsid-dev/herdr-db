//! I/O-free core of Herdr DB.
//!
//! Everything here is plain data and pure functions: the metadata model shared
//! by every pane, configuration parsing and merging, SQL generation helpers
//! (identifiers, pagination, PostgreSQL DDL, read-only role scripts) and the
//! UX warnings shown by the console. Tests run in milliseconds and the crate
//! compiles without pulling any driver.

pub mod cell;
pub mod config;
pub mod ddl;
pub mod model;
pub mod paging;
pub mod readonly_role;
pub mod request;
pub mod sql;
pub mod statements;
pub mod warnings;

pub use model::{Engine, ObjectKind, ObjectRef, SchemaModel, TableDetail};
