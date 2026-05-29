//! Shared postgres layer for the hive shared-state DB.
//!
//! Migrated from sqlite + r2d2 + rusqlite to postgres + sqlx (async).
//! Schema lives in `migrations/0001_initial.sql`; `open_pool` applies any
//! pending migrations on open.

pub mod enums;
pub mod error;
pub mod pool;
pub mod queries;
pub mod slug;
pub mod types;

pub use error::{Error, Result};
pub use pool::{DEFAULT_DATABASE_URL, default_database_url, open_pool};
pub use sqlx::PgPool;
