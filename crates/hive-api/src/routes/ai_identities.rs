//! AI identity + grant management and MCP token issuance + the revocation/
//! listing surface (hive-auth-mcp-design.md §1.5, §3.4, §5.5).
//!
//! These routes are the Phase-6 heart: humans create AI identities, grant their
//! AIs scoped access, connect AS an AI over MCP (minting the act-claim token),
//! and see + revoke every live AI connection. Authorization runs through the
//! authenticated `Principal` the auth layer resolved (owner/admin gating, §5.5)
//! — the dev-bypass principal has full authority and passes every gate.
//!
//! Routes live under `/ai-identities/*` to keep the bare `/ai` path free for
//! the AI directory (migration 0013, `crate::routes::ai`). The two concepts
//! are intentionally separate: the directory is "who are pia/apis/cera";
//! `ai_identities` is "this AI has scope X granted by user Y."
//!
//! Runtime sqlx only; no API keys — every token is an OAuth/OIDC JWT.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::ai::{self, RevokeScope};
use crate::auth::claims::PrincipalType;
use crate::auth::extractor::MaybeAuthUser;
use crate::auth::tokens::{self, McpTokenParams};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ai-identities", get(list_identities).post(create_identity))
        .route("/ai-identities/{handle}/grants", post(create_grant))
        .route(
            "/ai-identities/{handle}/grants/{user_id}",
            axum::routing::delete(delete_grant),
        )
        .route("/ai-identities/{handle}/connect", post(connect))
        .route("/ai-identities/{handle}/connections", get(ai_connections))
        .route("/me/ai-connections", get(my_connections))
        .route("/admin/ai-connections", get(admin_connections))
        .route("/ai-identities/connections/{jti}/revoke", post(revoke_one))
        .route("/admin/ai-connections/revoke-all", post(revoke_all))
}

// ---------- authorization helpers ----------

/// A resolved human caller: their user id + admin flag. Errors map to 401/403.
struct Caller {
    user_id: Uuid,
    is_admin: bool,
    is_dev: bool,
}

/// Require an authenticated HUMAN (or dev) principal. AI principals can't manage
/// identities/grants (they'd be acting as themselves). Returns the caller.
fn require_human(auth: &MaybeAuthUser) -> Result<Caller, ApiError> {
    let p = auth.0.as_ref().ok_or(ApiError::Unauthorized)?;
    match p.kind {
        PrincipalType::Dev => Ok(Caller {
            // Dev-bypass has no real user row; management ops that need a real
            // owner id use the dev sentinel only for gating, not for FK writes.
            user_id: Uuid::nil(),
            is_admin: true,
            is_dev: true,
        }),
        PrincipalType::Human => {
            let user_id = p
                .subject
                .parse::<Uuid>()
                .map_err(|_| ApiError::Unauthorized)?;
            Ok(Caller {
                user_id,
                is_admin: p.permissions.is_admin,
                is_dev: false,
            })
        }
        PrincipalType::Ai => Err(ApiError::Forbidden(
            "AI principals cannot manage identities or grants".into(),
        )),
    }
}

/// Is the caller allowed to administer this AI (owner or admin)? Dev passes.
fn can_admin_ai(caller: &Caller, ai: &ai::AiIdentity) -> bool {
    caller.is_dev || caller.is_admin || ai.owned_by == caller.user_id
}

// ---------- identity management ----------

#[derive(Debug, Deserialize)]
struct CreateIdentity {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default = "default_kind")]
    kind: String,
    /// Owner user id. Optional: defaults to the caller (a human owns the AIs
    /// they create). Admins/dev may set it to assign ownership to another user.
    #[serde(default)]
    owned_by: Option<Uuid>,
}

fn default_kind() -> String {
    "assistant".to_string()
}

#[derive(Debug, Serialize)]
struct IdentityView {
    id: Uuid,
    name: String,
    display_name: Option<String>,
    kind: String,
    owned_by: Uuid,
    status: String,
}

impl From<ai::AiIdentity> for IdentityView {
    fn from(a: ai::AiIdentity) -> Self {
        IdentityView {
            id: a.id,
            name: a.name,
            display_name: a.display_name,
            kind: a.kind,
            owned_by: a.owned_by,
            status: a.status,
        }
    }
}

