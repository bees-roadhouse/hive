// OAuth 2.1 AS endpoints + OIDC login (server.ts OAuth/OIDC sections).
// Owned by the OAuth workstream.

use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::{anyhow, bail, Result as AnyResult};
use axum::body::Bytes;
use axum::extract::rejection::FormRejection;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use axum::{Extension, Form, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hive_shared::{ActorKind, AiIdentity, OAuthConsentContext};
use jsonwebtoken::{Algorithm, DecodingKey};
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::auth::{csrf_for, token_hash, verify_pkce, SESSION_COOKIE, SESSION_TTL_SECS};
use crate::error::{err, ApiResult};
use crate::middleware::{cookie_value, issuer_for, AuthCtx};
use crate::store::oauth::{AuthCodeGrant, RedeemOutcome};
use crate::store::users::NewUser;
use crate::store::Store;

/// DCR is unauthenticated; cap to bound abuse (server.ts MAX_OAUTH_CLIENTS).
const MAX_OAUTH_CLIENTS: i64 = 200;

const OIDC_STATE_COOKIE: &str = "hive_oidc_state";
const OIDC_NONCE_COOKIE: &str = "hive_oidc_nonce";
const OIDC_RETURN_COOKIE: &str = "hive_oidc_return";

pub fn router() -> Router<Store> {
    Router::new()
        .route(
            "/.well-known/oauth-authorization-server",
            get(authorization_server_metadata),
        )
        .route(
            "/.well-known/oauth-protected-resource",
            get(protected_resource_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/authorize", get(authorize))
        .route("/oauth/authorize/context", get(consent_context))
        .route("/oauth/authorize/grant", post(consent_grant))
        .route("/oauth/token", post(token))
        .route("/api/auth/oidc/start", get(oidc_start))
        .route("/api/auth/oidc/callback", get(oidc_callback))
}

fn host_of(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok())
}

/// 302 redirect (Hono's `c.redirect` default status).
fn redirect(location: &str) -> AnyResult<Response> {
    let mut res = StatusCode::FOUND.into_response();
    res.headers_mut()
        .insert(header::LOCATION, HeaderValue::from_str(location)?);
    Ok(res)
}

/// Node's `htmlError` page — 400 text/html with the same body shape.
fn html_error(msg: &str) -> Response {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Authorization error</title>\n<body style=\"font-family:system-ui;background:#0e1116;color:#e6e9ee;padding:3rem\">\n<h1>🐝 Authorization error</h1><p>{msg}</p></body>"
    );
    (StatusCode::BAD_REQUEST, Html(body)).into_response()
}

/// `key=value&key=value` with percent-encoded values (URLSearchParams shape).
fn query_string(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

// ---- Discovery metadata (RFC 8414 + RFC 9728) ----

async fn authorization_server_metadata(State(s): State<Store>, headers: HeaderMap) -> ApiResult {
    let iss = issuer_for(&s, host_of(&headers)).await;
    Ok(Json(json!({
        "issuer": iss,
        "authorization_endpoint": format!("{iss}/authorize"),
        "token_endpoint": format!("{iss}/oauth/token"),
        "registration_endpoint": format!("{iss}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["mcp"],
    }))
    .into_response())
}

async fn protected_resource_metadata(State(s): State<Store>, headers: HeaderMap) -> ApiResult {
    let iss = issuer_for(&s, host_of(&headers)).await;
    Ok(Json(json!({
        "resource": format!("{iss}/mcp"),
        "authorization_servers": [iss],
        "bearer_methods_supported": ["header"],
        "scopes_supported": ["mcp"],
    }))
    .into_response())
}

// ---- Dynamic Client Registration (RFC 7591) ----

/// http/https URL with no (non-empty) fragment — JS `new URL(u)` + `!url.hash`.
fn valid_redirect(u: &str) -> bool {
    match reqwest::Url::parse(u) {
        Ok(url) => {
            (url.scheme() == "https" || url.scheme() == "http")
                && !matches!(url.fragment(), Some(f) if !f.is_empty())
        }
        Err(_) => false,
    }
}

async fn register(State(s): State<Store>, body: Bytes) -> ApiResult {
    if s.oauth_clients_count().await? >= MAX_OAUTH_CLIENTS {
        return Ok(err(StatusCode::TOO_MANY_REQUESTS, "too_many_clients"));
    }
    // Node: `c.req.json().catch(() => ({}))` — malformed JSON acts like `{}`.
    let body: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let mut redirect_uris: Vec<String> = Vec::new();
    if let Some(arr) = body.get("redirect_uris").and_then(Value::as_array) {
        for v in arr {
            match v.as_str() {
                Some(u) if valid_redirect(u) => redirect_uris.push(u.to_string()),
                _ => return Ok(err(StatusCode::BAD_REQUEST, "invalid_redirect_uri")),
            }
        }
    }
    if redirect_uris.is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "invalid_redirect_uri"));
    }
    let client_name: String = body
        .get("client_name")
        .and_then(Value::as_str)
        .unwrap_or("MCP client")
        .chars()
        .take(200)
        .collect();
    let client = s
        .oauth_clients_register(&client_name, &redirect_uris)
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "client_id": client.client_id,
            "client_name": client.client_name,
            "redirect_uris": client.redirect_uris,
            "grant_types": client.grant_types,
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        })),
    )
        .into_response())
}

