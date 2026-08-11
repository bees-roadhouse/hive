pub mod auth;
pub mod error;
pub mod legacy_import;
pub mod mcp;
pub mod middleware;
pub mod routes;

// The store layer lives in hive-core; re-export it so every existing
// `crate::store::…` / `hive_api::db::…` path keeps resolving unchanged.
pub use hive_core::{db, pgq, store};
