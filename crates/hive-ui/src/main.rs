//! hive-ui ... leptos 0.7 SSR axum server for the journal-canvas.
//!
//! v0 scope: one route (`/`) renders the 5 most-recent journal entries
//! fetched live from hive-api. The markdown canvas, checkbox component,
//! and structured views land in follow-up commits.

mod api;
mod app;
mod auth;
mod pages;

use std::net::SocketAddr;

use axum::Router;
use axum::extract::Form;
use axum::http::{HeaderMap, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use leptos::config::LeptosOptions;
use leptos_axum::{LeptosRoutes, generate_route_list};
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

    let routes = generate_route_list(App);

    let style_dir = ServeDir::new("style");

    let conf_for_shell = conf.clone();
    // Auth routes (Phase 3, §3.1): the OAuth password+PKCE flow runs entirely
    // server-side, so /login + /logout are plain axum handlers, separate from
    // the leptos-rendered views. They carry no LeptosOptions state.
    let auth_routes: Router = Router::new()
        .route("/login", get(login_form).post(login_submit))
        .route("/logout", post(logout).get(logout));

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
