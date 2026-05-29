//! hive-ui ... leptos 0.7 isomorphic axum server for the journal-canvas.
//!
//! The library at `hive_ui::*` carries the actual UI (the `App` component,
//! the pages, the api client). This bin wires it into axum: serves the SSR
//! HTML through `leptos_axum::LeptosRoutes`, exposes the hand-rolled `/login`,
//! `/logout`, POST `/journal/new`, `/api/recent`, `/who/:slug` endpoints, and
//! mounts the cargo-leptos site output (`target/site/pkg/...`) so the WASM
//! bundle reaches the browser.

use std::net::SocketAddr;

use axum::Router;
use axum::extract::{Form, Query};
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Json, Redirect, Response};
use axum::routing::{get, post};
use leptos::config::LeptosOptions;
use leptos_axum::LeptosRoutes;
use serde::Deserialize;
use tower_http::services::ServeDir;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use hive_ui::api;
use hive_ui::app::{App, shell};
use hive_ui::auth;
use hive_ui::auth::SESSION_COOKIE;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,hive_ui=debug")),
        )
        .with(fmt::layer())
        .init();

    // Address resolution: cargo-leptos sets `LEPTOS_SITE_ADDR` when it
    // spawns the bin (so the leptos dev server can reverse-proxy). When
    // running standalone (`cargo run`) we fall back to `HIVE_UI_PORT`.
    let addr: SocketAddr = if let Ok(site_addr) = std::env::var("LEPTOS_SITE_ADDR") {
        site_addr
            .parse()
            .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 8091)))
    } else {
        let port: u16 = std::env::var("HIVE_UI_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8091);
        SocketAddr::from(([0, 0, 0, 0], port))
    };

    let conf = LeptosOptions::builder()
        .output_name("hive-ui")
        .site_root("target/site")
        .site_pkg_dir("pkg")
        .site_addr(addr)
        .reload_port(3001)
        .env(leptos::config::Env::DEV)
        .build();

    // `/who/:slug` is a hand-rolled axum redirect (the slug-disambiguator), so
    // we exclude it from the leptos route table. `/journal/new` is now a real
    // leptos route (ComposePage); the POST handler below shares the path but
    // axum's method routing keeps them separate.
    let routes =
        leptos_axum::generate_route_list_with_exclusions(App, Some(vec!["/who/:slug".into()]));

    // The legacy `style/main.css` is still served at `/style/main.css`
    // because the hand-rolled login/who-not-found HTML links to it directly.
    // cargo-leptos puts the compiled stylesheet at `target/site/pkg/hive-ui.css`
    // (referenced from the Leptos `<Stylesheet>` in `App`).
    let style_dir = ServeDir::new("style");
    let static_dir = ServeDir::new("static");
    // cargo-leptos site output: `target/site/pkg/{hive-ui.js,hive-ui.wasm,hive-ui.css}`.
    // Leptos's `<HydrationScripts/>` (in `app::shell`) emits a loader script
    // that passes the wasm path explicitly to wasm-bindgen's init function,
    // so the file naming stays internally consistent (`hive-ui.wasm`, not
    // the wasm-bindgen default `_bg.wasm`).
    let site_root = std::env::var("LEPTOS_SITE_ROOT").unwrap_or_else(|_| "target/site".to_string());
    let pkg_dir = ServeDir::new(format!("{site_root}/pkg"));

    let conf_for_shell = conf.clone();
    // Auth routes (Phase 3, §3.1): the OAuth password+PKCE flow runs entirely
    // server-side, so /login + /logout are plain axum handlers, separate from
    // the leptos-rendered views. They carry no LeptosOptions state.
    //
    // The compose flow (/journal/new GET + POST) mirrors that pattern — also
    // hand-rolled, also stateless, also nothing for leptos to render.
    let auth_routes: Router = Router::new()
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout).get(logout))
        // POST /journal/new is the form-submit handler; the GET form itself
        // is a Leptos route (pages::compose::ComposePage). Axum's method
        // routing lets both share the same path.
        .route("/journal/new", post(compose_submit))
        // `/api/recent` is the typeahead source for the compose-time entity
        // picker (the hydrated ComposePage). One endpoint per page,
        // server-side fan-out to the per-type hive-api endpoints, JSON out.
        .route("/api/recent", get(api_recent))
        // `/who/:slug` is the AI-vs-human disambiguator for `@slug` mentions.
        // It looks the slug up server-side and redirects to /ai/:slug or
        // /people/:slug ... the mention renderer doesn't know which side
        // a bare `@slug` belongs to (the resolver records it post-write,
        // but the rendered prose doesn't carry the discriminator without
        // per-entry enrichment fetches).
        .route("/who/:slug", get(who_redirect));

    let app: Router = Router::<LeptosOptions>::new()
        .leptos_routes(&conf, routes, move || shell(conf_for_shell.clone()))
        .nest_service("/style", style_dir)
        .nest_service("/static", static_dir)
        .nest_service("/pkg", pkg_dir)
        .with_state(conf)
        .merge(auth_routes);

    tracing::info!("hive-ui listening on http://{addr}");
    tracing::info!("hive-api base: {}", api::api_base());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

