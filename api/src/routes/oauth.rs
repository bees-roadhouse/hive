// OAuth 2.1 AS endpoints + OIDC login (server.ts OAuth/OIDC sections).
// Owned by the OAuth workstream.

use axum::Router;

use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
}