async fn create_identity(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Json(req): Json<CreateIdentity>,
) -> Result<(StatusCode, Json<IdentityView>), ApiError> {
    let caller = require_human(&auth)?;
    // Non-admins can only create AIs owned by themselves.
    let owner = match req.owned_by {
        Some(o) if o != caller.user_id && !(caller.is_admin || caller.is_dev) => {
            return Err(ApiError::Forbidden(
                "only an admin can assign an AI to another owner".into(),
            ));
        }
        Some(o) => o,
        None => caller.user_id,
    };
    if owner.is_nil() {
        return Err(ApiError::BadRequest(
            "owned_by is required when the caller has no real user id (dev/admin must specify)"
                .into(),
        ));
    }
    let id = ai::create_ai_identity(
        &state.pool,
        &req.name,
        req.display_name.as_deref(),
        &req.kind,
        owner,
    )
    .await
    .map_err(ApiError::from)?;

    let view = IdentityView {
        id,
        name: req.name,
        display_name: req.display_name,
        kind: req.kind,
        owned_by: owner,
        status: "active".into(),
    };
    Ok((StatusCode::CREATED, Json(view)))
}

async fn list_identities(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
) -> Result<Json<Vec<IdentityView>>, ApiError> {
    let caller = require_human(&auth)?;
    // Admin/dev see all; a plain human sees the AIs they own.
    let owner_filter = if caller.is_admin || caller.is_dev {
        None
    } else {
        Some(caller.user_id)
    };
    let rows = ai::list_ai_identities(&state.pool, owner_filter)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(rows.into_iter().map(IdentityView::from).collect()))
}

// ---------- grant management ----------

#[derive(Debug, Deserialize)]
struct CreateGrant {
    /// The user whose grant this is (what THIS human lets the AI do AS them).
    /// Defaults to the caller. Admin/dev may grant on behalf of another user.
    #[serde(default)]
    user_id: Option<Uuid>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default = "default_visibility")]
    data_visibility: String,
    /// Default true (non-expiring MCP token); false opts this AI into expiry.
    #[serde(default = "default_true")]
    mcp_token_no_expiry: bool,
}

fn default_visibility() -> String {
    "owner".to_string()
}
fn default_true() -> bool {
    true
}

async fn create_grant(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(handle): Path<String>,
    Json(req): Json<CreateGrant>,
) -> Result<Json<Value>, ApiError> {
    let caller = require_human(&auth)?;
    let ai_row = lookup_ai(&state, &handle).await?;
    if !can_admin_ai(&caller, &ai_row) {
        return Err(ApiError::Forbidden("not an owner of this AI".into()));
    }
    // The grant's subject defaults to the caller; admin/dev may set another.
    let grant_user = match req.user_id {
        Some(u) if u != caller.user_id && !(caller.is_admin || caller.is_dev) => {
            return Err(ApiError::Forbidden(
                "only an admin can configure a grant for another user".into(),
            ));
        }
        Some(u) => u,
        None => caller.user_id,
    };
    if grant_user.is_nil() {
        return Err(ApiError::BadRequest(
            "user_id is required for this caller".into(),
        ));
    }
    let id = ai::upsert_grant(
        &state.pool,
        ai_row.id,
        grant_user,
        &req.scopes,
        &req.data_visibility,
        req.mcp_token_no_expiry,
    )
    .await
    .map_err(ApiError::from)?;

    // Reconfig → revoke (§3.4/§5.5): tightening (or any change to) a grant
    // revokes that (AI, user)'s live MCP tokens so the AI re-connects under the
    // new grant. Widening could wait, but uniformly re-minting on change is
    // simplest and always correct.
    let revoked = ai::revoke(
        &state.pool,
        &RevokeScope::AiActor {
            ai_id: ai_row.id,
            act_user_id: grant_user,
        },
        caller.user_id,
        "grant reconfigured",
    )
    .await
    .map_err(ApiError::from)?;
    state
        .auth
        .revocations()
        .insert_many(revoked.iter().copied());

    Ok(Json(json!({
        "grant_id": id,
        "ai": ai_row.name,
        "user_id": grant_user,
        "revoked_tokens": revoked.len(),
    })))
}

