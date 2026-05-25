//! OAuth 2.1 Authorization Server endpoints (hive-auth-mcp-design.md §8 Phase 2,
//! §3.1). The built-in AS: `/authorize` (auth-code + PKCE + login + consent) and
//! `/token` (authorization_code + refresh_token grants).
//!
//! Phase 2 note: the browser redirect UI is Phase 3. So `/authorize` here is a
//! POST that takes username + password + the PKCE/client params and returns the
//! authorization code in JSON (instead of a 302 to redirect_uri). The token
//! exchange, PKCE verification, session/refresh issuance, and rotation are the
//! real Phase-2 deliverable and are exercised directly. Phase 3 swaps the
//! POST-JSON front for the GET-redirect + HTML login/consent without changing
//! `/token`.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::tokens::{self, AccessTokenParams};
use crate::auth::{password, policy::AuthPolicy, store};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/authorize", post(authorize))
        .route("/token", post(token))
}

/// Client ids that are first-party and skip the consent step (§4.5 Gate 2).
fn is_first_party(client_id: &str) -> bool {
    matches!(client_id, "hive-ui" | "hive-cli")
}

#[derive(Debug, Deserialize)]
struct AuthorizeRequest {
    username: String,
    password: String,
    client_id: String,
    redirect_uri: String,
    /// PKCE S256 challenge (base64url of sha256(verifier)).
    code_challenge: String,
    #[serde(default = "default_challenge_method")]
    code_challenge_method: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    /// Consent decision for non-first-party clients. First-party skips this.
    #[serde(default)]
    consent: Option<bool>,
}

fn default_challenge_method() -> String {
    "S256".to_string()
}

#[derive(Debug, Serialize)]
struct AuthorizeResponse {
    code: String,
    /// Echoed so the (future browser) client can match its redirect.
    redirect_uri: String,
}

/// `/authorize`: authenticate the user (argon2id), apply consent, and issue a
/// single-use authorization code bound to the PKCE challenge.
async fn authorize(
    State(state): State<AppState>,
    Json(req): Json<AuthorizeRequest>,
) -> Result<Json<AuthorizeResponse>, OAuthError> {
    if req.code_challenge_method != "S256" {
        return Err(OAuthError::invalid_request("only S256 PKCE is supported"));
    }
    if req.code_challenge.is_empty() {
        return Err(OAuthError::invalid_request("code_challenge required"));
    }

    // Authenticate: find user, verify password. Uniform failure (no user
    // enumeration): same error whether the user is missing or the password is
    // wrong.
    let user = store::find_user_by_username(&state.pool, &req.username)
        .await
        .map_err(OAuthError::server)?;
    let user = match user {
        Some(u) if u.status == "active" => u,
        _ => return Err(OAuthError::access_denied("invalid credentials")),
    };
    let phc = store::password_hash_for(&state.pool, user.id)
        .await
        .map_err(OAuthError::server)?;
    let ok = phc.is_some_and(|h| password::verify_password(&req.password, &h));
    if !ok {
        return Err(OAuthError::access_denied("invalid credentials"));
    }

    // Consent (§4.5 Gate 2). First-party clients skip; others must pass
    // consent=true. (Persisted TOFU consent is Phase 6 alongside ai grants;
    // Phase 2 requires an explicit per-request yes for third-party clients.)
    if !is_first_party(&req.client_id) && req.consent != Some(true) {
        return Err(OAuthError::access_denied("consent required"));
    }

    // Scopes: intersect the requested scopes with what the user is granted.
    // (Wildcard-granted users get whatever they ask for.)
    let requested: Vec<String> = req
        .scope
        .as_deref()
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    let granted = grant_scopes(&user.granted_scopes, &requested);

    // Mint a single-use auth code (10-minute TTL) bound to the PKCE challenge.
    let code = tokens::generate_refresh_token().raw; // reuse the CSPRNG for an opaque code
    let expires_at = chrono::Utc::now() + chrono::Duration::minutes(10);
    store::insert_auth_code(
        &state.pool,
        &store::NewAuthCode {
            code: &code,
            client_id: &req.client_id,
            user_id: user.id,
            redirect_uri: &req.redirect_uri,
            code_challenge: &req.code_challenge,
            scopes: &granted,
            resource: req.resource.as_deref(),
            expires_at,
        },
    )
    .await
    .map_err(OAuthError::server)?;

    Ok(Json(AuthorizeResponse {
        code,
        redirect_uri: req.redirect_uri,
    }))
}

