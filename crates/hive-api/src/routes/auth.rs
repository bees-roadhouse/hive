//! Auth discovery endpoints (hive-auth-mcp-design.md §8 Phase 1, §3.3).
//!
//! Phase 1 publishes the verifying side of the local AS:
//! - `GET /jwks.json` — the JWK Set (the active Ed25519 public key).
//! - `GET /.well-known/oauth-authorization-server` — RFC 8414 AS metadata,
//!   a STUB advertising only what exists so far. Endpoints like `/token`,
//!   `/authorize`, `/device_authorization`, `/register` are listed as they
//!   land in later phases; for now we advertise the issuer, jwks_uri, EdDSA
//!   signing, and the grant/PKCE capabilities the design commits to.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde_json::{Value, json};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/jwks.json", get(jwks))
        .route("/.well-known/oauth-authorization-server", get(as_metadata))
}

async fn jwks(State(state): State<AppState>) -> Json<Value> {
    Json(crate::auth::keys::jwks_document(state.auth.key()))
}

async fn as_metadata(State(state): State<AppState>) -> Json<Value> {
    let issuer = state.auth.config().issuer.trim_end_matches('/').to_string();
    // RFC 8414 metadata. Endpoint URLs are advertised against the issuer base;
    // the endpoints themselves arrive in Phase 2+ (this is a Phase 1 stub).
    Json(json!({
        "issuer": issuer,
        "jwks_uri": format!("{issuer}/jwks.json"),
        "authorization_endpoint": format!("{issuer}/authorize"),
        "token_endpoint": format!("{issuer}/token"),
        "device_authorization_endpoint": format!("{issuer}/device_authorization"),
        "registration_endpoint": format!("{issuer}/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": [
            "authorization_code",
            "refresh_token",
            "urn:ietf:params:oauth:grant-type:device_code"
        ],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "id_token_signing_alg_values_supported": ["EdDSA"],
        // Phase 1 marker so consumers know endpoints are not all live yet.
        "x_hive_phase": 1
    }))
}
