// CORS + auth resolution + onboarding gate — parity with server.ts middleware.
// Identity comes from a session cookie (browser) or a Bearer API token; the
// x-hive-actor header is not honored. Non-public API paths are locked behind
// onboarding, then behind authentication. SPA asset paths are not gated (nginx
// served them ungated in the Node deployment); /mcp and /api/stream do their
// own auth so they can shape their 401s (www-authenticate, raw JSON).

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
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
    /// The human user whose namespace this principal reads/writes in: the
    /// logged-in user for a session, or the token's granter for a Bearer token
    /// (so an AI sees the memory namespace of whoever it acts for). `role` is
    /// resolved from THIS user, so admin-bypass follows the granting human.
    pub namespace_user: Option<String>,
    /// The raw session cookie value (the OAuth CSRF token derives from it).
    pub session_cookie: Option<String>,
}

impl AuthCtx {
    /// The acting identity, Node's `actor(c)` — "anon" when unauthenticated.
    pub fn actor(&self) -> &str {
        self.actor.as_deref().unwrap_or("anon")
    }

    /// The human user whose namespace governs visibility (falls back to the
    /// acting identity when no distinct granter is known).
    pub fn namespace_user(&self) -> &str {
        self.namespace_user
            .as_deref()
            .unwrap_or_else(|| self.actor())
    }

    /// The owner to stamp on writes — the namespace user when authenticated,
    /// else None (a system/anon write lands in the global/continuous history).
    pub fn namespace_owner(&self) -> Option<&str> {
        self.namespace_user.as_deref()
    }

    /// Admin authority belongs to an admin session, or to a token acting as the
    /// same admin human. Delegated/AI tokens keep the grantor's namespace but do
    /// not inherit admin-wide visibility.
    pub fn is_admin(&self) -> bool {
        self.role == Some(UserRole::Admin)
            && (self.principal == Some("session")
                || self.actor.as_deref() == self.namespace_user.as_deref())
    }

    /// What this principal may see across the per-user memory namespaces.
    pub fn visibility(&self) -> Visibility {
        if self.is_admin() {
            Visibility::All
        } else {
            Visibility::Namespace(self.namespace_user().to_string())
        }
    }
}

/// Per-user namespace visibility: admins see everything; everyone else sees
/// global (NULL-scoped) entries plus their own namespace (plus explicit
/// shares/@mentions, applied separately).
#[derive(Clone, Debug)]
pub enum Visibility {
    All,
    Namespace(String),
}

/// May this principal read/act for `identity`'s private surfaces (recall
/// brief, inbox)? Admins: anyone. Tokens: only their own actor. Sessions:
/// also the AIs the logged-in user owns. Shared by the MCP tools and the
/// HTTP routes so the two doors can't drift apart.
pub async fn can_act_for_identity(
    store: &Store,
    ctx: &AuthCtx,
    identity: &str,
) -> anyhow::Result<bool> {
    if ctx.is_admin() || identity == ctx.actor() {
        return Ok(true);
    }
    if ctx.principal == Some("session") {
        let owner = ctx.namespace_user();
        return Ok(store
            .people_ais_owned_by(owner)
            .await?
            .iter()
            .any(|p| p.slug == identity));
    }
    Ok(false)
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
        if let Ok(Some((actor, namespace_user))) = store.tokens_resolve(bearer).await {
            ctx.actor = Some(actor);
            ctx.principal = Some("token");
            // Admin-bypass and namespace follow the granting human user.
            if let Ok(Some(u)) = store.users_by_actor(&namespace_user).await {
                ctx.role = Some(u.role);
            }
            ctx.namespace_user = Some(namespace_user);
        }
    }

    let cookie = cookie_value(headers, SESSION_COOKIE);
    if ctx.actor.is_none() {
        if let Some(value) = &cookie {
            if let Ok(Some(user)) = store.sessions_resolve(value).await {
                ctx.actor = Some(user.actor.clone());
                ctx.principal = Some("session");
                ctx.role = Some(user.role);
                ctx.namespace_user = Some(user.actor);
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

/// First value of a possibly comma-joined proxy header, trimmed.
fn first_forwarded(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or(s).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// The public origin of this instance — all OAuth metadata URLs derive from it.
/// An explicit `instance.url` config or `HIVE_PUBLIC_URL` wins; otherwise the
/// origin is reconstructed from the request, honoring the reverse proxy's
/// `X-Forwarded-Proto`/`X-Forwarded-Host` (Traefik terminates TLS, so the app
/// sees plain HTTP — without this the metadata would advertise `http://` and
/// break OAuth 2.1 clients like Claude Desktop). This also yields the right
/// origin whether the client arrived via the LAN host or the public one.
pub async fn issuer_for(store: &Store, headers: &HeaderMap) -> String {
    if let Ok(Some(url)) = store.config_get("instance.url").await {
        return url;
    }
    if let Ok(url) = std::env::var("HIVE_PUBLIC_URL") {
        return url;
    }
    let proto = first_forwarded(headers, "x-forwarded-proto").unwrap_or_else(|| "http".to_string());
    let host = first_forwarded(headers, "x-forwarded-host")
        .or_else(|| {
            headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .map(String::from)
        })
        .unwrap_or_else(|| {
            let port = std::env::var("PORT").unwrap_or_else(|_| "7878".to_string());
            format!("localhost:{port}")
        });
    format!("{proto}://{host}")
}

#[cfg(test)]
mod tests {
    use super::first_forwarded;
    use axum::http::header::HeaderName;
    use axum::http::HeaderMap;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn forwarded_proto_and_host_drive_the_issuer() {
        // Behind Traefik: X-Forwarded-Proto=https must win over the plain HTTP
        // the app actually sees, and the comma-joined first value is taken.
        let h = hm(&[
            ("x-forwarded-proto", "https"),
            ("x-forwarded-host", "hive.home.beesroadhouse.com"),
            ("host", "hive-api:7878"),
        ]);
        assert_eq!(
            first_forwarded(&h, "x-forwarded-proto").as_deref(),
            Some("https")
        );
        assert_eq!(
            first_forwarded(&h, "x-forwarded-host").as_deref(),
            Some("hive.home.beesroadhouse.com")
        );

        let chained = hm(&[("x-forwarded-proto", "https, http")]);
        assert_eq!(
            first_forwarded(&chained, "x-forwarded-proto").as_deref(),
            Some("https")
        );

        let none = hm(&[("host", "h:7878")]);
        assert_eq!(first_forwarded(&none, "x-forwarded-proto"), None);
    }
}
