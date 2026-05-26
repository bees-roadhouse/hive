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
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::extractor::MaybeAuthUser;
use crate::auth::tokens::{self, AccessTokenParams};
use crate::auth::{device, mfa, password, policy::AuthPolicy, store};
use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/authorize", post(authorize))
        .route("/token", post(token))
        // RFC 8628 device grant (Phase 5): start the flow (public) + the
        // verification surface (authenticated human approves/denies).
        .route("/device_authorization", post(device_authorization))
        .route("/device", get(device_lookup))
        .route("/device/approve", post(device_approve))
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
    /// Second factor (§4): a 6-digit TOTP code. Present on the second leg of the
    /// two-step login, after a first leg returned `mfa_required`.
    #[serde(default)]
    mfa_code: Option<String>,
    /// Alternatively, a single-use recovery code in place of the TOTP.
    #[serde(default)]
    recovery_code: Option<String>,
}

fn default_challenge_method() -> String {
    "S256".to_string()
}

/// `/authorize` returns either the authorization code (auth complete) or — when
/// the user has MFA and didn't present a valid second factor — an
/// `mfa_required` challenge telling the client to re-submit with a code (§4).
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum AuthorizeResponse {
    Code {
        code: String,
        /// Echoed so the (future browser) client can match its redirect.
        redirect_uri: String,
    },
    MfaRequired {
        mfa_required: bool,
        methods: Vec<&'static str>,
    },
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

    // Second factor (§4): the password is leg one. If the policy enforces
    // internal MFA AND this user has a CONFIRMED TOTP credential, require a
    // valid second factor before issuing the code. Returns the resolved amr
    // (["pwd"] or ["pwd","otp"]). The two-step is: no code → mfa_required
    // challenge; valid code → proceed.
    let amr = match resolve_second_factor(&state, &user, &req).await? {
        SecondFactor::Proceed(amr) => amr,
        SecondFactor::Required => {
            return Ok(Json(AuthorizeResponse::MfaRequired {
                mfa_required: true,
                methods: vec!["totp", "recovery_code"],
            }));
        }
    };

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
            amr: &amr,
        },
    )
    .await
    .map_err(OAuthError::server)?;

    Ok(Json(AuthorizeResponse::Code {
        code,
        redirect_uri: req.redirect_uri,
    }))
}

/// Outcome of the second-factor decision.
enum SecondFactor {
    /// MFA not required (or just satisfied); proceed with these auth methods.
    Proceed(Vec<String>),
    /// MFA required but no valid factor presented yet — challenge the client.
    Required,
}

/// The login state machine's MFA branch (§4), kept in one place so the decision
/// isn't scattered. Returns Proceed(["pwd"]) when MFA doesn't apply,
/// Proceed(["pwd","otp"]) when a valid second factor was presented, or Required
/// when the user must supply one. A *wrong* factor is an error (access_denied),
/// rate-limited + lockout-aware.
async fn resolve_second_factor(
    state: &AppState,
    user: &store::User,
    req: &AuthorizeRequest,
) -> Result<SecondFactor, OAuthError> {
    let pol = AuthPolicy::load(&state.pool)
        .await
        .map_err(OAuthError::server)?;

    // delegated (IdP owns MFA) / off → hive doesn't prompt. One branch.
    if !pol.mfa_mode.enforces_internal() {
        if matches!(pol.mfa_mode, crate::auth::policy::MfaMode::Off) {
            tracing::warn!(user = %user.username, "HIVE_MFA_MODE=off — second factor skipped");
        }
        return Ok(SecondFactor::Proceed(vec!["pwd".to_string()]));
    }

    let cred = mfa::get_credential(&state.pool, user.id)
        .await
        .map_err(OAuthError::server)?;
    let Some(cred) = cred.filter(|c| c.is_confirmed()) else {
        // No confirmed credential => MFA doesn't gate this user (yet).
        return Ok(SecondFactor::Proceed(vec!["pwd".to_string()]));
    };

    // A factor must be presented. None yet → challenge.
    let presented_totp = req
        .mfa_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let presented_recovery = req
        .recovery_code
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if presented_totp.is_none() && presented_recovery.is_none() {
        return Ok(SecondFactor::Required);
    }

    // Locked out from too many failures?
    if cred.is_locked() {
        return Err(OAuthError::access_denied(
            "too many failed codes; try again later",
        ));
    }

    // Recovery code path (single-use), else TOTP.
    if let Some(rc) = presented_recovery {
        let ok = mfa::redeem_recovery_code(&state.pool, user.id, rc)
            .await
            .map_err(OAuthError::server)?;
        if ok {
            let _ = mfa::record_success(&state.pool, user.id).await;
            return Ok(SecondFactor::Proceed(vec![
                "pwd".to_string(),
                "otp".to_string(),
            ]));
        }
        let _ = mfa::record_failure(&state.pool, user.id).await;
        return Err(OAuthError::access_denied("invalid recovery code"));
    }

    let code = presented_totp.unwrap();
    let secret = crate::auth::totp::decrypt_secret(&cred.secret_enc)
        .map_err(|e| OAuthError::Server(e.to_string()))?;
    let now = chrono::Utc::now().timestamp().max(0) as u64;
    if crate::auth::totp::verify(&secret, code, now) {
        let _ = mfa::record_success(&state.pool, user.id).await;
        Ok(SecondFactor::Proceed(vec![
            "pwd".to_string(),
            "otp".to_string(),
        ]))
    } else {
        let locked = mfa::record_failure(&state.pool, user.id)
            .await
            .map_err(OAuthError::server)?;
        if locked {
            Err(OAuthError::access_denied(
                "invalid code; account temporarily locked",
            ))
        } else {
            Err(OAuthError::access_denied("invalid code"))
        }
    }
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
    // device_code grant (RFC 8628):
    #[serde(default)]
    device_code: Option<String>,
}