// ---- Authorization endpoint (browser entry) ----
// Validates, then hands off to the SPA consent screen. Never redirects on a
// bad client/redirect_uri.

async fn authorize(State(s): State<Store>, Query(q): Query<HashMap<String, String>>) -> ApiResult {
    let client = match q.get("client_id") {
        Some(id) => s.oauth_clients_get(id).await?,
        None => None,
    };
    let Some(client) = client else {
        return Ok(html_error("Unknown client_id."));
    };
    let redirect_uri = q.get("redirect_uri").map(String::as_str).unwrap_or("");
    if redirect_uri.is_empty() || !client.redirect_uris.iter().any(|u| u == redirect_uri) {
        return Ok(html_error("redirect_uri does not match a registered URI."));
    }
    // Past this point a bad request MAY be redirected back to the (validated) client.
    let state = q.get("state").map(String::as_str).unwrap_or("");
    let back = |e: &str| -> AnyResult<Response> {
        let st = if state.is_empty() {
            String::new()
        } else {
            format!("&state={}", urlencoding::encode(state))
        };
        redirect(&format!("{redirect_uri}?error={e}{st}"))
    };
    if q.get("response_type").map(String::as_str) != Some("code") {
        return Ok(back("unsupported_response_type")?);
    }
    let code_challenge = q.get("code_challenge").map(String::as_str).unwrap_or("");
    if q.get("code_challenge_method").map(String::as_str) != Some("S256")
        || code_challenge.is_empty()
    {
        return Ok(back("invalid_request")?);
    }
    // Hand to the SPA consent route (same origin). The session check happens
    // there (the /oauth/authorize/context call requires a logged-in human).
    let scope = q.get("scope").map(String::as_str).unwrap_or("mcp");
    let client_id = q.get("client_id").map(String::as_str).unwrap_or("");
    let qs = query_string(&[
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code_challenge", code_challenge),
        ("state", state),
        ("scope", scope),
    ]);
    Ok(redirect(&format!("/consent?{qs}"))?)
}

// ---- Consent context: who's asking + which AI identities may be granted ----

#[derive(Deserialize)]
struct ContextQuery {
    client_id: Option<String>,
}

