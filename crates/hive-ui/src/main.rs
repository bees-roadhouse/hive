//! hive-ui ... leptos 0.7 SSR axum server for the journal-canvas.
//!
//! v0 scope: one route (`/`) renders the 5 most-recent journal entries
//! fetched live from hive-api. The markdown canvas, checkbox component,
//! and structured views land in follow-up commits.

mod api;
mod app;
mod auth;
mod markdown;
mod pages;

use std::net::SocketAddr;

use axum::Router;
use axum::extract::Form;
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use leptos::config::LeptosOptions;
use leptos_axum::LeptosRoutes;
use serde::Deserialize;
use tower_http::services::ServeDir;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::app::{App, shell};
use crate::auth::SESSION_COOKIE;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,hive_ui=debug")),
        )
        .with(fmt::layer())
        .init();

    let port: u16 = std::env::var("HIVE_UI_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8091);
    let addr: SocketAddr = SocketAddr::from(([0, 0, 0, 0], port));

    let conf = LeptosOptions::builder()
        .output_name("hive-ui")
        .site_root("target/site")
        .site_pkg_dir("pkg")
        .site_addr(addr)
        .reload_port(3001)
        .env(leptos::config::Env::DEV)
        .build();

    // Exclude /journal/new from the leptos route list — it's served by our
    // own hand-rolled axum handlers below (mirroring /login). Without the
    // exclusion the leptos `/journal/:id` route would swallow it.
    let routes = leptos_axum::generate_route_list_with_exclusions(
        App,
        Some(vec!["/journal/new".into(), "/who/:slug".into()]),
    );

    let style_dir = ServeDir::new("style");

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
        .route("/journal/new", get(compose_form).post(compose_submit))
        // `/who/:slug` is the AI-vs-human disambiguator for `@slug` mentions.
        // It looks the slug up server-side and redirects to /ai/:slug or
        // /people/:slug ... the mention renderer doesn't know which side
        // a bare `@slug` belongs to (the resolver records it post-write,
        // but the rendered prose doesn't carry the discriminator without
        // per-entry enrichment fetches).
        .route("/who/{slug}", get(who_redirect));

    let app: Router = Router::<LeptosOptions>::new()
        .leptos_routes(&conf, routes, move || shell(conf_for_shell.clone()))
        .nest_service("/style", style_dir)
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

// ---------- compose handlers (/journal/new) ----------

const COMPOSE_WRITERS: &[&str] = &["pia", "apis", "cera", "nate", "maggie"];
const COMPOSE_DEFAULT_WRITER: &str = "nate";

#[derive(Debug, Deserialize)]
struct ComposeForm {
    ai: String,
    date: Option<String>,
    title: String,
    body: String,
    tags: Option<String>,
}

/// GET /journal/new — server-rendered compose form. Mirrors `login_form`.
async fn compose_form(axum::extract::RawQuery(query): axum::extract::RawQuery) -> Html<String> {
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

    let today = chrono::Local::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();

    let writer_options = COMPOSE_WRITERS
        .iter()
        .map(|w| {
            let selected = if *w == COMPOSE_DEFAULT_WRITER {
                " selected"
            } else {
                ""
            };
            format!("<option value=\"{w}\"{selected}>{w}</option>")
        })
        .collect::<String>();

    Html(format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\"/>\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"/>\
         <title>hive · new entry</title>\
         <link rel=\"stylesheet\" href=\"/style/main.css\"/></head>\
         <body><main class=\"compose-page\"><h1>new entry</h1>{error_block}\
         <form method=\"post\" action=\"/journal/new\" class=\"compose-form\">\
         <label>writer <select name=\"ai\" required>{writer_options}</select></label>\
         <label>date <input name=\"date\" type=\"date\" value=\"{today}\"/></label>\
         <label>title <input name=\"title\" type=\"text\" required/></label>\
         <label>body <textarea name=\"body\" rows=\"14\" required></textarea></label>\
         <label>tags <input name=\"tags\" type=\"text\" placeholder=\"comma-separated, e.g. immich,traefik\"/></label>\
         <div class=\"compose-actions\">\
         <a class=\"compose-cancel\" href=\"/\">cancel</a>\
         <button type=\"submit\">save</button>\
         </div>\
         </form></main></body></html>"
    ))
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
