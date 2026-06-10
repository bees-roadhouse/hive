// /api/search, /api/recall, /api/dashboard, /api/graph. Owned by the search workstream.

use axum::Router;

use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
}
