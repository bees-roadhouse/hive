//! The auth tower middleware (hive-auth-mcp-design.md §8 Phase 1).
//!
//! Runs on every request after the routers merge. It resolves the request to a
//! `Principal` (via dev-bypass or JWT validation) and stashes it in request
//! extensions for the `AuthUser` extractor. In Phase 1 it runs WARN-ONLY:
//! validation failures are logged ("would reject ...") but the request still
//! proceeds, so tokenless hive-ui/hive-cli keep working. Flip to enforce with
//! `HIVE_AUTH_ENFORCE=1` (Phase 3 does this once clients carry tokens).

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, State};
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::config::EnforcementMode;
use super::{AuthRejection, AuthState, bearer_from_header};

/// Axum middleware fn. Wired via `axum::middleware::from_fn_with_state`.
pub async fn auth_middleware(
    State(auth): State<AuthState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0);

    let token = bearer_from_header(
        req.headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    );

    let outcome = resolve_request(&auth, peer, token);

    match outcome {
        Ok(principal) => {
            // Authenticated (real token or dev-bypass). Hand the principal to
            // downstream handlers via extensions.
            req.extensions_mut().insert(principal);
            next.run(req).await
        }
        Err(rej) => {
            let path = req.uri().path().to_string();
            match auth.config().mode {
                EnforcementMode::Warn => {
                    // Phase 1: log what we WOULD reject, then let it through.
                    tracing::warn!(
                        path = %path,
                        rejection = ?rej,
                        "auth warn-only: request would be rejected under enforce mode; \
                         passing through (Phase 1)"
                    );
                    next.run(req).await
                }
                EnforcementMode::Enforce => reject_response(&auth, rej),
            }
        }
    }
}

/// Decide the auth outcome for a request without side effects (testable).
/// Tries the dev-bypass first (only compiled in under `--features dev`), then
/// falls back to JWT validation.
fn resolve_request(
    auth: &AuthState,
    peer: Option<SocketAddr>,
    token: Option<&str>,
) -> Result<super::claims::Principal, AuthRejection> {
    #[cfg(feature = "dev")]
    {
        if let Some(p) = super::dev::try_dev_bypass(auth.config(), peer, token) {
            return Ok(p);
        }
    }
    #[cfg(not(feature = "dev"))]
    let _ = peer; // peer is only consulted by the dev-bypass.

    let token = token.ok_or(AuthRejection::MissingToken)?;
    let claims = auth.validate_token(token)?;
    Ok(auth.principal_from_claims(claims))
}