/// Intersect requested scopes with the user's granted set. A user granted `*`
/// gets everything they request; otherwise only the overlap.
fn grant_scopes(user_granted: &[String], requested: &[String]) -> Vec<String> {
    if user_granted.iter().any(|s| s == "*") {
        return requested.to_vec();
    }
    requested
        .iter()
        .filter(|r| user_granted.iter().any(|g| g == *r))
        .cloned()
        .collect()
}

#[derive(Debug, Deserialize)]
struct TokenRequest {
    grant_type: String,
    // authorization_code grant:
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    code_verifier: Option<String>,
    #[serde(default)]
    redirect_uri: Option<String>,
    // refresh_token grant:
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug, Serialize)]
struct TokenResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: i64,
    refresh_token: String,
    scope: String,
}

/// `/token`: the two Phase-2 grants. Returns an EdDSA access JWT + a rotating
/// opaque refresh token.
async fn token(
    State(state): State<AppState>,
    Json(req): Json<TokenRequest>,
) -> Result<Json<TokenResponse>, OAuthError> {
    let pol = AuthPolicy::load(&state.pool)
        .await
        .map_err(OAuthError::server)?;

    match req.grant_type.as_str() {
        "authorization_code" => token_auth_code(&state, &req, &pol).await,
        "refresh_token" => token_refresh(&state, &req, &pol).await,
        other => Err(OAuthError::unsupported_grant(other)),
    }
}

async fn token_auth_code(
    state: &AppState,
    req: &TokenRequest,
    pol: &AuthPolicy,
) -> Result<Json<TokenResponse>, OAuthError> {
    let code = req
        .code
        .as_deref()
        .ok_or_else(|| OAuthError::invalid_request("code required"))?;
    let verifier = req
        .code_verifier
        .as_deref()
        .ok_or_else(|| OAuthError::invalid_request("code_verifier required (PKCE)"))?;

    let row = store::consume_auth_code(&state.pool, code)
        .await
        .map_err(OAuthError::server)?
        .ok_or_else(|| OAuthError::invalid_grant("unknown or already-used code"))?;

    if row.expires_at <= chrono::Utc::now() {
        return Err(OAuthError::invalid_grant("code expired"));
    }
    if let Some(ru) = &req.redirect_uri
        && ru != &row.redirect_uri
    {
        return Err(OAuthError::invalid_grant("redirect_uri mismatch"));
    }
    if !tokens::verify_pkce_s256(verifier, &row.code_challenge) {
        return Err(OAuthError::invalid_grant("PKCE verification failed"));
    }

    // Fetch the user's authz (is_admin + visibility) for the token claims.
    let (is_admin, visibility) = user_authz(state, row.user_id).await?;

    let session_secs = pol.effective_session_secs(None);
    let issued = store::create_session(
        &state.pool,
        row.user_id,
        &row.client_id,
        &row.scopes,
        &["pwd".to_string()],
        session_secs,
    )
    .await
    .map_err(OAuthError::server)?;

    let access = mint_for_session(
        state,
        &row.user_id.to_string(),
        &row.scopes,
        is_admin,
        &visibility,
        &issued.session_id.to_string(),
        pol.access_secs_within(session_secs),
    )?;

    Ok(Json(TokenResponse {
        access_token: access,
        token_type: "Bearer",
        expires_in: pol.access_secs_within(session_secs),
        refresh_token: issued.refresh.raw,
        scope: row.scopes.join(" "),
    }))
}