// ---------- auth handlers (Phase 3, §3.1) ----------

#[derive(Debug, Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

/// GET /login — the HTML login form. `?error=...` renders a message after a
/// failed POST. Minimal, server-rendered (no JS): a real form posting to the
/// same path.
async fn login_form(axum::extract::RawQuery(query): axum::extract::RawQuery) -> Html<String> {
    let error = query
        .as_deref()
        .and_then(|q| {
            q.split('&')
                .filter_map(|kv| kv.split_once('='))
                .find(|(k, _)| *k == "error")
                .map(|(_, v)| percent_decode(v))
        })
        .unwrap_or_default();
    let error_block = if error.is_empty() {
        String::new()
    } else {
        format!("<p class=\"error\">{}</p>", html_escape(&error))
    };
    Html(format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"/>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"/>\
         <title>hive · login</title>\
         <link rel=\"stylesheet\" href=\"/style/main.css\"/></head>\
         <body><main class=\"login-page\"><h1>hive login</h1>{error_block}\
         <form method=\"post\" action=\"/login\" class=\"login-form\">\
         <label>username <input name=\"username\" autocomplete=\"username\" required/></label>\
         <label>password <input name=\"password\" type=\"password\" autocomplete=\"current-password\" required/></label>\
         <button type=\"submit\">log in</button></form></main></body></html>"
    ))
}

/// POST /login — run the OAuth flow, set the session cookie, redirect home.
/// On failure, redirect back to the form with the error message.
async fn login_submit(Form(form): Form<LoginForm>) -> Response {
    match auth::login(form.username.trim(), &form.password).await {
        Ok(session_id) => {
            let cookie = session_cookie(&session_id);
            let mut headers = HeaderMap::new();
            if let Ok(v) = cookie.parse() {
                headers.insert(header::SET_COOKIE, v);
            }
            (headers, Redirect::to("/")).into_response()
        }
        Err(msg) => {
            let to = format!("/login?error={}", percent_encode(&msg));
            Redirect::to(&to).into_response()
        }
    }
}

/// GET|POST /logout — forget the session server-side, clear the cookie, home.
async fn logout(headers: HeaderMap) -> Response {
    if let Some(sid) = session_from_headers(&headers) {
        auth::forget(&sid);
    }
    let mut out = HeaderMap::new();
    if let Ok(v) = expired_cookie().parse() {
        out.insert(header::SET_COOKIE, v);
    }
    (out, Redirect::to("/")).into_response()
}

/// Build the session Set-Cookie. `HttpOnly` (no JS access) + `SameSite=Strict`
/// (CSRF resistance) + `Path=/`. `Secure` is added when serving over HTTPS
/// (signalled by HIVE_PUBLIC_URL starting with https) so localhost dev over
/// http still works.
fn session_cookie(session_id: &str) -> String {
    let secure = is_https();
    let mut c = format!("{SESSION_COOKIE}={session_id}; HttpOnly; SameSite=Strict; Path=/");
    if secure {
        c.push_str("; Secure");
    }
    c
}

fn expired_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0")
}

fn is_https() -> bool {
    std::env::var("HIVE_PUBLIC_URL")
        .map(|u| u.trim_start().starts_with("https://"))
        .unwrap_or(false)
}

