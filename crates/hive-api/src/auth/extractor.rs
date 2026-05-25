//! The `AuthUser` axum extractor (hive-auth-mcp-design.md §8 Phase 1).
//!
//! The auth layer (`super::layer`) inserts the resolved `Principal` into request
//! extensions. These extractors read it back out so handlers can see who's
//! calling without re-parsing tokens.
//!
//! Two forms:
//! - `MaybeAuthUser(Option<Principal>)` — never fails. The right choice in
//!   Phase 1 / warn-only, where a request may legitimately be unauthenticated.
//! - `AuthUser(Principal)` — requires an authenticated principal, 401s if
//!   absent. For routes that opt into hard auth in later phases.
//!
//! No Phase 1 route consumes these yet; they exist so Phase 2+ handlers and the
//! per-route scope guards have the seam ready.

use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;

use super::claims::Principal;

/// Always-succeeds extractor: `Some` when the request authenticated, `None`
/// otherwise. Safe under warn-only mode.
pub struct MaybeAuthUser(pub Option<Principal>);

impl<S> FromRequestParts<S> for MaybeAuthUser
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(MaybeAuthUser(parts.extensions.get::<Principal>().cloned()))
    }
}

/// Requires an authenticated principal; 401 if the layer didn't resolve one.
pub struct AuthUser(pub Principal);

impl<S> FromRequestParts<S> for AuthUser
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Principal>()
            .cloned()
            .map(AuthUser)
            .ok_or((StatusCode::UNAUTHORIZED, "authentication required"))
    }
}