async fn token_refresh(
    state: &AppState,
    req: &TokenRequest,
    pol: &AuthPolicy,
) -> Result<Json<TokenResponse>, OAuthError> {
    let presented = req
        .refresh_token
        .as_deref()
        .ok_or_else(|| OAuthError::invalid_request("refresh_token required"))?;

    let refreshed = match store::rotate_refresh_token(&state.pool, presented).await {
        Ok(r) => r,
        Err(store::StoreError::RefreshReuse) => {
            return Err(OAuthError::invalid_grant(
                "refresh token reuse detected; session revoked",
            ));
        }
        Err(store::StoreError::RefreshInvalid) => {
            return Err(OAuthError::invalid_grant("invalid refresh token"));
        }
        Err(e) => return Err(OAuthError::server(e)),
    };

    let (is_admin, visibility) = user_authz(state, refreshed.user_id).await?;
    let remaining = (refreshed.session_expires_at - chrono::Utc::now())
        .num_seconds()
        .max(1);
    let access = mint_for_session(
        state,
        &refreshed.user_id.to_string(),
        &refreshed.scopes,
        is_admin,
        &visibility,
        &refreshed.session_id.to_string(),
        pol.access_secs_within(remaining),
    )?;

    Ok(Json(TokenResponse {
        access_token: access,
        token_type: "Bearer",
        expires_in: pol.access_secs_within(remaining),
        refresh_token: refreshed.new_refresh.raw,
        scope: refreshed.scopes.join(" "),
    }))
}

/// Look up a user's admin flag + data-visibility for the token claims.
/// Visibility is `shared` for admins, `owner` otherwise (Phase 2 default; the
/// per-AI configurable visibility lands in Phase 6).
async fn user_authz(state: &AppState, user_id: uuid::Uuid) -> Result<(bool, String), OAuthError> {
    let row = sqlx::query_as::<_, (bool,)>("SELECT is_admin FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| OAuthError::server(store::StoreError::Sqlx(e)))?
        .ok_or_else(|| OAuthError::invalid_grant("user no longer exists"))?;
    let is_admin = row.0;
    let visibility = if is_admin { "shared" } else { "owner" };
    Ok((is_admin, visibility.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn mint_for_session(
    state: &AppState,
    subject: &str,
    scopes: &[String],
    is_admin: bool,
    visibility: &str,
    session_id: &str,
    ttl_secs: i64,
) -> Result<String, OAuthError> {
    let cfg = state.auth.config();
    tokens::mint_access_token(
        state.auth.key(),
        &AccessTokenParams {
            issuer: &cfg.issuer,
            audience: &cfg.audience,
            subject,
            principal_type: "human",
            scopes,
            is_admin,
            data_visibility: visibility,
            session_id,
            now: chrono::Utc::now().timestamp(),
            ttl_secs,
        },
    )
    .map_err(|e| OAuthError::Server(e.to_string()))
}

/// OAuth-style error response (RFC 6749 §5.2 shape).
enum OAuthError {
    InvalidRequest(String),
    InvalidGrant(String),
    UnsupportedGrantType(String),
    AccessDenied(String),
    Server(String),
}

impl OAuthError {
    fn invalid_request(m: &str) -> Self {
        OAuthError::InvalidRequest(m.to_string())
    }
    fn invalid_grant(m: &str) -> Self {
        OAuthError::InvalidGrant(m.to_string())
    }
    fn unsupported_grant(m: &str) -> Self {
        OAuthError::UnsupportedGrantType(m.to_string())
    }
    fn access_denied(m: &str) -> Self {
        OAuthError::AccessDenied(m.to_string())
    }
    fn server(e: impl std::fmt::Display) -> Self {
        OAuthError::Server(e.to_string())
    }
}

impl axum::response::IntoResponse for OAuthError {
    fn into_response(self) -> axum::response::Response {
        let (status, error, desc) = match self {
            OAuthError::InvalidRequest(m) => (StatusCode::BAD_REQUEST, "invalid_request", m),
            OAuthError::InvalidGrant(m) => (StatusCode::BAD_REQUEST, "invalid_grant", m),
            OAuthError::UnsupportedGrantType(m) => {
                (StatusCode::BAD_REQUEST, "unsupported_grant_type", m)
            }
            OAuthError::AccessDenied(m) => (StatusCode::UNAUTHORIZED, "access_denied", m),
            OAuthError::Server(m) => {
                tracing::error!(error = %m, "oauth server error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "server_error",
                    "internal error".to_string(),
                )
            }
        };
        let body: Value = json!({ "error": error, "error_description": desc });
        (status, Json(body)).into_response()
    }
}
