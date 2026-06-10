// Tasks/decisions/events/topics/phases/projects-with-children/links/shares/
// autocomplete routes. Owned by the core-stores workstream.

use axum::Router;

use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
}
