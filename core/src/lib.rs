// hive-core: the store layer (Postgres store, schema/migrations, pgq query
// helpers) plus the MCP tool layer — the single-user data core the desktop
// shell (Phase 2) and the stdio bridge (PR 1.8) will drive directly.
// PR 1.4 adds the append-only durable layer underneath: the op-log record
// envelope + encrypted segment files (oplog), the content-addressed encrypted
// blockstore (blockstore), and master-key custody (keys). Store wiring to it
// happens at the 1.6 cutover.

pub mod blockstore;
pub mod db;
pub mod keys;
pub mod mcp;
pub mod oplog;
pub mod pgq;
pub mod store;