/// Build the enforce-mode rejection response with the correct status + the
/// `WWW-Authenticate` challenge (RFC 9728 §5.1: point clients at the resource
/// metadata so they can discover the AS).
fn reject_response(auth: &AuthState, rej: AuthRejection) -> Response {
    let resource_metadata = format!(
        "{}/.well-known/oauth-protected-resource",
        auth.config().issuer.trim_end_matches('/')
    );
    let (status, challenge) = match rej {
        AuthRejection::InsufficientScope(scope) => (
            StatusCode::FORBIDDEN,
            format!(
                "Bearer error=\"insufficient_scope\", scope=\"{scope}\", \
                 resource_metadata=\"{resource_metadata}\""
            ),
        ),
        AuthRejection::MissingToken => (
            StatusCode::UNAUTHORIZED,
            format!("Bearer resource_metadata=\"{resource_metadata}\""),
        ),
        AuthRejection::InvalidToken(_) => (
            StatusCode::UNAUTHORIZED,
            format!("Bearer error=\"invalid_token\", resource_metadata=\"{resource_metadata}\""),
        ),
    };
    (status, [(header::WWW_AUTHENTICATE, challenge)]).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::config::{AuthConfig, EnforcementMode};
    use crate::auth::keys;

    fn test_auth(mode: EnforcementMode) -> AuthState {
        let (kid, der, _) = test_key();
        let key = keys::SigningKey {
            kid: kid.clone(),
            encoding: jsonwebtoken::EncodingKey::from_ed_der(&der),
            decoding: decoding_for(&der),
            public_jwk: serde_json::json!({"kid": kid}),
        };
        let config = AuthConfig {
            issuer: "http://127.0.0.1:7878".to_string(),
            audience: "http://127.0.0.1:7878".to_string(),
            mode,
            prod_markers_present: false,
        };
        AuthState::new(config, key)
    }

    fn test_key() -> (String, Vec<u8>, ()) {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        (
            uuid::Uuid::now_v7().to_string(),
            pkcs8.as_ref().to_vec(),
            (),
        )
    }

    fn decoding_for(pkcs8_der: &[u8]) -> jsonwebtoken::DecodingKey {
        use ring::signature::KeyPair;
        let pair = ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8_der).unwrap();
        jsonwebtoken::DecodingKey::from_ed_der(pair.public_key().as_ref())
    }

    #[test]
    fn missing_token_is_rejected_in_resolve() {
        let auth = test_auth(EnforcementMode::Enforce);
        let got = resolve_request(&auth, None, None);
        assert_eq!(got.unwrap_err(), AuthRejection::MissingToken);
    }

    #[test]
    fn garbage_token_is_invalid() {
        let auth = test_auth(EnforcementMode::Enforce);
        let got = resolve_request(&auth, None, Some("not.a.jwt"));
        assert!(matches!(got, Err(AuthRejection::InvalidToken(_))));
    }

    /// Phase 3 verification (§8 Phase 3): the enforce path is reachable and a
    /// real Bearer token passes it. We mint a token with the same key the
    /// `AuthState` holds (mirroring what `/token` does), then confirm
    /// `resolve_request` — the exact function the enforce-mode middleware runs —
    /// accepts it and rejects the tokenless case. This is the "tokenless ->
    /// reject, Bearer -> pass under Enforce" gate the lead asked for.
    #[test]
    fn enforce_rejects_tokenless_and_accepts_bearer_token() {
        use crate::auth::tokens::{self, AccessTokenParams};

        let auth = test_auth(EnforcementMode::Enforce);

        // Tokenless under enforce => MissingToken (the middleware 401s on this).
        assert_eq!(
            resolve_request(&auth, None, None).unwrap_err(),
            AuthRejection::MissingToken
        );

        // Mint a valid human access token against the auth state's key + config.
        let cfg = auth.config().clone();
        let scopes = vec!["hive.read".to_string(), "hive.write".to_string()];
        let token = tokens::mint_access_token(
            auth.key(),
            &AccessTokenParams {
                issuer: &cfg.issuer,
                audience: &cfg.audience,
                subject: "11111111-1111-7111-8111-111111111111",
                principal_type: "human",
                scopes: &scopes,
                is_admin: false,
                data_visibility: "owner",
                session_id: "22222222-2222-7222-8222-222222222222",
                now: chrono::Utc::now().timestamp(),
                ttl_secs: 600,
            },
        )
        .expect("mint access token");

        // The same token presented as a Bearer resolves to a Principal: enforce
        // would let this request through.
        let principal = resolve_request(&auth, None, Some(&token))
            .expect("a valid Bearer token must pass the enforce path");
        assert_eq!(principal.subject, "11111111-1111-7111-8111-111111111111");
        assert!(principal.permissions.has_scope("hive.read"));
        assert!(principal.permissions.has_scope("hive.write"));
        assert!(!principal.permissions.has_scope("mcp"));
    }

    /// A token signed by a *different* key must not pass — guards against the
    /// enforce path accepting anything that merely parses as a JWT.
    #[test]
    fn enforce_rejects_token_signed_by_foreign_key() {
        use crate::auth::tokens::{self, AccessTokenParams};

        let auth = test_auth(EnforcementMode::Enforce);
        // A separate, unrelated signing key.
        let (kid, der, _) = test_key();
        let foreign = keys::SigningKey {
            kid: kid.clone(),
            encoding: jsonwebtoken::EncodingKey::from_ed_der(&der),
            decoding: decoding_for(&der),
            public_jwk: serde_json::json!({"kid": kid}),
        };
        let cfg = auth.config().clone();
        let token = tokens::mint_access_token(
            &foreign,
            &AccessTokenParams {
                issuer: &cfg.issuer,
                audience: &cfg.audience,
                subject: "33333333-3333-7333-8333-333333333333",
                principal_type: "human",
                scopes: &["hive.read".to_string()],
                is_admin: false,
                data_visibility: "owner",
                session_id: "44444444-4444-7444-8444-444444444444",
                now: chrono::Utc::now().timestamp(),
                ttl_secs: 600,
            },
        )
        .expect("mint with foreign key");

        let got = resolve_request(&auth, None, Some(&token));
        assert!(
            matches!(got, Err(AuthRejection::InvalidToken(_))),
            "a foreign-signed token must be rejected, got {got:?}"
        );
    }

    #[test]
    fn reject_response_sets_www_authenticate_and_401() {
        let auth = test_auth(EnforcementMode::Enforce);
        let resp = reject_response(&auth, AuthRejection::MissingToken);
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let www = resp
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(www.contains("resource_metadata"));
        assert!(www.contains("oauth-protected-resource"));
    }

    #[test]
    fn insufficient_scope_is_403() {
        let auth = test_auth(EnforcementMode::Enforce);
        let resp = reject_response(&auth, AuthRejection::InsufficientScope("mcp".into()));
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
}
