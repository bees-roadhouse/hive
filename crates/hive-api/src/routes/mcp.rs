//! MCP as an OAuth-protected resource (hive-auth-mcp-design.md §3.3, §8 Phase 6).
//!
//! Phase 6 lands the *protected-resource seam*, not a full MCP tool server:
//! - `GET /.well-known/oauth-protected-resource` — RFC 9728 Protected Resource
//!   Metadata, so an MCP client discovers the AS (its `authorization_servers`)
//!   and the canonical resource URI to bind tokens to.
//! - `/mcp` — the Streamable-HTTP MCP endpoint *seam*. The tool/resource surface
//!   (journal/tasks/notes/wire/search as MCP tools) is a documented follow-up;
//!   here it enforces the RS contract: a request without a valid AI token gets
//!   401 + `WWW-Authenticate: Bearer resource_metadata=...` pointing at the
//!   metadata above (the discovery trigger the spec mandates). A request that
//!   already carries a valid token (resolved by the auth layer) gets a minimal
//!   "ready" acknowledgement so the seam is exercisable end-to-end.
//!
//! No API keys — discovery points only at OAuth endpoints.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use serde_json::{Value, json};

use crate::auth::claims::PrincipalType;
use crate::auth::extractor::MaybeAuthUser;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route("/mcp", any(mcp_endpoint))
}

/// RFC 9728 Protected Resource Metadata. `authorization_servers` MUST be
/// non-empty (the spec MUST) and points at this same service (it's both AS and
/// RS in builtin mode). `resource` is the canonical MCP URI tokens bind to.
async fn protected_resource_metadata(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.auth.config();
    let issuer = cfg.issuer.trim_end_matches('/').to_string();
    Json(json!({
        "resource": cfg.mcp_resource(),
        "authorization_servers": [issuer],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["mcp", "journal.read", "journal.write", "tasks.read", "tasks.write", "notes.read", "notes.write", "wire.read"],
        "resource_documentation": format!("{issuer}/.well-known/oauth-authorization-server"),
    }))
}

/// The `/mcp` endpoint seam. Enforces the RS contract regardless of the global
/// warn/enforce mode: MCP clients MUST be able to discover the AS via a 401 +
/// `WWW-Authenticate` (§3.3). A valid AI principal (resolved by the auth layer)
/// passes; anything else triggers discovery.
async fn mcp_endpoint(State(state): State<AppState>, auth: MaybeAuthUser) -> Response {
    let cfg = state.auth.config();
    let resource_metadata = format!(
        "{}/.well-known/oauth-protected-resource",
        cfg.issuer.trim_end_matches('/')
    );

    match auth.0 {
        // A token resolved to a principal. MCP tokens are AI principals; a human
        // or dev token is also accepted here (dev convenience / human probing),
        // but the intended caller is an AI acting for a human (§3.4).
        Some(p) => {
            let acting = match p.kind {
                PrincipalType::Ai => json!({ "principal": "ai", "sub": p.subject, "act": p.act }),
                PrincipalType::Human => json!({ "principal": "human", "sub": p.subject }),
                PrincipalType::Dev => json!({ "principal": "dev" }),
            };
            // The actual MCP JSON-RPC tool dispatch lands in a follow-up; this
            // seam confirms auth + audience are wired so a client can connect.
            Json(json!({
                "mcp": "ready",
                "resource": cfg.mcp_resource(),
                "note": "tool surface is a Phase-6+ follow-up; auth seam is live",
                "acting_as": acting,
                "scopes": p.permissions.scopes,
            }))
            .into_response()
        }
        // No valid token: emit the RFC 9728 discovery challenge (spec MUST).
        None => (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                format!("Bearer resource_metadata=\"{resource_metadata}\""),
            )],
            Json(json!({ "error": "invalid_token", "error_description": "MCP requires a bearer token" })),
        )
            .into_response(),
    }
}
