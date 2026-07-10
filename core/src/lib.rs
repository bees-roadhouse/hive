// hive-core: the store layer (Postgres store, schema/migrations, pgq query
// helpers) plus the MCP tool layer — the single-user data core the desktop
// shell (Phase 2) and the stdio bridge (PR 1.8) will drive directly.

pub mod db;
pub mod mcp;
pub mod pgq;
pub mod store;
