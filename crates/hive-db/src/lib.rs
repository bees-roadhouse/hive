//! Shared sqlite layer for the hive shared-state DB.
//!
//! Frozen against the post-task-8 `~/.hive/hive.db` schema. See
//! `SCHEMA.md` for the human-readable description and `schema::SCHEMA_SQL`
//! for the canonical CREATE statements.

pub mod enums;
pub mod error;
pub mod pool;
pub mod queries;
pub mod schema;
pub mod types;

pub use error::{Error, Result};
pub use pool::{Pool, default_db_path, open_pool};
pub use rusqlite::Connection;