/// The device-code grant type URN (RFC 8628).
const DEVICE_CODE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

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
        DEVICE_CODE_GRANT => token_device_code(&state, &req, &pol).await,
        other => Err(OAuthError::unsupported_grant(other)),
    }
}

/// Issue a human session + access/refresh tokens for an authenticated user.
/// Shared by the auth-code and device-code grants (both end here once the user
/// is established): create the session (recording amr), mint the access JWT.
async fn issue_session_tokens(
    state: &AppState,
    pol: &AuthPolicy,
    user_id: uuid::Uuid,
    client_id: &str,
    scopes: &[String],
    amr: &[String],
) -> Result<Json<TokenResponse>, OAuthError> {
    let (is_admin, visibility) = user_authz(state, user_id).await?;
    let session_secs = pol.effective_session_secs(None);
    let issued = store::create_session(&state.pool, user_id, client_id, scopes, amr, session_secs)
        .await
        .map_err(OAuthError::server)?;
    let access = mint_for_session(
        state,
        &user_id.to_string(),
        scopes,
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
        scope: scopes.join(" "),
    }))
}

/// `/token` device-code grant (RFC 8628 §3.4-3.5). Polls a device authorization
/// by its device_code and maps the lifecycle to the standard OAuth errors;
/// `authorization_pending` / `slow_down` / `expired_token` / `access_denied`.
/// On approval, issues tokens for the approving user (reusing the session mint)
/// and consumes the device row so the code can't be replayed.
async fn token_device_code(
    state: &AppState,
    req: &TokenRequest,
    pol: &AuthPolicy,
) -> Result<Json<TokenResponse>, OAuthError> {
    let device_code = req
        .device_code
        .as_deref()
        .ok_or_else(|| OAuthError::invalid_request("device_code required"))?;

    use crate::auth::device::{self, PollOutcome};
    match device::poll(&state.pool, device_code)
        .await
        .map_err(OAuthError::server)?
    {
        PollOutcome::AuthorizationPending => Err(OAuthError::authorization_pending()),
        PollOutcome::SlowDown => Err(OAuthError::slow_down()),
        PollOutcome::ExpiredToken => Err(OAuthError::expired_token()),
        PollOutcome::AccessDenied => Err(OAuthError::access_denied("authorization denied")),
        PollOutcome::Unknown => Err(OAuthError::invalid_grant("unknown device_code")),
        PollOutcome::Approved {
            device_id,
            user_id,
            client_id,
            scopes,
            amr,
        } => {
            let resp = issue_session_tokens(state, pol, user_id, &client_id, &scopes, &amr).await?;
            // Single redemption: consume the device row after issuing.
            device::consume(&state.pool, device_id)
                .await
                .map_err(OAuthError::server)?;
            Ok(resp)
        }
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

    // amr was decided at /authorize (pwd, or pwd+otp if MFA was required) and
    // carried on the auth code; the shared mint records it on the session (§4).
    issue_session_tokens(
        state,
        pol,
        row.user_id,
        &row.client_id,
        &row.scopes,
        &row.amr,
    )
    .await
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

// ---------- device authorization grant (RFC 8628, §3.2) ----------

#[derive(Debug, Deserialize)]
struct DeviceAuthRequest {
    #[serde(default = "default_device_client")]
    client_id: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    resource: Option<String>,
}

fn default_device_client() -> String {
    "hive-cli".to_string()
}

#[derive(Debug, Serialize)]
struct DeviceAuthResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: String,
    expires_in: i64,
    interval: i32,
}

/// `POST /device_authorization` (RFC 8628 §3.1): a public client (the CLI)
/// starts the flow. Returns the device_code (polled at /token) + the human
/// user_code + where to approve it. No auth required — approval is the gate.
async fn device_authorization(
    State(state): State<AppState>,
    Json(req): Json<DeviceAuthRequest>,
) -> Result<Json<DeviceAuthResponse>, OAuthError> {
    let scopes: Vec<String> = req
        .scope
        .as_deref()
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();

    let created = device::create(
        &state.pool,
        &req.client_id,
        &scopes,
        req.resource.as_deref(),
    )
    .await
    .map_err(OAuthError::server)?;

    let issuer = state.auth.config().issuer.trim_end_matches('/').to_string();
    let verification_uri = format!("{issuer}/device");
    let verification_uri_complete = format!("{verification_uri}?user_code={}", created.user_code);

    Ok(Json(DeviceAuthResponse {
        device_code: created.device_code,
        user_code: created.user_code,
        verification_uri,
        verification_uri_complete,
        expires_in: created.expires_in,
        interval: created.interval_secs,
    }))
}

#[derive(Debug, Deserialize)]
struct DeviceLookupQuery {
    user_code: String,
}

/// `GET /device?user_code=...` : the verification surface looks up a pending
/// device by its user_code so the approval UI can show what's being authorized.
/// (The browser login/consent chrome is hive-ui's; this returns the facts.)
async fn device_lookup(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<DeviceLookupQuery>,
) -> Result<Json<Value>, OAuthError> {
    let found = device::find_by_user_code(&state.pool, &q.user_code)
        .await
        .map_err(OAuthError::server)?
        .ok_or_else(|| OAuthError::invalid_grant("unknown user_code"))?;
    Ok(Json(json!({
        "client_id": found.client_id,
        "scopes": found.scopes,
        "status": found.status,
        "expired": found.expired,
    })))
}

#[derive(Debug, Deserialize)]
struct DeviceApproveRequest {
    user_code: String,
    /// true = approve, false = deny.
    #[serde(default = "default_true_approve")]
    approve: bool,
}

fn default_true_approve() -> bool {
    true
}

/// `POST /device/approve` (RFC 8628 §3.3): an AUTHENTICATED human approves (or
/// denies) a device by user_code, binding it to themselves. Their own session
/// already satisfied MFA, so the device inherits that assurance (amr carried
/// from the human's token). An AI/dev principal can't approve a device for a
/// human account.
async fn device_approve(
    State(state): State<AppState>,
    auth: MaybeAuthUser,
    Json(req): Json<DeviceApproveRequest>,
) -> Result<Json<Value>, OAuthError> {
    let principal = auth
        .0
        .ok_or_else(|| OAuthError::AccessDenied("authentication required to approve".into()))?;
    if principal.kind != crate::auth::claims::PrincipalType::Human {
        return Err(OAuthError::AccessDenied(
            "only a human account can approve a device".into(),
        ));
    }
    let user_id = principal
        .subject
        .parse::<uuid::Uuid>()
        .map_err(|_| OAuthError::AccessDenied("invalid principal".into()))?;

    if !req.approve {
        let denied = device::deny(&state.pool, &req.user_code)
            .await
            .map_err(OAuthError::server)?;
        if !denied {
            return Err(OAuthError::invalid_grant(
                "no pending device for that user_code",
            ));
        }
        return Ok(Json(json!({ "status": "denied" })));
    }

    // The device inherits the approving human's auth methods. The human reached
    // this authenticated endpoint, so they completed password (+MFA if enrolled);
    // record amr=["pwd"] (the device grant doesn't itself add a second factor —
    // the human's session is the assurance).
    let amr = vec!["pwd".to_string()];
    let approved = device::approve(&state.pool, &req.user_code, user_id, &amr)
        .await
        .map_err(OAuthError::server)?;
    if !approved {
        return Err(OAuthError::invalid_grant(
            "no pending device for that user_code (already used or expired?)",
        ));
    }
    Ok(Json(json!({ "status": "approved" })))
}

/// OAuth-style error response (RFC 6749 §5.2 shape; device-grant codes per
/// RFC 8628 §3.5).
enum OAuthError {
    InvalidRequest(String),
    InvalidGrant(String),
    UnsupportedGrantType(String),
    AccessDenied(String),
    /// RFC 8628: the user hasn't approved the device yet — client keeps polling.
    AuthorizationPending,
    /// RFC 8628: the client is polling faster than `interval` — back off.
    SlowDown,
    /// RFC 8628: the device_code expired — client stops.
    ExpiredToken,
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
    fn authorization_pending() -> Self {
        OAuthError::AuthorizationPending
    }
    fn slow_down() -> Self {
        OAuthError::SlowDown
    }
    fn expired_token() -> Self {
        OAuthError::ExpiredToken
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
            // RFC 8628 §3.5: these are HTTP 400 with the OAuth error code; the
            // polling client branches on the code, not the HTTP status.
            OAuthError::AuthorizationPending => (
                StatusCode::BAD_REQUEST,
                "authorization_pending",
                "the authorization request is still pending".to_string(),
            ),
            OAuthError::SlowDown => (
                StatusCode::BAD_REQUEST,
                "slow_down",
                "polling too frequently; increase the interval".to_string(),
            ),
            OAuthError::ExpiredToken => (
                StatusCode::BAD_REQUEST,
                "expired_token",
                "the device_code has expired".to_string(),
            ),
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
