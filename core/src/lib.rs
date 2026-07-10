// hive-core: the store layer (Postgres store, schema/migrations, pgq query
// helpers) plus the MCP tool layer — the single-user data core the desktop
// shell (Phase 2) and the stdio bridge (PR 1.8) will drive directly.
// PR 1.4 adds the append-only durable layer underneath: the op-log record
// envelope + encrypted segment files (oplog), the content-addressed encrypted
// blockstore (blockstore), and master-key custody (keys). PR 1.5 adds the
// derived side: the SQLCipher-encrypted SQLite index (index) and the record
// projector that maintains it (fold) — additive; the Store still runs on
// Postgres until the 1.6 cutover.

pub mod blockstore;
pub mod db;
pub mod fold;
pub mod index;
pub mod keys;
pub mod mcp;
pub mod oplog;
pub mod pgq;
pub mod store;