async fn delete_grant(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path((handle, user_id)): Path<(String, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let caller = require_human(&auth)?;
    let ai_row = lookup_ai(&state, &handle).await?;
    if !can_admin_ai(&caller, &ai_row) {
        return Err(ApiError::Forbidden("not an owner of this AI".into()));
    }
    ai::revoke_grant(&state.pool, ai_row.id, user_id)
        .await
        .map_err(ApiError::from)?;
    // Revoking the grant kills that (AI, user)'s live tokens too.
    let revoked = ai::revoke(
        &state.pool,
        &RevokeScope::AiActor {
            ai_id: ai_row.id,
            act_user_id: user_id,
        },
        caller.user_id,
        "grant revoked",
    )
    .await
    .map_err(ApiError::from)?;
    state
        .auth
        .revocations()
        .insert_many(revoked.iter().copied());
    Ok(Json(json!({ "revoked_tokens": revoked.len() })))
}

// ---------- MCP token issuance (the heart, §3.4) ----------

#[derive(Debug, Deserialize)]
struct ConnectRequest {
    /// The connecting human (the actor). Defaults to the caller. Admin/dev may
    /// connect on behalf of another owner for testing.
    #[serde(default)]
    act_user_id: Option<Uuid>,
    /// Optional scope narrowing: the issued scope is the requested set ∩ the
    /// grant ∩ the human's own scopes. Omit to use the full grant.
    #[serde(default)]
    scope: Option<String>,
    /// Client id of the connecting MCP client (recorded on the session).
    #[serde(default = "default_client")]
    client_id: String,
}

fn default_client() -> String {
    "mcp-client".to_string()
}

#[derive(Debug, Serialize)]
struct ConnectResponse {
    access_token: String,
    token_type: &'static str,
    /// null => non-expiring (the default MCP class, §2).
    expires_in: Option<i64>,
    scope: String,
    /// The bound MCP resource (RFC 8707 audience).
    resource: String,
    ai: String,
}

/// `POST /ai-identities/{handle}/connect`: a human owner connects AS an AI over MCP. Mints
/// a token with `sub` = AI, `act` = human, scope = the intersection (§3.4),
/// records a revocable `mcp_ai` session. Default non-expiring.
async fn connect(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(handle): Path<String>,
    Json(req): Json<ConnectRequest>,
) -> Result<Json<ConnectResponse>, ApiError> {
    let caller = require_human(&auth)?;
    let ai_row = lookup_ai(&state, &handle).await?;

    // The actor is the connecting human. Default = caller; admin/dev may act for
    // another owner. A plain human can only connect as themselves.
    let actor = match req.act_user_id {
        Some(u) if u != caller.user_id && !(caller.is_admin || caller.is_dev) => {
            return Err(ApiError::Forbidden(
                "cannot connect an AI on behalf of another user".into(),
            ));
        }
        Some(u) => u,
        None => caller.user_id,
    };
    if actor.is_nil() {
        return Err(ApiError::BadRequest(
            "act_user_id required for this caller".into(),
        ));
    }

    // The grant for THIS (AI, actor). No active grant => the actor hasn't
    // authorized this Ai to act as them.
    let grant = ai::active_grant(&state.pool, ai_row.id, actor)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| {
            ApiError::Forbidden(format!(
                "no active grant: {} has not authorized '{}' to act as them",
                actor, ai_row.name
            ))
        })?;

    // Intersection ceiling (§3.4): grant ∩ the actor's own scopes ∩ requested.
    let human_scopes = user_granted_scopes(&state, actor).await?;
    let requested: Option<Vec<String>> = req
        .scope
        .as_deref()
        .map(|s| s.split_whitespace().map(str::to_string).collect());
    let effective = intersect_scopes(&grant.granted_scopes, &human_scopes, requested.as_deref());

    // Non-expiring unless the grant opted into expiry.
    let exp_secs = if grant.mcp_token_no_expiry {
        None
    } else {
        // Opt-in expiry borrows the policy access TTL as a sane bound.
        let pol = crate::auth::policy::AuthPolicy::load(&state.pool)
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        Some(pol.access_token_secs)
    };
    let expires_at = exp_secs.map(|s| chrono::Utc::now() + chrono::Duration::seconds(s));

    // Record the revocable session first so the jti exists as a handle (§5.5).
    let issued = ai::create_mcp_session(
        &state.pool,
        ai_row.id,
        actor,
        &req.client_id,
        &effective,
        expires_at,
    )
    .await
    .map_err(ApiError::from)?;

    let cfg = state.auth.config();
    let resource = cfg.mcp_resource();
    let token = tokens::mint_mcp_token(
        state.auth.key(),
        &McpTokenParams {
            issuer: &cfg.issuer,
            audience: &resource,
            ai_subject: &ai_row.id.to_string(),
            act_subject: &actor.to_string(),
            scopes: &effective,
            data_visibility: &grant.data_visibility,
            session_id: &issued.session_id.to_string(),
            jti: &issued.jti.to_string(),
            now: chrono::Utc::now().timestamp(),
            exp_secs,
        },
    )
    .map_err(|e| ApiError::Internal(e.to_string()))?;

    tracing::info!(
        ai = %ai_row.name, actor = %actor, jti = %issued.jti, no_expiry = grant.mcp_token_no_expiry,
        "minted MCP token (AI acting for human)"
    );

    Ok(Json(ConnectResponse {
        access_token: token,
        token_type: "Bearer",
        expires_in: exp_secs,
        scope: effective.join(" "),
        resource,
        ai: ai_row.name,
    }))
}

