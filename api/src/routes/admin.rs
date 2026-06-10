// Actors delete/merge, import, sources, outbox, worker status, embeddings,
// fixtures. Owned by the admin workstream.

use axum::Router;

use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
}
