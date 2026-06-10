// GET /api/stream SSE (self-authenticating, 25s heartbeat). Owned by the admin workstream.

use axum::Router;

use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
}
