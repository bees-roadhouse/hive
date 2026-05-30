//! MCP JSON-RPC tool surface on `/mcp` (journal-canonical writes via `journal_add`).
//!
//! Tools call hive-db directly (same paths as REST), not HTTP loopback.

mod protocol;
mod tools;

pub use protocol::handle_jsonrpc;