// ---------- listing + revocation surface (§5.5) ----------

#[derive(Debug, Serialize)]
struct ConnectionView {
    jti: Option<Uuid>,
    ai: String,
    ai_id: Uuid,
    act_user_id: Uuid,
    scopes: Vec<String>,
    client_id: String,
    expires_at: Option<String>,
    last_seen_at: Option<String>,
    created_at: Option<String>,
}

impl From<ai::AiConnection> for ConnectionView {
    fn from(c: ai::AiConnection) -> Self {
        ConnectionView {
            jti: c.jti,
            ai: c.ai_name,
            ai_id: c.ai_id,
            act_user_id: c.act_user_id,
            scopes: c.scopes,
            client_id: c.client_id,
            expires_at: c.expires_at.map(|t| t.to_rfc3339()),
            last_seen_at: c.last_seen_at.map(|t| t.to_rfc3339()),
            created_at: c.created_at.map(|t| t.to_rfc3339()),
        }
    }
}

/// `GET /me/ai-connections`: connections the caller is the actor on (§5.5).
async fn my_connections(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
) -> Result<Json<Vec<ConnectionView>>, ApiError> {
    let caller = require_human(&auth)?;
    if caller.user_id.is_nil() {
        return Ok(Json(Vec::new())); // dev sentinel has no actor rows
    }
    let rows = ai::connections_for_actor(&state.pool, caller.user_id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(rows.into_iter().map(ConnectionView::from).collect()))
}

/// `GET /ai-identities/{handle}/connections`: all connections for an AI the caller owns.
async fn ai_connections(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(handle): Path<String>,
) -> Result<Json<Vec<ConnectionView>>, ApiError> {
    let caller = require_human(&auth)?;
    let ai_row = lookup_ai(&state, &handle).await?;
    if !can_admin_ai(&caller, &ai_row) {
        return Err(ApiError::Forbidden("not an owner of this AI".into()));
    }
    let rows = ai::connections_for_ai(&state.pool, ai_row.id)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(rows.into_iter().map(ConnectionView::from).collect()))
}

/// `GET /admin/ai-connections`: every active connection (admin/dev only).
async fn admin_connections(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
) -> Result<Json<Vec<ConnectionView>>, ApiError> {
    let caller = require_human(&auth)?;
    if !(caller.is_admin || caller.is_dev) {
        return Err(ApiError::Forbidden("admin only".into()));
    }
    let rows = ai::all_connections(&state.pool)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(rows.into_iter().map(ConnectionView::from).collect()))
}

/// `POST /ai-identities/connections/{jti}/revoke`: revoke one token (scope #1, §5.5).
/// The caller must be the actor on that connection, an owner of the AI, or
/// admin.
async fn revoke_one(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Path(jti): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let caller = require_human(&auth)?;
    // Authorize: find the session for this jti and check actor/owner/admin.
    let owns = revoke_one_authorized(&state, &caller, &jti).await?;
    if !owns {
        return Err(ApiError::Forbidden(
            "not allowed to revoke this token".into(),
        ));
    }
    let revoked = ai::revoke(
        &state.pool,
        &RevokeScope::Jti(jti),
        caller.user_id,
        "owner revoke",
    )
    .await
    .map_err(ApiError::from)?;
    state
        .auth
        .revocations()
        .insert_many(revoked.iter().copied());
    Ok(Json(json!({ "revoked": revoked.len() })))
}

/// `POST /admin/ai-connections/revoke-all`: the global kill-switch (scope #5).
async fn revoke_all(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
) -> Result<Json<Value>, ApiError> {
    let caller = require_human(&auth)?;
    if !(caller.is_admin || caller.is_dev) {
        return Err(ApiError::Forbidden("admin only (break-glass)".into()));
    }
    let revoked = ai::revoke(
        &state.pool,
        &RevokeScope::All,
        caller.user_id,
        "global kill-switch",
    )
    .await
    .map_err(ApiError::from)?;
    state
        .auth
        .revocations()
        .insert_many(revoked.iter().copied());
    tracing::warn!(count = revoked.len(), by = %caller.user_id, "GLOBAL MCP kill-switch invoked");
    Ok(Json(json!({ "revoked": revoked.len() })))
}