fn session_from_headers(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie
        .split(';')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| k.trim() == SESSION_COOKIE)
        .map(|(_, v)| v.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------- compose POST handler (/journal/new) ----------
//
// The GET form is now a hydrated Leptos route (pages::compose::ComposePage).
// This handler only handles the form submit: validate, forward to hive-api,
// redirect home (or back to the form with ?error=...).

const COMPOSE_WRITERS: &[&str] = &["pia", "apis", "cera", "nate", "maggie"];

#[derive(Debug, Deserialize)]
struct ComposeForm {
    ai: String,
    date: Option<String>,
    title: String,
    body: String,
    tags: Option<String>,
}

/// POST /journal/new — validate, forward to hive-api, redirect.
async fn compose_submit(headers: HeaderMap, Form(form): Form<ComposeForm>) -> Response {
    let ai = form.ai.trim();
    if !COMPOSE_WRITERS.contains(&ai) {
        let to = format!("/journal/new?error={}", percent_encode("invalid writer"));
        return Redirect::to(&to).into_response();
    }

    let title = form.title.trim();
    let body = form.body.trim();
    if title.is_empty() || body.is_empty() {
        let to = format!(
            "/journal/new?error={}",
            percent_encode("title and body are required")
        );
        return Redirect::to(&to).into_response();
    }

    let date_owned = form
        .date
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tags_owned = form
        .tags
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut payload = serde_json::Map::new();
    payload.insert("ai".into(), serde_json::Value::String(ai.to_string()));
    payload.insert("title".into(), serde_json::Value::String(title.to_string()));
    payload.insert("body".into(), serde_json::Value::String(body.to_string()));
    if let Some(d) = date_owned {
        payload.insert("date".into(), serde_json::Value::String(d));
    }
    if let Some(t) = tags_owned {
        payload.insert("tags".into(), serde_json::Value::String(t));
    }

    let url = format!("{}/journal", api::api_base());
    let token = session_from_headers(&headers).and_then(|sid| auth::access_token_for(&sid));

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            let to = format!("/journal/new?error={}", percent_encode(&e.to_string()));
            return Redirect::to(&to).into_response();
        }
    };

    let mut req = client.post(&url).json(&payload);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let to = format!("/journal/new?error={}", percent_encode(&e.to_string()));
            return Redirect::to(&to).into_response();
        }
    };

    let status = resp.status();
    if status.is_success() {
        return Redirect::to("/").into_response();
    }

    // Surface the api's error text when we have one; fall back to the status.
    let body_text = resp.text().await.unwrap_or_default();
    let msg = if body_text.is_empty() {
        status.to_string()
    } else {
        // Try to pull error_description / error / message out of a JSON body.
        match serde_json::from_str::<serde_json::Value>(&body_text) {
            Ok(v) => v
                .get("error_description")
                .or_else(|| v.get("error"))
                .or_else(|| v.get("message"))
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
                .unwrap_or(body_text),
            Err(_) => body_text,
        }
    };
    let to = format!("/journal/new?error={}", percent_encode(&msg));
    Redirect::to(&to).into_response()
}

// ---------- /who/:slug disambiguator ----------

/// GET /who/:slug — resolve a bare `@slug` mention to its canonical detail
/// page. Looks up the people directory and `Redirect::permanent` to either
/// `/ai/<slug>` (kind = ai) or `/people/<slug>` (kind = human). If neither
/// matches, render a small "no such handle" page.
async fn who_redirect(axum::extract::Path(slug): axum::extract::Path<String>) -> Response {
    // Try the AI directory first, then humans. The kind discriminator is
    // now the API path itself (`/ai` vs `/people`) so we probe both. We hit
    // the API directly (rather than the client helper in `api.rs`) because
    // this handler runs outside the leptos render context, so the
    // SessionId-from-context plumbing doesn't apply.
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return who_not_found(&slug).into_response(),
    };
    let base = api::api_base();
    for (path_prefix, target_prefix) in [("/ai", "/ai"), ("/people", "/people")] {
        let url = format!("{base}{path_prefix}/{slug}");
        if let Ok(resp) = client.get(&url).send().await
            && resp.status().is_success()
        {
            return Redirect::permanent(&format!("{target_prefix}/{slug}")).into_response();
        }
    }
    who_not_found(&slug).into_response()
}

fn who_not_found(slug: &str) -> Html<String> {
    Html(format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"/>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"/>\
         <title>hive · no such handle</title>\
         <link rel=\"stylesheet\" href=\"/style/main.css\"/></head>\
         <body><main class=\"who-page\"><p class=\"entry-back\"><a href=\"/people\">← people</a> <a href=\"/ai\">ai</a></p>\
         <h1>no such handle</h1>\
         <p>no person or ai is registered with the slug <code>{slug}</code>.</p>\
         </main></body></html>",
        slug = html_escape(slug)
    ))
}

// ---------- /api/recent (compose-time picker backend) ----------

#[derive(Debug, Deserialize)]
struct RecentQuery {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    q: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct RecentRow {
    id: String,
    title: String,
    meta: String,
    created_at: String,
}

/// GET /api/recent?type=<task|note|event|journal|person|ai>&q=<substring>
///
/// Single endpoint for the compose-picker.js typeahead. Server-side
/// fan-out to the per-type hive-api endpoints, case-insensitive title
/// filter applied in Rust (so the upstream stays untouched), sorted by
/// `created_at DESC`, capped at 20.
async fn api_recent(headers: HeaderMap, Query(q): Query<RecentQuery>) -> Response {
    let token = session_from_headers(&headers).and_then(|sid| auth::access_token_for(&sid));
    let needle = q.q.as_deref().unwrap_or("").trim().to_lowercase();

    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "api_recent: build client failed");
            return Json(Vec::<RecentRow>::new()).into_response();
        }
    };

    let rows = match q.ty.as_str() {
        "task" => fetch_recent_tasks(&client, token.as_deref()).await,
        "note" => fetch_recent_notes(&client, token.as_deref()).await,
        "event" => fetch_recent_events(&client, token.as_deref()).await,
        "journal" => fetch_recent_journal(&client, token.as_deref()).await,
        "person" => fetch_recent_people(&client, token.as_deref()).await,
        "ai" => fetch_recent_ai(&client, token.as_deref()).await,
        _ => Vec::new(),
    };

    let mut filtered: Vec<RecentRow> = if needle.is_empty() {
        rows
    } else {
        rows.into_iter()
            .filter(|r| r.title.to_lowercase().contains(&needle))
            .collect()
    };

    // Sort created_at desc. Empty strings sort last by reversing later.
    filtered.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    filtered.truncate(20);

    Json(filtered).into_response()
}

