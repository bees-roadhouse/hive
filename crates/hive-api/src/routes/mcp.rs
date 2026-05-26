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
//!
//! Phase 7 (§5.7) hooks the risk engine in here: each AI-token use on `/mcp` is
//! scored against the session baseline (IP/UA/cadence) and — in shadow mode by
//! default — logged as "would force re-key"; under `HIVE_RISK_ENFORCE` an
//! anomalous use invalidates the jti (re-key, not revoke).

use std::net::SocketAddr;

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::claims::{Principal, PrincipalType};
use crate::auth::extractor::MaybeAuthUser;
use crate::auth::risk::{self, RiskSignals};
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
/// passes; anything else triggers discovery. For an AI principal, the Phase-7
/// risk engine scores this use (§5.7) before serving.
async fn mcp_endpoint(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    auth: MaybeAuthUser,
) -> Response {
    let cfg = state.auth.config();
    let resource_metadata = format!(
        "{}/.well-known/oauth-protected-resource",
        cfg.issuer.trim_end_matches('/')
    );

    match auth.0 {
        Some(p) => {
            // Phase 7: score AI-token uses against the session baseline. Only AI
            // principals (the non-expiring class) are scored; human/dev skip.
            let mut rekeyed = false;
            if p.kind == PrincipalType::Ai {
                let signals = capture_signals(peer, &headers);
                match run_risk(&state, &p, &signals).await {
                    Ok(forced) => rekeyed = forced,
                    Err(e) => tracing::warn!(error = %e, "risk scoring failed (non-fatal)"),
                }
                // Enforced re-key: the jti is now revoked, so reject this request
                // with the reauth challenge — the client re-connects (re-key).
                if rekeyed {
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
            }

            let acting = match p.kind {
                PrincipalType::Ai => json!({ "principal": "ai", "sub": p.subject, "act": p.act }),
                PrincipalType::Human => json!({ "principal": "human", "sub": p.subject }),
                PrincipalType::Dev => json!({ "principal": "dev" }),
            };
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

/// Extract the per-use risk signals from the request: client IP (trusted
/// `X-Forwarded-For` first hop if present, else the socket peer) + user-agent.
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

/// Run the Phase-7 risk pipeline for one AI-token use: locate the session,
/// score against its baseline, record the signal + audit event, update the
/// baseline. In shadow mode (default) this only logs/records; under
/// `HIVE_RISK_ENFORCE` a MEDIUM/HIGH band invalidates the jti (revocation set)
/// and marks the session needs_rekey. Returns true iff the token was re-keyed.
async fn run_risk(state: &AppState, p: &Principal, signals: &RiskSignals) -> anyhow::Result<bool> {
    // sub = AI id; act = connecting human id. Need both to find the session.
    let ai_id = p.subject.parse::<Uuid>().ok();
    let act_id = p.act.as_deref().and_then(|a| a.parse::<Uuid>().ok());
    let (Some(ai_id), Some(act_id)) = (ai_id, act_id) else {
        return Ok(false); // not a well-formed AI token; nothing to score
    };

    let Some((session_id, jti)) =
        crate::auth::ai::find_live_session(&state.pool, ai_id, act_id).await?
    else {
        return Ok(false); // no live session row (e.g. dev/synthetic) — skip
    };

    let now = chrono::Utc::now();
    let base = risk::load_baseline(&state.pool, session_id).await?;
    let decision = risk::score(signals, &base, now);

    // Always record the signal + advance the baseline (the score compared
    // against the PRIOR baseline, so update after scoring).
    risk::record_signal(&state.pool, session_id, jti, signals, &decision).await?;
    risk::update_baseline(&state.pool, session_id, &base, signals, now).await?;

    let enforce = risk::enforce_enabled();
    let forces = decision.band.forces_rekey();

    // Audit row (shadow or enforced) for every non-LOW decision.
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
        // Enforced re-key: invalidate the jti (push to the revocation set so
        // it's rejected) + mark the session needs_rekey. The grant + identity
        // stay intact — the next connect mints a fresh token. NOT a revoke.
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
        // Shadow: log what we WOULD do, change nothing.
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
