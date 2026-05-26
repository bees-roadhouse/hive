//! TOTP MFA enrollment + management (hive-auth-mcp-design.md §4).
//!
//! The login-time second-factor check lives in the /authorize flow
//! (routes/oauth.rs); these routes are the *enrollment* surface: a logged-in
//! human generates a TOTP secret, confirms it with a live code, and gets
//! one-time recovery codes. Until confirmed, enrollment doesn't gate login.
//!
//! All routes require an authenticated HUMAN principal (the auth layer resolved
//! it). An AI/dev principal can't enroll a human's MFA. No API keys.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::claims::PrincipalType;
use crate::auth::extractor::MaybeAuthUser;
use crate::auth::{mfa, store, totp};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mfa/status", get(status))
        .route("/mfa/totp/enroll", post(enroll))
        .route("/mfa/totp/verify", post(verify))
        .route("/mfa/totp", axum::routing::delete(disable))
}

/// Resolve the authenticated human's user id (enrollment is human-only).
async fn require_human_user(
    state: &AppState,
    auth: &MaybeAuthUser,
) -> Result<(Uuid, store::User), MfaError> {
    let p = auth.0.as_ref().ok_or(MfaError::Unauthorized)?;
    if p.kind != PrincipalType::Human {
        return Err(MfaError::Forbidden(
            "MFA enrollment is for human accounts".into(),
        ));
    }
    let user_id = p
        .subject
        .parse::<Uuid>()
        .map_err(|_| MfaError::Unauthorized)?;
    // Load the username for the otpauth:// label.
    let user = store::find_user_by_id(&state.pool, user_id)
        .await
        .map_err(MfaError::from)?
        .ok_or(MfaError::Unauthorized)?;
    Ok((user_id, user))
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    enrolled: bool,
    confirmed: bool,
    recovery_codes_remaining: i64,
}

/// `GET /mfa/status`: where the caller stands on MFA.
async fn status(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
) -> Result<Json<StatusResponse>, MfaError> {
    let (user_id, _) = require_human_user(&state, &auth).await?;
    let cred = mfa::get_credential(&state.pool, user_id)
        .await
        .map_err(MfaError::from)?;
    let remaining = mfa::remaining_recovery_codes(&state.pool, user_id)
        .await
        .map_err(MfaError::from)?;
    Ok(Json(StatusResponse {
        enrolled: cred.is_some(),
        confirmed: cred.is_some_and(|c| c.is_confirmed()),
        recovery_codes_remaining: remaining,
    }))
}

#[derive(Debug, Serialize)]
struct EnrollResponse {
    /// The base32 secret (also embedded in the URI) — shown so the user can
    /// hand-enter it if they can't scan.
    secret: String,
    /// The otpauth:// provisioning URI an authenticator app scans (render as QR
    /// client-side; we don't ship a PNG in this phase).
    otpauth_uri: String,
}

/// `POST /mfa/totp/enroll`: generate a fresh secret, store it encrypted +
/// pending, and return the provisioning URI. Re-enrolling overwrites a prior
/// *pending* secret. (A confirmed credential must be disabled before re-enroll.)
async fn enroll(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
) -> Result<Json<EnrollResponse>, MfaError> {
    let (user_id, user) = require_human_user(&state, &auth).await?;

    // Don't silently clobber a confirmed credential.
    if let Some(c) = mfa::get_credential(&state.pool, user_id)
        .await
        .map_err(MfaError::from)?
        && c.is_confirmed()
    {
        return Err(MfaError::Conflict(
            "MFA already active; disable it before re-enrolling".into(),
        ));
    }

    let secret = totp::generate_secret_base32();
    let enc = totp::encrypt_secret(&secret).map_err(|e| MfaError::Internal(e.to_string()))?;
    mfa::upsert_pending_secret(&state.pool, user_id, &enc)
        .await
        .map_err(MfaError::from)?;

    let issuer = mfa_issuer(&state);
    let uri = totp::provisioning_uri(&secret, &issuer, &user.username);
    Ok(Json(EnrollResponse {
        secret,
        otpauth_uri: uri,
    }))
}

#[derive(Debug, Deserialize)]
struct VerifyRequest {
    code: String,
}

#[derive(Debug, Serialize)]
struct VerifyResponse {
    confirmed: bool,
    /// One-time recovery codes, shown ONCE here; stored hashed. The user must
    /// save these — they're the only way in if the authenticator is lost.
    recovery_codes: Vec<String>,
}

/// `POST /mfa/totp/verify`: confirm enrollment by submitting a current code.
/// On success, marks the credential confirmed and issues recovery codes.
async fn verify(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Json(req): Json<VerifyRequest>,
) -> Result<Json<VerifyResponse>, MfaError> {
    let (user_id, _) = require_human_user(&state, &auth).await?;
    let cred = mfa::get_credential(&state.pool, user_id)
        .await
        .map_err(MfaError::from)?
        .ok_or_else(|| MfaError::BadRequest("no pending enrollment; call enroll first".into()))?;

    let secret =
        totp::decrypt_secret(&cred.secret_enc).map_err(|e| MfaError::Internal(e.to_string()))?;
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    if !totp::verify(&secret, req.code.trim(), now) {
        return Err(MfaError::BadRequest("code did not verify".into()));
    }

    mfa::confirm(&state.pool, user_id)
        .await
        .map_err(MfaError::from)?;

    // Fresh recovery codes on confirm (replaces any prior set).
    let codes = mfa::generate_recovery_codes(10);
    mfa::replace_recovery_codes(&state.pool, user_id, &codes)
        .await
        .map_err(MfaError::from)?;

    tracing::info!(user = %user_id, "TOTP MFA confirmed + recovery codes issued");
    Ok(Json(VerifyResponse {
        confirmed: true,
        recovery_codes: codes,
    }))
}

/// `DELETE /mfa/totp`: disable MFA for the caller (drops the credential +
/// recovery codes). Requires the caller be the user themselves (already gated
/// by require_human_user — a principal can only delete their own).
async fn disable(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
) -> Result<Json<Value>, MfaError> {
    let (user_id, _) = require_human_user(&state, &auth).await?;
    mfa::remove(&state.pool, user_id)
        .await
        .map_err(MfaError::from)?;
    tracing::info!(user = %user_id, "MFA disabled");
    Ok(Json(json!({ "disabled": true })))
}

/// The issuer label for the otpauth:// URI — the host part of the AS issuer, so
/// authenticator entries read "hive (nate)" rather than a full URL.
fn mfa_issuer(state: &AppState) -> String {
    let issuer = state.auth.config().issuer.clone();
    issuer
        .strip_prefix("https://")
        .or_else(|| issuer.strip_prefix("http://"))
        .unwrap_or(&issuer)
        .split(['/', ':'])
        .next()
        .unwrap_or("hive")
        .to_string()
}

enum MfaError {
    Unauthorized,
    Forbidden(String),
    BadRequest(String),
    Conflict(String),
    Internal(String),
}

impl From<crate::auth::store::StoreError> for MfaError {
    fn from(e: crate::auth::store::StoreError) -> Self {
        MfaError::Internal(e.to_string())
    }
}

impl axum::response::IntoResponse for MfaError {
    fn into_response(self) -> axum::response::Response {
        let (status, msg) = match self {
            MfaError::Unauthorized => (StatusCode::UNAUTHORIZED, "authentication required".into()),
            MfaError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            MfaError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            MfaError::Conflict(m) => (StatusCode::CONFLICT, m),
            MfaError::Internal(m) => {
                tracing::error!(error = %m, "mfa route internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}