// ---------- internal helpers ----------

async fn lookup_ai(state: &AppState, handle: &str) -> Result<ai::AiIdentity, ApiError> {
    ai::find_ai_by_name(&state.pool, handle)
        .await
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::NotFound(format!("no AI identity '{handle}'")))
}

/// The scopes a human holds (their own ceiling for the intersection). Reads
/// `users.granted_scopes`; a `*` means "no ceiling" (return None-equivalent).
async fn user_granted_scopes(state: &AppState, user_id: Uuid) -> Result<Vec<String>, ApiError> {
    let row = sqlx::query_as::<_, (Vec<String>,)>("SELECT granted_scopes FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .ok_or_else(|| ApiError::Forbidden("connecting user no longer exists".into()))?;
    Ok(row.0)
}

/// Intersection (§3.4): grant ∩ human ∩ requested. A `*` in the human's own
/// scopes means "no human ceiling" (the human can grant anything the AI grant
/// lists). `requested = None` means "use the grant as-is".
fn intersect_scopes(
    grant: &[String],
    human: &[String],
    requested: Option<&[String]>,
) -> Vec<String> {
    let human_unbounded = human.iter().any(|s| s == "*");
    grant
        .iter()
        .filter(|g| g.as_str() != "*") // never propagate a literal wildcard into an AI token
        .filter(|g| human_unbounded || human.iter().any(|h| h == *g))
        .filter(|g| match requested {
            Some(req) => req.iter().any(|r| r == *g),
            None => true,
        })
        .cloned()
        .collect()
}

/// Authorize a single-token revoke: caller is the actor on the jti's session,
/// an owner of its AI, or admin/dev.
async fn revoke_one_authorized(
    state: &AppState,
    caller: &Caller,
    jti: &Uuid,
) -> Result<bool, ApiError> {
    if caller.is_admin || caller.is_dev {
        return Ok(true);
    }
    let row = sqlx::query_as::<_, (Uuid, Option<Uuid>, Uuid)>(
        "SELECT s.ai_id, s.act_user_id, a.owned_by \
         FROM sessions s JOIN ai_identities a ON a.id = s.ai_id \
         WHERE s.jti = $1 AND s.kind = 'mcp_ai'",
    )
    .bind(jti)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    let Some((_ai_id, act_user_id, owned_by)) = row else {
        return Err(ApiError::NotFound("no such connection".into()));
    };
    Ok(act_user_id == Some(caller.user_id) || owned_by == caller.user_id)
}

// ---------- error type ----------

enum ApiError {
    Unauthorized,
    Forbidden(String),
    NotFound(String),
    BadRequest(String),
    Internal(String),
}

impl From<crate::auth::store::StoreError> for ApiError {
    fn from(e: crate::auth::store::StoreError) -> Self {
        ApiError::Internal(e.to_string())
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "authentication required".into()),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(m) => {
                tracing::error!(error = %m, "ai route internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersection_caps_at_grant_and_human() {
        let grant = vec![
            "journal.read".into(),
            "journal.write".into(),
            "tasks.read".into(),
        ];
        let human = vec!["journal.read".into(), "journal.write".into()];
        // human lacks tasks.read => it's dropped even though the grant has it.
        let got = intersect_scopes(&grant, &human, None);
        assert_eq!(
            got,
            vec!["journal.read".to_string(), "journal.write".to_string()]
        );
    }

    #[test]
    fn requested_narrows_further() {
        let grant = vec!["journal.read".into(), "journal.write".into()];
        let human = vec!["*".into()]; // unbounded human
        let requested = vec!["journal.read".into()];
        let got = intersect_scopes(&grant, &human, Some(&requested));
        assert_eq!(got, vec!["journal.read".to_string()]);
    }

    #[test]
    fn wildcard_never_propagates_into_ai_token() {
        // Even a wildcard grant must not put a literal "*" in an AI token.
        let grant = vec!["*".into(), "journal.read".into()];
        let human = vec!["*".into()];
        let got = intersect_scopes(&grant, &human, None);
        assert_eq!(got, vec!["journal.read".to_string()]);
    }

    #[test]
    fn human_ceiling_blocks_overbroad_grant() {
        let grant = vec!["finance.read".into()];
        let human = vec!["journal.read".into()]; // can't see finance
        let got = intersect_scopes(&grant, &human, None);
        assert!(got.is_empty(), "AI can't exceed the human's own reach");
    }
}
