//! hive-ui browser login + server-side token store (hive-auth-mcp-design.md
//! §8 Phase 3, §3.1).
//!
//! ## Cookie-vs-storage decision
//!
//! The design (§3.1) wants the refresh token in an `HttpOnly` cookie so client
//! JS can't read it. hive-ui is **Leptos 0.7 SSR with no hydration yet** (task
//! #7), so there is no browser JS to hold a token at all — every hive-api fetch
//! runs server-side inside the SSR render. That makes a pure-browser token
//! store both unnecessary and wrong here.
//!
//! So Phase 3 keeps the OAuth tokens **server-side** in this process, keyed by
//! an opaque session id, and the browser holds only that session id in an
//! `HttpOnly; SameSite=Strict` cookie (plus `Secure` when served over HTTPS).
//! This is stronger than the SPA case in the design: neither the access nor the
//! refresh token ever reaches the browser. When hydration lands and the client
//! starts making direct fetches, revisit (the access token would then need to
//! reach the client, or all writes stay proxied through SSR server functions).
//!
//! The store is in-memory and process-local: a restart drops sessions (users
//! re-login), which is fine for a single-node personal canvas. A durable/shared
//! store is a later concern if hive-ui ever scales out.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api::api_base;

/// The session cookie name. Opaque id only — never a token.
pub const SESSION_COOKIE: &str = "hive_ui_session";
const CLIENT_ID: &str = "hive-ui";
const REDIRECT_URI: &str = "http://127.0.0.1/ui/callback";

/// Tokens the server holds for one logged-in session.
#[derive(Debug, Clone)]
struct Tokens {
    access: String,
    refresh: String,
}

/// Process-local session -> tokens map. See the module doc for why server-side.
fn store() -> &'static Mutex<HashMap<String, Tokens>> {
    static STORE: OnceLock<Mutex<HashMap<String, Tokens>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("build reqwest client")
    })
}

/// A fresh opaque session id (256-bit, base64url).
fn new_session_id() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The access token for a session, if it has one. Read by `api.rs` per request
/// (the session id arrives via Leptos context, set from the cookie).
pub fn access_token_for(session_id: &str) -> Option<String> {
    store()
        .lock()
        .ok()?
        .get(session_id)
        .map(|t| t.access.clone())
}

/// Drop a session's tokens (logout).
pub fn forget(session_id: &str) {
    if let Ok(mut s) = store().lock() {
        s.remove(session_id);
    }
}

// ---------- OAuth flow (server-side, §3.1) ----------

struct Pkce {
    verifier: String,
    challenge: String,
}

fn generate_pkce() -> Pkce {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Pkce {
        verifier,
        challenge,
    }
}

#[derive(Serialize)]
struct AuthorizeBody<'a> {
    username: &'a str,
    password: &'a str,
    client_id: &'a str,
    redirect_uri: &'a str,
    code_challenge: &'a str,
    code_challenge_method: &'a str,
}

#[derive(Deserialize)]
struct AuthorizeResp {
    code: String,
}

#[derive(Serialize)]
struct CodeTokenBody<'a> {
    grant_type: &'a str,
    code: &'a str,
    code_verifier: &'a str,
    redirect_uri: &'a str,
}

#[derive(Serialize)]
struct RefreshTokenBody<'a> {
    grant_type: &'a str,
    refresh_token: &'a str,
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

/// Run the password -> /authorize (PKCE) -> /token flow against hive-api, store
/// the resulting tokens under a new session id, and return that id (the caller
/// sets it as the cookie). Errors carry hive-api's message for the login page.
pub async fn login(username: &str, password: &str) -> Result<String, String> {
    let base = api_base();
    let pkce = generate_pkce();

    let authorize: AuthorizeResp = post_json(
        &format!("{base}/authorize"),
        &AuthorizeBody {
            username,
            password,
            client_id: CLIENT_ID,
            redirect_uri: REDIRECT_URI,
            code_challenge: &pkce.challenge,
            code_challenge_method: "S256",
        },
    )
    .await
    .map_err(|e| format!("authorize failed: {e}"))?;

    let token: TokenResp = post_json(
        &format!("{base}/token"),
        &CodeTokenBody {
            grant_type: "authorization_code",
            code: &authorize.code,
            code_verifier: &pkce.verifier,
            redirect_uri: REDIRECT_URI,
        },
    )
    .await
    .map_err(|e| format!("token exchange failed: {e}"))?;

    let session_id = new_session_id();
    let refresh = token.refresh_token.unwrap_or_default();
    if let Ok(mut s) = store().lock() {
        s.insert(
            session_id.clone(),
            Tokens {
                access: token.access_token,
                refresh,
            },
        );
    }
    Ok(session_id)
}

/// Attempt a refresh-token rotation for a session after a 401 (§2 rotation).
/// On success the store is updated with the new access + refresh and the fresh
/// access token is returned. On failure the session is forgotten (the user must
/// re-login) and `None` is returned.
pub async fn refresh(session_id: &str) -> Option<String> {
    let refresh_token = {
        let s = store().lock().ok()?;
        s.get(session_id)?.refresh.clone()
    };
    if refresh_token.is_empty() {
        forget(session_id);
        return None;
    }
    let base = api_base();
    let res: Result<TokenResp, String> = post_json(
        &format!("{base}/token"),
        &RefreshTokenBody {
            grant_type: "refresh_token",
            refresh_token: &refresh_token,
        },
    )
    .await;
    match res {
        Ok(token) => {
            let new_access = token.access_token.clone();
            if let Ok(mut s) = store().lock()
                && let Some(t) = s.get_mut(session_id)
            {
                t.access = token.access_token;
                if let Some(r) = token.refresh_token {
                    t.refresh = r;
                }
            }
            Some(new_access)
        }
        Err(_) => {
            // Refresh rejected (expired/revoked/reuse) -> session is dead.
            forget(session_id);
            None
        }
    }
}

async fn post_json<B: Serialize, T: serde::de::DeserializeOwned>(
    url: &str,
    body: &B,
) -> Result<T, String> {
    let resp = http_client()
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let text = resp.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        // Surface hive-api's {error, error_description} when present.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            let msg = v
                .get("error_description")
                .or_else(|| v.get("error"))
                .and_then(|m| m.as_str());
            if let Some(m) = msg {
                return Err(m.to_string());
            }
        }
        return Err(format!("{status}"));
    }
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = generate_pkce();
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expected);
    }

    #[test]
    fn store_roundtrip_and_forget() {
        let sid = new_session_id();
        store().lock().unwrap().insert(
            sid.clone(),
            Tokens {
                access: "acc".into(),
                refresh: "ref".into(),
            },
        );
        assert_eq!(access_token_for(&sid), Some("acc".to_string()));
        forget(&sid);
        assert_eq!(access_token_for(&sid), None);
    }

    #[test]
    fn session_ids_are_unique() {
        assert_ne!(new_session_id(), new_session_id());
    }
}
