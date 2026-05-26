//! CLI token carry + `hive login` (hive-auth-mcp-design.md §8 Phase 3, §3.2).
//!
//! Phase 3 gives the CLI a real token to send: the API is the source of truth
//! (no DB access here), so the CLI authenticates over HTTP and attaches a
//! `Bearer` on every request. Two token sources, resolved in order:
//!
//!   1. `HIVE_TOKEN` ... explicit env override (CI, scripts, `dev` token).
//!   2. a cached token file under the config dir (written by `hive login`).
//!
//! `hive login` runs the password -> `/authorize` (PKCE) -> `/token` flow and
//! caches the resulting access token. `hive login --device` (Phase 5) runs the
//! RFC 8628 device-authorization grant: it requests a device+user code, prints
//! where to approve it, and polls `/token` honoring the interval + slow_down
//! until the human approves in a browser.
//!
//! Security note: the cached file holds an access token (short-lived per
//! policy, §2). The design's end state is the OS keychain (§3.2); the file is
//! the Phase-3 stand-in and is written 0600 on unix.

use std::path::PathBuf;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::api;

/// Env var carrying an explicit token (wins over the cached file).
const TOKEN_ENV: &str = "HIVE_TOKEN";
/// First-party client id the AS pre-registers (skips consent, §4.5).
const CLIENT_ID: &str = "hive-cli";
/// The CLI is a public client; the AS ignores the redirect for the POST-JSON
/// authorize, but we send a loopback value to match the spec shape.
const REDIRECT_URI: &str = "http://127.0.0.1/cli/callback";

/// Resolve the bearer token to attach to requests, if any.
///
/// `None` is a valid state: under the Phase-1/2 warn-only server, a tokenless
/// CLI still works. Once the server flips to enforce, an unset token yields a
/// 401 with a clear message (see `api::error_message`), prompting `hive login`.
pub fn current_token() -> Option<String> {
    if let Ok(t) = std::env::var(TOKEN_ENV) {
        let t = t.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    read_token_file()
}

/// The cached-token path: `<config_dir>/hive/token`. `config_dir` is
/// `%APPDATA%` on Windows, `~/.config` on Linux, `~/Library/Application Support`
/// on macOS (via `directories`). Falls back to `~/.hive/token`.
fn token_file_path() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("com", "beesroadhouse", "hive") {
        return dirs.config_dir().join("token");
    }
    directories::UserDirs::new()
        .map(|u| u.home_dir().join(".hive").join("token"))
        .unwrap_or_else(|| PathBuf::from(".hive-token"))
}

fn read_token_file() -> Option<String> {
    let path = token_file_path();
    let raw = std::fs::read_to_string(&path).ok()?;
    let t = raw.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Cache the access token to the token file, creating the config dir. On unix
/// the file is written 0600 so other users can't read it.
fn write_token_file(token: &str) -> anyhow::Result<PathBuf> {
    let path = token_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }
    Ok(path)
}

