// hive-core: the single-user data core (PR 1.6 cutover shape). The append-only
// op log (oplog) is the source of truth; the SQLCipher SQLite index (index) is
// its rebuildable projection, maintained by the fold; the blockstore holds
// payload bytes; keys holds master-key custody. The store layer rides ONE
// writer thread that owns all of it (store/core.rs) behind the unchanged
// async Store surface, and mcp is the tool layer the desktop shell (Phase 2)
// and the stdio bridge (PR 1.8) drive directly. Postgres left at the cutover;
// the PR 1.7 importer reads old instances with its own Postgres client.

pub mod blockstore;
pub mod fold;
pub mod index;
pub mod keys;
pub mod mcp;
pub mod oplog;
pub mod store;