async fn consent_context(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<ContextQuery>,
) -> ApiResult {
    if ctx.principal != Some("session") {
        return Ok(err(StatusCode::UNAUTHORIZED, "session_required"));
    }
    let Some(client) = s
        .oauth_clients_get(q.client_id.as_deref().unwrap_or(""))
        .await?
    else {
        return Ok(err(StatusCode::NOT_FOUND, "unknown_client"));
    };
    let identities = s
        .people_ais_owned_by(ctx.actor())
        .await?
        .into_iter()
        .map(|p| AiIdentity {
            slug: p.slug,
            name: p.name,
        })
        .collect();
    let cookie = ctx.session_cookie.clone().unwrap_or_default();
    Ok(Json(OAuthConsentContext {
        client_name: client.client_name,
        identities,
        csrf: csrf_for(&cookie),
    })
    .into_response())
}

// ---- Consent grant: issue an auth code bound to the chosen AI identity ----

#[derive(Deserialize)]
struct GrantBody {
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    ai_actor: Option<String>,
    csrf: Option<String>,
}

async fn consent_grant(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    headers: HeaderMap,
    Json(body): Json<GrantBody>,
) -> ApiResult {
    if ctx.principal != Some("session") {
        return Ok(err(StatusCode::UNAUTHORIZED, "session_required"));
    }
    // CSRF: per-session token + same-origin Origin check (not SameSite alone).
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if origin != issuer_for(&s, host_of(&headers)).await {
            return Ok(err(StatusCode::FORBIDDEN, "bad_origin"));
        }
    }
    let cookie = ctx.session_cookie.clone().unwrap_or_default();
    let csrf = body.csrf.unwrap_or_default();
    if csrf.is_empty() || csrf != csrf_for(&cookie) {
        return Ok(err(StatusCode::FORBIDDEN, "bad_csrf"));
    }
    let client_id = body.client_id.unwrap_or_default();
    let redirect_uri = body.redirect_uri.unwrap_or_default();
    let valid = s
        .oauth_clients_get(&client_id)
        .await?
        .map(|c| c.redirect_uris.iter().any(|u| u == &redirect_uri))
        .unwrap_or(false);
    if !valid {
        return Ok(err(StatusCode::BAD_REQUEST, "invalid_client"));
    }
    let owner = ctx.actor().to_string();
    let ai_actor = body.ai_actor.unwrap_or_default();
    let owned = s
        .people_ais_owned_by(&owner)
        .await?
        .iter()
        .any(|p| p.slug == ai_actor);
    if !owned {
        return Ok(err(StatusCode::FORBIDDEN, "not_your_identity"));
    }
    let code = s
        .oauth_codes_create(&AuthCodeGrant {
            client_id,
            redirect_uri: redirect_uri.clone(),
            code_challenge: body.code_challenge.unwrap_or_default(),
            ai_actor,
            granted_by: owner,
            scope: body.scope.unwrap_or_else(|| "mcp".to_string()),
        })
        .await?;
    let state = body.state.unwrap_or_default();
    let st = if state.is_empty() {
        String::new()
    } else {
        format!("&state={}", urlencoding::encode(&state))
    };
    let redirect = format!("{redirect_uri}?code={}{st}", urlencoding::encode(&code));
    Ok(Json(json!({ "redirect": redirect })).into_response())
}

// ---- Token endpoint: exchange code (+PKCE) for a long-lived AI token ----

#[derive(Deserialize, Default)]
struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    code_verifier: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
}