/// Remove the cached token (the `hive logout` path).
pub fn clear_token() -> anyhow::Result<bool> {
    let path = token_file_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// A generated PKCE verifier + its S256 challenge (RFC 7636).
struct Pkce {
    verifier: String,
    challenge: String,
}

fn generate_pkce() -> Pkce {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let verifier = URL_SAFE_NO_PAD.encode(bytes);
    let digest = Sha256::digest(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(digest);
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
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

#[derive(Deserialize)]
struct AuthorizeResp {
    code: String,
}

#[derive(Serialize)]
struct TokenBody<'a> {
    grant_type: &'a str,
    code: &'a str,
    code_verifier: &'a str,
    redirect_uri: &'a str,
}

#[derive(Deserialize)]
struct TokenResp {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Run the password -> auth-code (PKCE) -> token flow against the configured
/// hive-api and cache the access token. `scope` is optional (the AS intersects
/// it with what the user is granted; `None` lets the server decide).
pub async fn login(username: &str, password: &str, scope: Option<&str>) -> anyhow::Result<()> {
    let base = api::api_base();
    let pkce = generate_pkce();

    let authorize: AuthorizeResp = api::post_unauthed(
        &format!("{base}/authorize"),
        &AuthorizeBody {
            username,
            password,
            client_id: CLIENT_ID,
            redirect_uri: REDIRECT_URI,
            code_challenge: &pkce.challenge,
            code_challenge_method: "S256",
            scope,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("login failed at /authorize: {e}"))?;

    let token: TokenResp = api::post_unauthed(
        &format!("{base}/token"),
        &TokenBody {
            grant_type: "authorization_code",
            code: &authorize.code,
            code_verifier: &pkce.verifier,
            redirect_uri: REDIRECT_URI,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("login failed at /token: {e}"))?;

    // The refresh token is returned but not yet cached: Phase 3 caches the
    // access token only (the keychain-backed refresh-rotation path is Phase 5,
    // alongside the device grant). Drop it explicitly so the intent is clear.
    let _ = token.refresh_token;

    let path = write_token_file(&token.access_token)?;
    let ttl = token
        .expires_in
        .map(|s| format!(" (expires in {s}s)"))
        .unwrap_or_default();
    println!(
        "logged in as '{username}'{ttl}; token cached at {}",
        path.display()
    );
    Ok(())
}

#[derive(Serialize)]
struct DeviceAuthBody<'a> {
    client_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}

#[derive(Deserialize)]
struct DeviceAuthResp {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Serialize)]
struct DeviceTokenBody<'a> {
    grant_type: &'a str,
    device_code: &'a str,
    client_id: &'a str,
}

/// An OAuth error body `{error, error_description}` — the device poll branches
/// on `error` (authorization_pending / slow_down / expired_token / ...).
#[derive(Deserialize)]
struct OAuthErrorBody {
    error: String,
}

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// RFC 8628 device-authorization grant (§3.2). Requests a device+user code,
/// prints the verification URL + user_code, and polls /token at the server's
/// interval (backing off +5s on slow_down) until the human approves. Caches the
/// access token on success.
pub async fn login_device(scope: Option<&str>) -> anyhow::Result<()> {
    let base = api::api_base();

    let auth: DeviceAuthResp = api::post_unauthed(
        &format!("{base}/device_authorization"),
        &DeviceAuthBody {
            client_id: CLIENT_ID,
            scope,
        },
    )
    .await
    .map_err(|e| anyhow::anyhow!("device authorization request failed: {e}"))?;

    // Tell the human where to go. Prefer the complete URL (carries the code).
    println!("To authorize this device, open:");
    if let Some(complete) = &auth.verification_uri_complete {
        println!("  {complete}");
    }
    println!(
        "  {}  and enter code:  {}",
        auth.verification_uri, auth.user_code
    );
    println!("Waiting for approval...");

    let mut interval = auth.interval.unwrap_or(5).max(1);
    let url = format!("{base}/token");
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;

        let body = DeviceTokenBody {
            grant_type: DEVICE_GRANT,
            device_code: &auth.device_code,
            client_id: CLIENT_ID,
        };
        match api::post_unauthed_raw(&url, &body).await? {
            // Success: a token JSON body.
            (true, text) => {
                let token: TokenResp = serde_json::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("malformed token response: {e}"))?;
                let _ = token.refresh_token;
                let path = write_token_file(&token.access_token)?;
                let ttl = token
                    .expires_in
                    .map(|s| format!(" (expires in {s}s)"))
                    .unwrap_or_default();
                println!("device approved{ttl}; token cached at {}", path.display());
                return Ok(());
            }
            // Error: branch on the OAuth error code (RFC 8628 §3.5).
            (false, text) => {
                let err = serde_json::from_str::<OAuthErrorBody>(&text)
                    .map(|e| e.error)
                    .unwrap_or_else(|_| "server_error".to_string());
                match err.as_str() {
                    "authorization_pending" => continue,
                    "slow_down" => {
                        interval += 5;
                        continue;
                    }
                    "expired_token" => {
                        anyhow::bail!(
                            "the code expired before approval; run `hive login --device` again"
                        )
                    }
                    "access_denied" => anyhow::bail!("authorization was denied"),
                    other => anyhow::bail!("device login failed: {other}"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256_of_verifier() {
        let p = generate_pkce();
        // Recompute the challenge from the verifier; must match.
        let digest = Sha256::digest(p.verifier.as_bytes());
        let expected = URL_SAFE_NO_PAD.encode(digest);
        assert_eq!(p.challenge, expected);
        // Verifier length: 32 random bytes base64url-no-pad = 43 chars.
        assert_eq!(p.verifier.len(), 43);
    }

    #[test]
    fn pkce_verifiers_are_unique() {
        let a = generate_pkce();
        let b = generate_pkce();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }

    #[test]
    fn token_file_path_ends_with_token() {
        let p = token_file_path();
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("token"));
    }
}