async fn api_get_json(
    client: &reqwest::Client,
    path: &str,
    token: Option<&str>,
) -> Option<serde_json::Value> {
    let url = format!("{}{}", api::api_base(), path);
    let mut req = client.get(&url);
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => resp.json::<serde_json::Value>().await.ok(),
        Ok(resp) => {
            tracing::debug!(status = %resp.status(), %url, "api_recent: non-success");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, %url, "api_recent: fetch failed");
            None
        }
    }
}

fn json_str<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

async fn fetch_recent_tasks(client: &reqwest::Client, token: Option<&str>) -> Vec<RecentRow> {
    // `all=true` so closed tasks show up too ... the picker is a reference
    // surface, not a worklist.
    let v = match api_get_json(client, "/tasks?all=true", token).await {
        Some(v) => v,
        None => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .map(|t| {
            let owner = json_str(t, "owner");
            let status = json_str(t, "status");
            RecentRow {
                id: json_str(t, "id").to_string(),
                title: json_str(t, "title").to_string(),
                meta: format!("@{owner} · {status}"),
                created_at: json_str(t, "created_at").to_string(),
            }
        })
        .collect()
}

async fn fetch_recent_notes(client: &reqwest::Client, token: Option<&str>) -> Vec<RecentRow> {
    let v = match api_get_json(client, "/notes?limit=50", token).await {
        Some(v) => v,
        None => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .map(|n| {
            let author = json_str(n, "author");
            RecentRow {
                id: json_str(n, "id").to_string(),
                title: json_str(n, "title").to_string(),
                meta: format!("@{author}"),
                created_at: json_str(n, "created_at").to_string(),
            }
        })
        .collect()
}

async fn fetch_recent_events(client: &reqwest::Client, token: Option<&str>) -> Vec<RecentRow> {
    let v = match api_get_json(client, "/events?limit=50", token).await {
        Some(v) => v,
        None => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .map(|e| {
            let starts = json_str(e, "starts_at");
            // Trim to YYYY-MM-DD for the meta line.
            let day = starts.get(..10).unwrap_or(starts);
            RecentRow {
                id: json_str(e, "id").to_string(),
                title: json_str(e, "title").to_string(),
                meta: day.to_string(),
                created_at: json_str(e, "created_at").to_string(),
            }
        })
        .collect()
}

async fn fetch_recent_journal(client: &reqwest::Client, token: Option<&str>) -> Vec<RecentRow> {
    let v = match api_get_json(client, "/journal?limit=50", token).await {
        Some(v) => v,
        None => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .map(|j| {
            let ai = json_str(j, "ai");
            let date = json_str(j, "entry_date");
            let title = json_str(j, "title");
            let title = if title.is_empty() {
                "(untitled)"
            } else {
                title
            };
            RecentRow {
                id: json_str(j, "id").to_string(),
                title: title.to_string(),
                meta: format!("@{ai} · {date}"),
                created_at: json_str(j, "created_at").to_string(),
            }
        })
        .collect()
}

async fn fetch_recent_people(client: &reqwest::Client, token: Option<&str>) -> Vec<RecentRow> {
    let v = match api_get_json(client, "/people", token).await {
        Some(v) => v,
        None => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .map(|p| RecentRow {
            id: json_str(p, "id").to_string(),
            title: json_str(p, "display_name").to_string(),
            meta: String::new(),
            created_at: json_str(p, "created_at").to_string(),
        })
        .collect()
}

async fn fetch_recent_ai(client: &reqwest::Client, token: Option<&str>) -> Vec<RecentRow> {
    let v = match api_get_json(client, "/ai", token).await {
        Some(v) => v,
        None => return Vec::new(),
    };
    let arr = match v.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    arr.iter()
        .map(|a| RecentRow {
            id: json_str(a, "id").to_string(),
            title: json_str(a, "display_name").to_string(),
            meta: String::new(),
            created_at: json_str(a, "created_at").to_string(),
        })
        .collect()
}

/// Minimal HTML-escape for the error message echoed into the login page.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Minimal percent-encode/decode for the single `error` query param round-trip.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}