async fn token(State(s): State<Store>, form: Result<Form<TokenForm>, FormRejection>) -> ApiResult {
    // Node: `c.req.parseBody().catch(() => ({}))` — unparseable body acts empty.
    let form = form.map(|Form(f)| f).unwrap_or_default();
    if form.grant_type.as_deref() != Some("authorization_code") {
        return Ok(err(StatusCode::BAD_REQUEST, "unsupported_grant_type"));
    }
    let code = form.code.unwrap_or_default();
    let verifier = form.code_verifier.unwrap_or_default();
    let redirect_uri = form.redirect_uri.unwrap_or_default();
    let client_id = form.client_id.unwrap_or_default();
    if code.is_empty() || verifier.is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "invalid_request"));
    }

    let grant = match s.oauth_codes_redeem(&code).await? {
        RedeemOutcome::Ok(g) => g,
        RedeemOutcome::Replay => {
            // Code reuse — treat as compromise: revoke any token already
            // minted for this client.
            s.tokens_revoke_by_client(&client_id).await?;
            return Ok(err(StatusCode::BAD_REQUEST, "invalid_grant"));
        }
        RedeemOutcome::Expired | RedeemOutcome::Unknown => {
            return Ok(err(StatusCode::BAD_REQUEST, "invalid_grant"));
        }
    };
    if grant.client_id != client_id || grant.redirect_uri != redirect_uri {
        return Ok(err(StatusCode::BAD_REQUEST, "invalid_grant"));
    }
    if !verify_pkce(&verifier, &grant.code_challenge) {
        return Ok(err(StatusCode::BAD_REQUEST, "invalid_grant"));
    }

    let (token, _record) = s
        .tokens_create_oauth(
            &grant.ai_actor,
            &grant.client_id,
            &grant.granted_by,
            &grant.scope,
        )
        .await?;
    Ok(Json(json!({
        "access_token": token,
        "token_type": "Bearer",
        "scope": grant.scope,
        "expires_in": 31_536_000,
    }))
    .into_response())
}

// ============================================================================
// OIDC human login (dormant unless OIDC_ISSUER is configured)
// ============================================================================

struct OidcConfig {
    issuer: String,
    client_id: String,
    client_secret: String,
    redirect_uri: String,
    allowed_domains: Vec<String>,
}

/// Read OIDC config from env, or None when unconfigured (feature off).
fn oidc_config() -> Option<OidcConfig> {
    let issuer = std::env::var("OIDC_ISSUER")
        .ok()
        .filter(|v| !v.is_empty())?;
    Some(OidcConfig {
        issuer: issuer.strip_suffix('/').unwrap_or(&issuer).to_string(),
        client_id: std::env::var("OIDC_CLIENT_ID").unwrap_or_default(),
        client_secret: std::env::var("OIDC_CLIENT_SECRET").unwrap_or_default(),
        redirect_uri: std::env::var("OIDC_REDIRECT_URI").unwrap_or_default(),
        allowed_domains: std::env::var("OIDC_ALLOWED_DOMAINS")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
    })
}

#[derive(Clone, Deserialize)]
struct Discovery {
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
}

static DISCOVERY: OnceLock<Discovery> = OnceLock::new();

async fn discover(cfg: &OidcConfig) -> AnyResult<Discovery> {
    if let Some(d) = DISCOVERY.get() {
        return Ok(d.clone());
    }
    let res = reqwest::get(format!("{}/.well-known/openid-configuration", cfg.issuer)).await?;
    if !res.status().is_success() {
        bail!("oidc discovery failed: {}", res.status().as_u16());
    }
    let d: Discovery = res.json().await?;
    let _ = DISCOVERY.set(d.clone());
    Ok(d)
}

/// 64-char hex from fresh randomness — Node's `tokenHash(Date.now():random)`.
fn random_hash() -> String {
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    token_hash(&hex::encode(bytes))
}

fn oidc_cookie(name: &str, value: &str) -> AnyResult<HeaderValue> {
    Ok(HeaderValue::from_str(&format!(
        "{name}={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600",
        urlencoding::encode(value)
    ))?)
}

#[derive(Deserialize)]
struct OidcStartQuery {
    return_to: Option<String>,
}

