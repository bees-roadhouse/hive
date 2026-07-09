// Onboarding + session auth + users + API tokens (server.ts auth section).

use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use hive_shared::{ActorKind, AuthMe, OnboardingPayload, UserRole};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{
    local_auth_enabled, oauth_never_expires_enabled, oidc_enabled, SESSION_COOKIE, SESSION_TTL_SECS,
};
use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::users::NewUser;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/onboarding/status", get(onboarding_status))
        .route("/api/onboarding", post(onboarding_complete))
        .route("/api/auth/login", post(login))
        .route("/api/auth/logout", post(logout))
        .route("/api/auth/me", get(me))
        .route("/api/auth/config", get(auth_config))
        .route("/api/users", get(users_list).post(users_create))
        .route("/api/tokens", get(tokens_list).post(tokens_create))
        .route("/api/tokens/{id}", delete(tokens_remove))
}

fn session_cookie_header(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={SESSION_TTL_SECS}"
    ))
    .expect("cookie value is header-safe")
}

fn clear_cookie_header() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0"
    ))
    .expect("static cookie header")
}

async fn onboarding_status(State(s): State<Store>) -> ApiResult {
    Ok(Json(s.onboarding_status().await?).into_response())
}

async fn onboarding_complete(State(s): State<Store>, body: Json<OnboardingPayload>) -> ApiResult {
    if !s.onboarding_required().await? {
        return Ok(err(StatusCode::CONFLICT, "already_completed"));
    }
    let Json(body) = body;
    if body.instance_name.trim().is_empty()
        || body.admin_name.trim().is_empty()
        || body.admin_email.trim().is_empty()
        || body.password.trim().is_empty()
    {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "instanceName, adminName, adminEmail, password required",
        ));
    }
    if body.password.len() < 8 {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "password must be at least 8 characters",
        ));
    }
    let (user, session) = s
        .onboarding_complete(
            &body.instance_name,
            &body.admin_name,
            &body.admin_email,
            &body.password,
        )
        .await?;
    let mut res = (StatusCode::CREATED, Json(json!({ "user": user }))).into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, session_cookie_header(&session));
    Ok(res)
}

#[derive(Deserialize)]
struct LoginBody {
    email: Option<String>,
    password: Option<String>,
}

async fn login(State(s): State<Store>, Json(body): Json<LoginBody>) -> ApiResult {
    if !local_auth_enabled() {
        return Ok(err(StatusCode::NOT_FOUND, "local_auth_disabled"));
    }
    let (Some(email), Some(password)) = (body.email, body.password) else {
        return Ok(err(StatusCode::BAD_REQUEST, "email and password required"));
    };
    let Some(user) = s.users_authenticate(&email, &password).await? else {
        return Ok(err(StatusCode::UNAUTHORIZED, "invalid credentials"));
    };
    let session = s.sessions_create(&user.id).await?;
    let safe = hive_shared::SafeUser {
        id: user.id,
        actor: user.actor,
        email: user.email,
        name: user.name,
        role: user.role,
    };
    let mut res = Json(json!({ "user": safe })).into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, session_cookie_header(&session));
    Ok(res)
}

async fn logout(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    if let Some(cookie) = &ctx.session_cookie {
        s.sessions_destroy(cookie).await?;
    }
    let mut res = Json(json!({"ok": true})).into_response();
    res.headers_mut()
        .insert(header::SET_COOKIE, clear_cookie_header());
    Ok(res)
}

async fn me(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    let user = match &ctx.actor {
        Some(a) => s.users_list().await?.into_iter().find(|u| &u.actor == a),
        None => None,
    };
    Ok(Json(AuthMe {
        user,
        principal: ctx.principal.map(String::from),
    })
    .into_response())
}

async fn auth_config(State(s): State<Store>) -> ApiResult {
    // OIDC is live only when enabled and minimally configured.
    let oidc = oidc_enabled()
        && std::env::var("OIDC_ISSUER")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        && std::env::var("OIDC_CLIENT_ID")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        && std::env::var("OIDC_REDIRECT_URI")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
    Ok(Json(hive_shared::AuthConfig {
        oidc,
        local_auth: local_auth_enabled(),
        oauth_never_expires: oauth_never_expires_enabled(),
        instance_name: s.config_get("instance.name").await?,
        mail_enabled: super::mail::mail_enabled(),
    })
    .into_response())
}

// ---- users (admin) ----

async fn users_list(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    Ok(Json(s.users_list().await?).into_response())
}

#[derive(Deserialize)]
struct UserCreateBody {
    name: Option<String>,
    email: Option<String>,
    password: Option<String>,
    role: Option<UserRole>,
    kind: Option<ActorKind>,
}

async fn users_create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(body): Json<UserCreateBody>,
) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    let (Some(name), Some(email), Some(password)) = (body.name, body.email, body.password) else {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "name, email, password required",
        ));
    };
    if name.trim().is_empty() || email.trim().is_empty() || password.trim().is_empty() {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "name, email, password required",
        ));
    }
    if password.len() < 8 {
        return Ok(err(
            StatusCode::BAD_REQUEST,
            "password must be at least 8 characters",
        ));
    }
    let user = s
        .users_create(
            NewUser {
                name,
                email,
                password,
                role: body.role,
                actor: None,
                kind: body.kind,
            },
            ctx.actor(),
        )
        .await?;
    Ok((StatusCode::CREATED, Json(user)).into_response())
}

// ---- API tokens (admin) ----

async fn tokens_list(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    Ok(Json(s.tokens_list().await?).into_response())
}

#[derive(Deserialize)]
struct TokenCreateBody {
    actor: Option<String>,
    label: Option<String>,
    #[serde(rename = "expiresInDays")]
    expires_in_days: Option<i64>,
    #[serde(rename = "neverExpires")]
    never_expires: Option<bool>,
}

async fn tokens_create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(body): Json<TokenCreateBody>,
) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    let (Some(actor), Some(label)) = (body.actor, body.label) else {
        return Ok(err(StatusCode::BAD_REQUEST, "actor and label required"));
    };
    if actor.trim().is_empty() || label.trim().is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "actor and label required"));
    }
    // The plaintext token is returned ONCE here and never again.
    let (token, record) = s
        .tokens_create(
            &actor,
            &label,
            body.expires_in_days,
            body.never_expires.unwrap_or(false),
            ctx.actor(),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "token": token, "record": record })),
    )
        .into_response())
}

async fn tokens_remove(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    if s.tokens_remove(&id).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok(not_found())
    }
}
