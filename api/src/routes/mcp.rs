// POST /mcp JSON-RPC with Bearer auth + www-authenticate. Owned by the MCP workstream.

use axum::Router;

use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
}
