//! Dev-mode auth bypass (hive-auth-mcp-design.md §5.8). DEV-ONLY.
//!
//! This entire module is compiled ONLY under `--features dev`. A release build
//! does not contain it — that compile-time gate is the primary safeguard. The
//! runtime gates below (loopback peer, no prod markers, configured token) are
//! belt-and-suspenders for the dev build itself.
//!
//! When all gates pass, a request bearing the configured `HIVE_DEV_TOKEN`
//! authenticates as a SYNTHETIC `dev-bypass` principal — a distinct identity,
//! never impersonating a real user or AI, so dev-created data stays honestly
//! attributable. Every use logs loudly at WARN.

use std::net::SocketAddr;

use super::claims::{Principal, PrincipalType, ResolvedPermissions};
use super::config::AuthConfig;

/// The reserved synthetic subject for dev-bypass auth. Never a real user/AI id.
pub const DEV_BYPASS_SUBJECT: &str = "dev-bypass";

/// The env var holding the operator-chosen dev token. No baked-in default: if
/// unset, the bypass is inert even in a dev build on localhost.
const DEV_TOKEN_ENV: &str = "HIVE_DEV_TOKEN";

/// Attempt the dev-bypass for a request. Returns `Some(principal)` only when
/// EVERY gate passes:
///
/// 1. peer address is loopback (checked on the real socket, not a header),
/// 2. no production markers are present (§5.8),
/// 3. `HIVE_DEV_TOKEN` is set AND the presented token matches it.
///
/// Otherwise `None` — the normal auth path then runs.
pub fn try_dev_bypass(
    cfg: &AuthConfig,
    peer: Option<SocketAddr>,
    presented_token: Option<&str>,
) -> Option<Principal> {
    // Gate 2: refuse on prod markers. Cheapest decisive check first.
    if cfg.prod_markers_present {
        return None;
    }
    // Gate 1: loopback peer only. A missing peer addr fails closed.
    match peer {
        Some(addr) if addr.ip().is_loopback() => {}
        _ => return None,
    }
    // Gate 3: token must be configured AND match.
    let configured = std::env::var(DEV_TOKEN_ENV)
        .ok()
        .filter(|s| !s.is_empty())?;
    let presented = presented_token?;
    if !constant_time_eq(configured.as_bytes(), presented.as_bytes()) {
        return None;
    }

    tracing::warn!(
        peer = ?peer,
        "DEV-MODE AUTH BYPASS used — synthetic full-authority principal '{}'; \
         this build is NOT for production",
        DEV_BYPASS_SUBJECT
    );

    Some(Principal {
        subject: DEV_BYPASS_SUBJECT.to_string(),
        kind: PrincipalType::Dev,
        act: None,
        permissions: ResolvedPermissions::full(),
    })
}

/// Loud one-time startup banner when the bypass is armed (dev feature compiled
/// in + token set + no prod markers). Called from `main` after config build.
pub fn log_startup_banner(cfg: &AuthConfig) {
    let token_set = std::env::var(DEV_TOKEN_ENV).is_ok_and(|s| !s.is_empty());
    if cfg.prod_markers_present {
        tracing::warn!(
            "dev feature is COMPILED IN but production markers are present — \
             dev-bypass is HARD-DISABLED. (This build should not be in production.)"
        );
    } else if token_set {
        tracing::warn!(
            "DEV-MODE AUTH BYPASS ARMED — HIVE_DEV_TOKEN is set, requests on \
             loopback presenting it authenticate as '{}' with full authority. \
             DEV BUILD ONLY.",
            DEV_BYPASS_SUBJECT
        );
    } else {
        tracing::info!("dev feature compiled in but HIVE_DEV_TOKEN unset — dev-bypass inert.");
    }
}

/// Constant-time byte comparison so dev-token matching doesn't leak length/
/// content via timing. Small surface, but free to do right.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev_cfg(prod_markers: bool) -> AuthConfig {
        AuthConfig {
            issuer: "http://127.0.0.1:7878".to_string(),
            audience: "http://127.0.0.1:7878".to_string(),
            mode: super::super::config::EnforcementMode::Warn,
            prod_markers_present: prod_markers,
        }
    }

    fn loopback() -> SocketAddr {
        "127.0.0.1:50000".parse().unwrap()
    }

    #[test]
    fn prod_markers_block_bypass_even_with_token_and_loopback() {
        // SAFETY: single-threaded test scope; we set then remove the env var.
        unsafe { std::env::set_var(DEV_TOKEN_ENV, "secret") };
        let got = try_dev_bypass(&dev_cfg(true), Some(loopback()), Some("secret"));
        unsafe { std::env::remove_var(DEV_TOKEN_ENV) };
        assert!(got.is_none(), "prod markers must hard-block the bypass");
    }

    #[test]
    fn non_loopback_peer_blocks_bypass() {
        unsafe { std::env::set_var(DEV_TOKEN_ENV, "secret") };
        let remote: SocketAddr = "203.0.113.5:50000".parse().unwrap();
        let got = try_dev_bypass(&dev_cfg(false), Some(remote), Some("secret"));
        unsafe { std::env::remove_var(DEV_TOKEN_ENV) };
        assert!(got.is_none(), "non-loopback peer must block the bypass");
    }

    #[test]
    fn wrong_token_blocks_bypass() {
        unsafe { std::env::set_var(DEV_TOKEN_ENV, "secret") };
        let got = try_dev_bypass(&dev_cfg(false), Some(loopback()), Some("wrong"));
        unsafe { std::env::remove_var(DEV_TOKEN_ENV) };
        assert!(got.is_none(), "mismatched token must block the bypass");
    }

    #[test]
    fn all_gates_pass_yields_synthetic_full_authority_principal() {
        unsafe { std::env::set_var(DEV_TOKEN_ENV, "secret") };
        let got = try_dev_bypass(&dev_cfg(false), Some(loopback()), Some("secret"));
        unsafe { std::env::remove_var(DEV_TOKEN_ENV) };
        let p = got.expect("all gates pass => Some");
        assert_eq!(p.subject, DEV_BYPASS_SUBJECT);
        assert_eq!(p.kind, PrincipalType::Dev);
        assert!(p.permissions.is_admin);
        assert!(p.permissions.has_scope("anything"));
    }
}
