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

pub mod ai;
pub mod claims;
pub mod config;
pub mod device;
pub mod extractor;
pub mod keys;
pub mod layer;
pub mod mfa;
pub mod password;
pub mod policy;
pub mod resolve;
pub mod revocation;
pub mod store;
pub mod tokens;
pub mod totp;

#[cfg(feature = "dev")]
pub mod dev;

use std::sync::Arc;

use jsonwebtoken::{Algorithm, DecodingKey, Validation};

use crate::auth::claims::{Claims, Principal};
use crate::auth::config::AuthConfig;
use crate::auth::keys::SigningKey;
use crate::auth::revocation::RevocationSet;

/// Shared auth state held in `AppState`. Cheap to clone (Arc inside).
#[derive(Clone)]
pub struct AuthState {
    inner: Arc<AuthInner>,
}

struct AuthInner {
    config: AuthConfig,
    key: SigningKey,
    /// Revoked-`jti` set for the non-expiring MCP/AI token class (§5.5).
    revocations: RevocationSet,
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
        Self::with_revocations(config, key, RevocationSet::empty())
    }

    /// Build with a pre-loaded revocation set (startup loads it from the DB).
    pub fn with_revocations(
        config: AuthConfig,
        key: SigningKey,
        revocations: RevocationSet,
    ) -> Self {
        Self {
            inner: Arc::new(AuthInner {
                config,
                key,
                revocations,
            }),
        }
    }

    pub fn config(&self) -> &AuthConfig {
        &self.inner.config
    }

    pub fn key(&self) -> &SigningKey {
        &self.inner.key
    }

    /// The shared revoked-`jti` set (§5.5). Routes that revoke push into it.
    pub fn revocations(&self) -> &RevocationSet {
        &self.inner.revocations
    }

    /// Validate a bearer token into `Claims`: EdDSA signature against the local
    /// key, plus `aud`/`exp` checks. `exp` is validated only when present (the
    /// non-expiring MCP/AI class legitimately omits it, §2).
    ///
    /// Audience: accepts the base RS audience (UI/CLI human tokens) OR the
    /// canonical MCP resource URI (AI tokens, RFC 8707/9728, §3.3). Both are
    /// "this server," so both are valid here; per-route MCP guards can later
    /// require the MCP audience specifically.
    ///
    /// Revocation (§5.5): for an AI token (non-expiring class), the `jti` is
    /// checked against the in-memory revocation set — a revoked AI token is
    /// rejected even though its signature + claims are otherwise valid. Human
    /// tokens lean on short `exp` and skip the per-request set check.
    pub fn validate_token(&self, token: &str) -> Result<Claims, AuthRejection> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[self.inner.config.issuer.as_str()]);
        validation.set_audience(&[
            self.inner.config.audience.as_str(),
            self.inner.config.mcp_resource().as_str(),
        ]);
        // exp is optional in our model; don't require it. jsonwebtoken still
        // validates it when the claim is present.
        validation.required_spec_claims.clear();
        validation.validate_exp = true;

        let claims = decode_claims(token, &self.inner.key.decoding, &validation)
            .map_err(|e| AuthRejection::InvalidToken(e.to_string()))?;

        // Non-expiring AI tokens MUST pass the revocation check on every use.
        if claims.principal_kind() == claims::PrincipalType::Ai
            && let Some(jti) = claims.jti.as_deref()
            && let Ok(parsed) = jti.parse::<uuid::Uuid>()
            && self.inner.revocations.is_revoked(&parsed)
        {
            return Err(AuthRejection::InvalidToken("token revoked".to_string()));
        }

        Ok(claims)
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
    use crate::auth::config::{AuthConfig, EnforcementMode};
    use crate::auth::tokens::{self, McpTokenParams};

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer_from_header(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer_from_header(Some("bearer xyz")), Some("xyz"));
        assert_eq!(bearer_from_header(Some("Basic abc")), None);
        assert_eq!(bearer_from_header(Some("Bearer   ")), None);
        assert_eq!(bearer_from_header(None), None);
    }

    fn test_state() -> AuthState {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let der = pkcs8.as_ref().to_vec();
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(&der).unwrap();
        use ring::signature::KeyPair;
        let kid = uuid::Uuid::now_v7().to_string();
        let key = keys::SigningKey {
            kid: kid.clone(),
            encoding: jsonwebtoken::EncodingKey::from_ed_der(&der),
            decoding: jsonwebtoken::DecodingKey::from_ed_der(pair.public_key().as_ref()),
            public_jwk: serde_json::json!({"kid": kid}),
        };
        let config = AuthConfig {
            issuer: "http://127.0.0.1:7878".to_string(),
            audience: "http://127.0.0.1:7878".to_string(),
            mode: EnforcementMode::Enforce,
            prod_markers_present: false,
        };
        AuthState::new(config, key)
    }

    /// Phase 6 (§3.4): an MCP token carries sub=AI + act=human, no exp, and
    /// validates with the MCP-resource audience. resolve_permissions reads the
    /// baked intersection but never grants admin.
    #[test]
    fn mcp_token_has_act_claim_no_exp_and_resolves_ai() {
        let auth = test_state();
        let cfg = auth.config().clone();
        let ai = "01999999-0000-7000-8000-0000000000aa";
        let human = "01999999-0000-7000-8000-0000000000bb";
        let jti = uuid::Uuid::now_v7().to_string();
        let token = tokens::mint_mcp_token(
            auth.key(),
            &McpTokenParams {
                issuer: &cfg.issuer,
                audience: &cfg.mcp_resource(),
                ai_subject: ai,
                act_subject: human,
                scopes: &["journal.read".to_string(), "tasks.read".to_string()],
                data_visibility: "owner",
                session_id: "01999999-0000-7000-8000-0000000000cc",
                jti: &jti,
                now: chrono::Utc::now().timestamp(),
                exp_secs: None,
            },
        )
        .expect("mint mcp token");

        let claims = auth.validate_token(&token).expect("mcp token validates");
        assert_eq!(claims.sub, ai);
        assert_eq!(claims.act.as_deref(), Some(human));
        assert_eq!(claims.exp, None, "MCP token must be non-expiring");
        assert_eq!(claims.principal_kind(), claims::PrincipalType::Ai);

        let principal = auth.principal_from_claims(claims);
        assert!(principal.permissions.has_scope("journal.read"));
        assert!(!principal.permissions.is_admin, "AI is never admin");
        assert_eq!(principal.act.as_deref(), Some(human));
    }

    /// Phase 6 (§5.5): a non-expiring AI token is rejected once its jti is in
    /// the revocation set — the only off-switch for the no-exp class.
    #[test]
    fn revoked_mcp_token_is_rejected() {
        let auth = test_state();
        let cfg = auth.config().clone();
        let jti = uuid::Uuid::now_v7();
        let token = tokens::mint_mcp_token(
            auth.key(),
            &McpTokenParams {
                issuer: &cfg.issuer,
                audience: &cfg.mcp_resource(),
                ai_subject: "01999999-0000-7000-8000-0000000000aa",
                act_subject: "01999999-0000-7000-8000-0000000000bb",
                scopes: &["journal.read".to_string()],
                data_visibility: "owner",
                session_id: "01999999-0000-7000-8000-0000000000cc",
                jti: &jti.to_string(),
                now: chrono::Utc::now().timestamp(),
                exp_secs: None,
            },
        )
        .unwrap();

        // Valid before revocation.
        assert!(auth.validate_token(&token).is_ok());
        // Revoke the jti -> the same token now fails validation.
        auth.revocations().insert(jti);
        let got = auth.validate_token(&token);
        assert!(
            matches!(got, Err(AuthRejection::InvalidToken(ref m)) if m.contains("revoked")),
            "revoked AI token must be rejected, got {got:?}"
        );
    }

    /// A revoked jti must NOT block a human token (humans lean on exp, not the
    /// revocation set) — the per-request check is AI-only.
    #[test]
    fn human_token_ignores_revocation_set() {
        use crate::auth::tokens::AccessTokenParams;
        let auth = test_state();
        let cfg = auth.config().clone();
        let token = tokens::mint_access_token(
            auth.key(),
            &AccessTokenParams {
                issuer: &cfg.issuer,
                audience: &cfg.audience,
                subject: "01999999-0000-7000-8000-0000000000dd",
                principal_type: "human",
                scopes: &["hive.read".to_string()],
                is_admin: false,
                data_visibility: "owner",
                session_id: "01999999-0000-7000-8000-0000000000ee",
                now: chrono::Utc::now().timestamp(),
                ttl_secs: 600,
            },
        )
        .unwrap();
        // Even with a populated revocation set, the human token validates (its
        // jti isn't checked).
        auth.revocations().insert(uuid::Uuid::now_v7());
        assert!(auth.validate_token(&token).is_ok());
    }
}