async fn oidc_start(Query(q): Query<OidcStartQuery>) -> ApiResult {
    let Some(cfg) = oidc_config() else {
        return Ok(err(StatusCode::NOT_FOUND, "oidc_not_configured"));
    };
    let disco = discover(&cfg).await?;
    let state = random_hash();
    let nonce = random_hash();
    let qs = query_string(&[
        ("response_type", "code"),
        ("client_id", &cfg.client_id),
        ("redirect_uri", &cfg.redirect_uri),
        ("scope", "openid email profile"),
        ("state", &state),
        ("nonce", &nonce),
    ]);
    let mut res = redirect(&format!("{}?{qs}", disco.authorization_endpoint))?;
    let headers = res.headers_mut();
    headers.append(header::SET_COOKIE, oidc_cookie(OIDC_STATE_COOKIE, &state)?);
    headers.append(header::SET_COOKIE, oidc_cookie(OIDC_NONCE_COOKIE, &nonce)?);
    if let Some(rt) = q.return_to.filter(|v| !v.is_empty()) {
        headers.append(header::SET_COOKIE, oidc_cookie(OIDC_RETURN_COOKIE, &rt)?);
    }
    Ok(res)
}

async fn oidc_callback(
    State(s): State<Store>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let Some(cfg) = oidc_config() else {
        return Ok(err(StatusCode::NOT_FOUND, "oidc_not_configured"));
    };
    let code = q.get("code").map(String::as_str).unwrap_or("");
    let state = q.get("state").map(String::as_str).unwrap_or("");
    let state_cookie = cookie_value(&headers, OIDC_STATE_COOKIE).unwrap_or_default();
    if code.is_empty() || state.is_empty() || state != state_cookie {
        return Ok(html_error("Invalid OIDC state."));
    }
    let nonce = cookie_value(&headers, OIDC_NONCE_COOKIE).unwrap_or_default();
    match oidc_sign_in(&s, &cfg, code, &nonce, &headers).await {
        Ok(res) => Ok(res),
        Err(e) => Ok(html_error(&format!("OIDC sign-in failed: {e}"))),
    }
}

/// The body of server.ts's callback try-block; any Err becomes the
/// "OIDC sign-in failed: …" page.
async fn oidc_sign_in(
    s: &Store,
    cfg: &OidcConfig,
    code: &str,
    nonce: &str,
    headers: &HeaderMap,
) -> AnyResult<Response> {
    let disco = discover(cfg).await?;
    let id_token = exchange_code(cfg, &disco.token_endpoint, code).await?;
    let claims = verify_id_token(cfg, &disco.jwks_uri, &id_token, nonce).await?;
    let email = claims.email.to_lowercase();
    let mut user = s.users_by_email(&email).await?.map(|(u, _hash)| u);
    if user.is_none() {
        let domain = email.split('@').nth(1).unwrap_or("").to_string();
        if !cfg.allowed_domains.contains(&domain) {
            return Ok(html_error(
                "No hive account for this email, and its domain isn't allowed.",
            ));
        }
        let safe = s
            .users_create(
                NewUser {
                    name: claims.name.clone().unwrap_or_else(|| email.clone()),
                    email: email.clone(),
                    password: token_hash(&random_hash()),
                    role: None,
                    actor: None,
                    kind: Some(ActorKind::Human),
                },
                "oidc",
            )
            .await?;
        user = s.users_by_id(&safe.id).await?;
    }
    let Some(user) = user else {
        return Ok(html_error("Could not provision an account."));
    };
    let session = s.sessions_create(&user.id).await?;
    let back = cookie_value(headers, OIDC_RETURN_COOKIE)
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "/".to_string());
    let mut res = redirect(&back)?;
    let h = res.headers_mut();
    h.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{SESSION_COOKIE}={session}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_SECS}"
        ))?,
    );
    h.append(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!("{OIDC_RETURN_COOKIE}=; Path=/; Max-Age=0"))?,
    );
    Ok(res)
}

async fn exchange_code(cfg: &OidcConfig, token_endpoint: &str, code: &str) -> AnyResult<String> {
    let res = reqwest::Client::new()
        .post(token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", cfg.redirect_uri.as_str()),
            ("client_id", cfg.client_id.as_str()),
            ("client_secret", cfg.client_secret.as_str()),
        ])
        .send()
        .await?;
    if !res.status().is_success() {
        bail!("oidc token exchange failed: {}", res.status().as_u16());
    }
    let v: Value = res.json().await?;
    v.get("id_token")
        .and_then(Value::as_str)
        .map(String::from)
        .ok_or_else(|| anyhow!("malformed id_token"))
}

