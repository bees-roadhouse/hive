// CORS + auth resolution + onboarding gate — parity with server.ts middleware.
// Identity comes from a session cookie (browser) or a Bearer API token; the
// x-hive-actor header is not honored. Non-public API paths are locked behind
// onboarding, then behind authentication. SPA asset paths are not gated (nginx
// served them ungated in the Node deployment); /mcp and /api/stream do their
// own auth so they can shape their 401s (www-authenticate, raw JSON).

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use hive_shared::UserRole;
use serde_json::json;

use crate::auth::SESSION_COOKIE;
use crate::store::Store;

#[derive(Clone, Debug, Default)]
pub struct AuthCtx {
    pub actor: Option<String>,
    /// "session" | "token"
    pub principal: Option<&'static str>,
    pub role: Option<UserRole>,
    /// The raw session cookie value (the OAuth CSRF token derives from it).
    pub session_cookie: Option<String>,
}

impl AuthCtx {
    /// The acting identity, Node's `actor(c)` — "anon" when unauthenticated.
    pub fn actor(&self) -> &str {
        self.actor.as_deref().unwrap_or("anon")
    }

    /// Session-principal admin (Node `requireAdmin`).
    pub fn is_admin(&self) -> bool {
        self.role == Some(UserRole::Admin)
    }
}

const PUBLIC_PATHS: &[&str] = &[
    "/api/healthz",
    "/api/onboarding/status",
    "/api/onboarding",
    "/api/auth/login",
    "/api/auth/me",
    "/api/auth/config",
    "/.well-known/oauth-authorization-server",
    "/.well-known/oauth-protected-resource",
    "/oauth/register",
    "/oauth/token",
    "/authorize",
    "/api/auth/oidc/start",
    "/api/auth/oidc/callback",
];

/// Paths the auth gate applies to (what Hono served in the Node deployment).
fn gated(path: &str) -> bool {
    path.starts_with("/api")
        || path.starts_with("/oauth")
        || path.starts_with("/.well-known")
        || path == "/authorize"
}

/// Self-authenticating endpoints (raw-server routes in Node).
fn self_authed(path: &str) -> bool {
    path == "/mcp" || path == "/api/stream"
}

pub fn cookie_value(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').map(str::trim).find_map(|part| {
        part.strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|v| {
                urlencoding::decode(v)
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| v.to_string())
            })
    })
}

pub async fn resolve_auth(store: &Store, headers: &axum::http::HeaderMap) -> AuthCtx {
    let mut ctx = AuthCtx::default();

    if let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
    {
        if let Ok(Some(actor)) = store.tokens_resolve(bearer).await {
            ctx.actor = Some(actor);
            ctx.principal = Some("token");
        }
    }

    let cookie = cookie_value(headers, SESSION_COOKIE);
    if ctx.actor.is_none() {
        if let Some(value) = &cookie {
            if let Ok(Some(user)) = store.sessions_resolve(value).await {
                ctx.actor = Some(user.actor);
                ctx.principal = Some("session");
                ctx.role = Some(user.role);
            }
        }
    }
    ctx.session_cookie = cookie;
    ctx
}

pub async fn auth_and_cors(State(store): State<Store>, mut req: Request, next: Next) -> Response {
    let origin = req.headers().get(header::ORIGIN).cloned();

    // Preflight short-circuits before auth (Node returns 204).
    if req.method() == Method::OPTIONS {
        let mut res = StatusCode::NO_CONTENT.into_response();
        apply_cors(&mut res, origin.as_ref());
        return res;
    }

    let path = req.uri().path().to_string();
    let ctx = resolve_auth(&store, req.headers()).await;
    let authed = ctx.actor.is_some();
    req.extensions_mut().insert(ctx);

    let mut res = if !gated(&path) || self_authed(&path) || PUBLIC_PATHS.contains(&path.as_str()) {
        next.run(req).await
    } else {
        // Before setup, everything non-public is locked until onboarding runs.
        match store.onboarding_required().await {
            Ok(true) => (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "onboarding_required"})),
            )
                .into_response(),
            _ if !authed => (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "unauthenticated"})),
            )
                .into_response(),
            _ => next.run(req).await,
        }
    };

    apply_cors(&mut res, origin.as_ref());
    res
}

/// Reflect the request Origin (credentials must flow, so "*" won't do).
fn apply_cors(res: &mut Response, origin: Option<&HeaderValue>) {
    let headers = res.headers_mut();
    if let Some(origin) = origin {
        headers.insert("access-control-allow-origin", origin.clone());
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        headers.insert(
            "access-control-allow-credentials",
            HeaderValue::from_static("true"),
        );
    }
    headers.insert(
        "access-control-allow-headers",
        // mcp-* headers: browser MCP clients send them on /mcp preflight.
        HeaderValue::from_static(
            "content-type, authorization, x-hive-actor, mcp-session-id, mcp-protocol-version",
        ),
    );
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET,POST,PATCH,DELETE,OPTIONS"),
    );
}

/// The public origin of this instance — all OAuth metadata URLs derive from it.
pub async fn issuer_for(store: &Store, host: Option<&str>) -> String {
    if let Ok(Some(url)) = store.config_get("instance.url").await {
        return url;
    }
    if let Ok(url) = std::env::var("HIVE_PUBLIC_URL") {
        return url;
    }
    let port = std::env::var("PORT").unwrap_or_else(|_| "7878".to_string());
    format!(
        "http://{}",
        host.map(String::from)
            .unwrap_or(format!("localhost:{port}"))
    )
}
