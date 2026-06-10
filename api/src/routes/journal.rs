// Journal routes: GET/POST /api/journal, /api/journal/{id}, /api/journal/writers.
// Owned by the core-stores workstream (port the server.ts journal section).

use axum::Router;

use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
}