struct IdClaims {
    email: String,
    name: Option<String>,
}

fn decode_seg(seg: &str) -> AnyResult<Value> {
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(seg)?)?)
}

/// Verify an ID token's signature (RS256 via JWKS) and iss/aud/exp/nonce —
/// parity with oidc.ts verifyIdToken (including its error strings).
async fn verify_id_token(
    cfg: &OidcConfig,
    jwks_uri: &str,
    id_token: &str,
    nonce: &str,
) -> AnyResult<IdClaims> {
    let mut segs = id_token.split('.');
    let (Some(h), Some(p), Some(sig)) = (segs.next(), segs.next(), segs.next()) else {
        bail!("malformed id_token");
    };
    if h.is_empty() || p.is_empty() || sig.is_empty() {
        bail!("malformed id_token");
    }
    let header = decode_seg(h)?;
    let payload = decode_seg(p)?;

    let jwks: Value = reqwest::get(jwks_uri).await?.json().await?;
    let empty = Vec::new();
    let keys = jwks.get("keys").and_then(Value::as_array).unwrap_or(&empty);
    let kid = header.get("kid").and_then(Value::as_str);
    let jwk = keys
        .iter()
        .find(|k| k.get("kid").and_then(Value::as_str) == kid)
        .or_else(|| keys.first())
        .ok_or_else(|| anyhow!("no jwks key"))?;
    let n = jwk
        .get("n")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("no jwks key"))?;
    let e = jwk
        .get("e")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("no jwks key"))?;
    let key = DecodingKey::from_rsa_components(n, e)?;
    let message = format!("{h}.{p}");
    if !jsonwebtoken::crypto::verify(sig, message.as_bytes(), &key, Algorithm::RS256)? {
        bail!("bad id_token signature");
    }

    let iss = payload.get("iss").and_then(Value::as_str).unwrap_or("");
    if iss.strip_suffix('/').unwrap_or(iss) != cfg.issuer {
        bail!("iss mismatch");
    }
    let aud_ok = match payload.get("aud") {
        Some(Value::Array(a)) => a.iter().any(|v| v.as_str() == Some(cfg.client_id.as_str())),
        Some(Value::String(a)) => *a == cfg.client_id,
        _ => false,
    };
    if !aud_ok {
        bail!("aud mismatch");
    }
    // Node: `Number(payload.exp) * 1000 < Date.now()` — a missing exp is NaN,
    // which never compares true, so absence passes (parity, not preference).
    if let Some(exp) = payload.get("exp").and_then(Value::as_f64) {
        if exp * 1000.0 < chrono::Utc::now().timestamp_millis() as f64 {
            bail!("id_token expired");
        }
    }
    if payload.get("nonce").and_then(Value::as_str) != Some(nonce) {
        bail!("nonce mismatch");
    }
    let email = payload
        .get("email")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("no email in id_token"))?;
    Ok(IdClaims {
        email: email.to_string(),
        name: payload
            .get("name")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_validation_matches_node() {
        assert!(valid_redirect("https://example.com/cb"));
        assert!(valid_redirect("http://localhost:3000/cb"));
        assert!(!valid_redirect("ftp://example.com/cb"));
        assert!(!valid_redirect("not a url"));
        assert!(!valid_redirect("https://example.com/cb#frag"));
        // JS `!url.hash` treats a bare trailing '#' as no fragment.
        assert!(valid_redirect("https://example.com/cb#"));
    }

    #[test]
    fn html_error_is_400_html() {
        let res = html_error("Unknown client_id.");
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let ct = res
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(ct.starts_with("text/html"));
    }

    #[test]
    fn query_string_encodes_values() {
        let qs = query_string(&[("scope", "openid email profile"), ("state", "a&b")]);
        assert_eq!(qs, "scope=openid%20email%20profile&state=a%26b");
    }
}
