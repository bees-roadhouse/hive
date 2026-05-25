//! Auth (hive-auth-mcp-design.md §8 Phase 1): the RS side of the local AS.
//!
//! Phase 1 lands the *resource-server* half: verify EdDSA JWTs against the
//! local JWKS, resolve them to the internal permission vocabulary through the
//! one `resolve_permissions` chokepoint (§6.1), and run a tower layer that
//! today only WARNS about what it would reject (so tokenless hive-ui/hive-cli
//! keep working). Token minting + the user/session tables are Phase 2.
//!
//! Submodules:
//! - `claims`   — Claims, Principal, the ResolvedPermissions vocabulary.
//! - `config`   — issuer/audience, warn-vs-enforce, prod-marker detection.
//! - `keys`     — Ed25519 keygen + JWKS + load-or-create.
//! - `resolve`  — the resolve_permissions chokepoint.
//! - `layer`    — the tower middleware (warn-only in Phase 1).
//! - `extractor`— the `AuthUser` axum extractor handlers use.
//! - `dev`      — the dev-mode bypass (compiled only under `--features dev`).
//!
//! NOTE on `dead_code`: Phase 1 lands the auth *seam* — several items here are
//! the stable API surface that Phase 2+ (token minting, per-route scope guards,
//! the user/session tables) and the `dev` feature consume, but that nothing in
//! the Phase-1 default build calls yet. They're exercised by this module's unit
//! tests. We allow dead_code module-wide rather than scatter per-item allows or
//! fake usages; revisit when Phase 2 wires them in.
#![allow(dead_code)]

pub mod claims;
pub mod config;
pub mod extractor;
pub mod keys;
pub mod layer;
pub mod resolve;

#[cfg(feature = "dev")]
pub mod dev;

use std::sync::Arc;

use jsonwebtoken::{Algorithm, DecodingKey, Validation};

use crate::auth::claims::{Claims, Principal};
use crate::auth::config::AuthConfig;
use crate::auth::keys::SigningKey;

/// Shared auth state held in `AppState`. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct AuthState {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    config: AuthConfig,
    key: SigningKey,
}

/// Why a token failed to authenticate — drives the 401/403 distinction and the
/// warn-mode log message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthRejection {
    /// No bearer token present at all.
    MissingToken,
    /// Token present but signature/claims invalid or expired → 401.
    InvalidToken(String),
    /// Token valid but lacks the required scope/permission → 403. (Not used by
    /// the blanket layer in Phase 1; per-route guards raise it in later phases.)
    InsufficientScope(String),
}

impl AuthState {
    pub fn new(config: AuthConfig, key: SigningKey) -> Self {
        Self {
            inner: Arc::new(AuthInner { config, key }),
        }
    }

    pub fn config(&self) -> &AuthConfig {
        &self.inner.config
    }

    pub fn key(&self) -> &SigningKey {
        &self.inner.key
    }

    /// Validate a bearer token into `Claims`: EdDSA signature against the local
    /// key, plus `aud`/`exp` checks. `exp` is validated only when present (the
    /// non-expiring MCP/AI class legitimately omits it, §2).
    pub fn validate_token(&self, token: &str) -> Result<Claims, AuthRejection> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[self.inner.config.issuer.as_str()]);
        validation.set_audience(&[self.inner.config.audience.as_str()]);
        // exp is optional in our model; don't require it. jsonwebtoken still
        // validates it when the claim is present.
        validation.required_spec_claims.clear();
        validation.validate_exp = true;

        decode_claims(token, &self.inner.key.decoding, &validation)
            .map_err(|e| AuthRejection::InvalidToken(e.to_string()))
    }

    /// Resolve a validated token to an authenticated `Principal` via the
    /// `resolve_permissions` chokepoint (§6.1).
    pub fn principal_from_claims(&self, claims: Claims) -> Principal {
        let permissions = resolve::resolve_permissions(&claims);
        Principal {
            subject: claims.sub.clone(),
            kind: claims.principal_kind(),
            act: claims.act.clone(),
            permissions,
        }
    }
}

/// Decode + validate, separated out so the unit test can exercise it directly.
fn decode_claims(
    token: &str,
    key: &DecodingKey,
    validation: &Validation,
) -> Result<Claims, jsonwebtoken::errors::Error> {
    jsonwebtoken::decode::<Claims>(token, key, validation).map(|data| data.claims)
}

/// Extract a bearer token from an `Authorization: Bearer <token>` header value.
pub fn bearer_from_header(value: Option<&str>) -> Option<&str> {
    let v = value?;
    let rest = v
        .strip_prefix("Bearer ")
        .or_else(|| v.strip_prefix("bearer "))?;
    let t = rest.trim();
    if t.is_empty() { None } else { Some(t) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer_from_header(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_from_header(Some("bearer xyz")), Some("xyz"));
        assert_eq!(bearer_from_header(Some("Basic abc")), None);
        assert_eq!(bearer_from_header(Some("Bearer   ")), None);
        assert_eq!(bearer_from_header(None), None);
    }
}
