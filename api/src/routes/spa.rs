// Solid.js SPA static serving with index.html fallback. Owned by the SPA workstream.

use axum::Router;

use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
}
