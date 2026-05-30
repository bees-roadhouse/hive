//! MCP as an OAuth-protected resource (hive-auth-mcp-design.md §3.3, §8 Phase 6).
//!
//! - `GET /.well-known/oauth-protected-resource` — RFC 9728 Protected Resource
//!   Metadata for MCP clients to discover the AS.
//! - `POST /mcp` — Streamable-HTTP JSON-RPC surface (`initialize`, `tools/list`,
//!   `tools/call`). Tools call hive-db directly; canonical writes use `journal_add`.
//!
//! Auth: bearer token required by default (even in global warn mode). Set
//! `HIVE_MCP_OPEN=1` while `HIVE_AUTH_ENFORCE` is unset for tokenless local dev
//! (same posture as tokenless REST in warn mode).

use std::net::SocketAddr;

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::claims::{Principal, PrincipalType};
use crate::auth::config::EnforcementMode;
use crate::auth::extractor::MaybeAuthUser;
use crate::auth::risk::{self, RiskSignals};
use crate::mcp::handle_jsonrpc;
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route("/mcp", post(mcp_post).get(mcp_get))
}

/// RFC 9728 Protected Resource Metadata.
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

fn mcp_open_allowed(state: &AppState) -> bool {
    state.auth.config().mode == EnforcementMode::Warn
        && std::env::var("HIVE_MCP_OPEN")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
}

async fn mcp_get() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        [(header::ALLOW, "POST")],
        Json(json!({ "error": "method_not_allowed", "message": "POST JSON-RPC to /mcp" })),
    )
        .into_response()
}

async fn mcp_post(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    auth: MaybeAuthUser,
    body: String,
) -> Response {
    let cfg = state.auth.config();
    let resource_metadata = format!(
        "{}/.well-known/oauth-protected-resource",
        cfg.issuer.trim_end_matches('/')
    );

    let principal = match auth.0 {
        Some(p) => {
            if p.kind == PrincipalType::Ai {
                let signals = capture_signals(peer, &headers);
                match run_risk(&state, &p, &signals).await {
                    Ok(true) => {
                        return (
                            StatusCode::UNAUTHORIZED,
                            [(
                                header::WWW_AUTHENTICATE,
                                format!(
                                    "Bearer error=\"invalid_token\", error_description=\"reauth_required\", resource_metadata=\"{resource_metadata}\""
                                ),
                            )],
                            Json(json!({ "error": "invalid_token", "error_description": "reauth_required" })),
                        )
                            .into_response();
                    }
                    Ok(false) => {}
                    Err(e) => tracing::warn!(error = %e, "risk scoring failed (non-fatal)"),
                }
            }
            Some(p)
        }
        None if mcp_open_allowed(&state) => None,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                [(
                    header::WWW_AUTHENTICATE,
                    format!("Bearer resource_metadata=\"{resource_metadata}\""),
                )],
                Json(json!({ "error": "invalid_token", "error_description": "MCP requires a bearer token (or HIVE_MCP_OPEN=1 in auth warn mode)" })),
            )
                .into_response();
        }
    };

    if body.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "bad_request", "message": "POST /mcp expects a JSON-RPC body" })),
        )
            .into_response();
    }

    let response = handle_jsonrpc(&state, principal.as_ref(), &body).await;
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        Json(response),
    )
        .into_response()
}

fn capture_signals(peer: SocketAddr, headers: &HeaderMap) -> RiskSignals {
    let xff = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(',').next())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let ip = xff.or_else(|| Some(peer.ip().to_string()));
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    RiskSignals {
        ip,
        user_agent,
        ..Default::default()
    }
}

async fn run_risk(state: &AppState, p: &Principal, signals: &RiskSignals) -> anyhow::Result<bool> {
    let ai_id = p.subject.parse::<Uuid>().ok();
    let act_id = p.act.as_deref().and_then(|a| a.parse::<Uuid>().ok());
    let (Some(ai_id), Some(act_id)) = (ai_id, act_id) else {
        return Ok(false);
    };

    let Some((session_id, jti)) =
        crate::auth::ai::find_live_session(&state.pool, ai_id, act_id).await?
    else {
        return Ok(false);
    };

    let now = chrono::Utc::now();
    let base = risk::load_baseline(&state.pool, session_id).await?;
    let decision = risk::score(signals, &base, now);

    risk::record_signal(&state.pool, session_id, jti, signals, &decision).await?;
    risk::update_baseline(&state.pool, session_id, &base, signals, now).await?;

    let enforce = risk::enforce_enabled();
    let forces = decision.band.forces_rekey();

    if forces {
        risk::record_event(
            &state.pool,
            session_id,
            jti,
            Some(ai_id),
            Some(act_id),
            &decision,
            enforce,
        )
        .await?;
    }

    if forces && enforce {
        if let Some(jti) = jti {
            state.auth.revocations().insert(jti);
        }
        risk::mark_needs_rekey(&state.pool, session_id).await?;
        tracing::warn!(
            session = %session_id, jti = ?jti, band = decision.band.as_str(),
            reasons = ?decision.reasons,
            "RISK: forced re-key (jti invalidated; grant intact, re-connect to re-mint)"
        );
        Ok(true)
    } else if forces {
        tracing::warn!(
            session = %session_id, jti = ?jti, band = decision.band.as_str(),
            reasons = ?decision.reasons,
            "RISK (shadow): would force re-key for jti; set HIVE_RISK_ENFORCE=1 to enforce"
        );
        Ok(false)
    } else {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_open_requires_warn_mode() {
        assert_ne!(EnforcementMode::Enforce, EnforcementMode::Warn);
    }
}
