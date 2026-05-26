//! Auth configuration + the production-marker detection that gates the
//! dev-bypass (§5.8) and (later) hardening decisions.
//!
//! Phase 1 keeps this small: the issuer/audience the RS validates against, the
//! warn-vs-enforce switch (default warn, §8 Phase 1), and the prod-marker
//! probe. The dev-token + dev-bypass arming live behind the `dev` feature.

use std::net::SocketAddr;

/// How the auth layer treats a request that fails validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementMode {
    /// Log what WOULD be rejected, but let the request through. Phase 1 default
    /// so hive-ui/hive-cli keep working tokenless until they have tokens.
    Warn,
    /// Reject with 401/403. Flipped on in Phase 3 once clients carry tokens.
    Enforce,
}

impl EnforcementMode {
    fn from_env() -> Self {
        // HIVE_AUTH_ENFORCE=1/true flips to enforce; anything else => warn.
        match std::env::var("HIVE_AUTH_ENFORCE").ok().as_deref() {
            Some("1") | Some("true") | Some("TRUE") => EnforcementMode::Enforce,
            _ => EnforcementMode::Warn,
        }
    }
}

/// Resolved auth configuration, built once at startup and held in `AppState`.
#[derive(Debug, Clone)]
pub struct AuthConfig {
    /// Expected `iss` on validated tokens, and the issuer in AS metadata.
    pub issuer: String,
    /// Expected `aud` on validated tokens (the canonical RS/MCP URI).
    pub audience: String,
    pub mode: EnforcementMode,
    /// True when any production marker is present (§5.8). Even in a `dev`
    /// build, the bypass refuses to arm when this is set.
    pub prod_markers_present: bool,
}

impl AuthConfig {
    /// The canonical MCP resource URI (RFC 8707/9728, §3.3): the issuer with a
    /// `/mcp` path, no trailing slash, no fragment. This is the audience an AI's
    /// MCP token is bound to and the `resource` MCP clients request.
    pub fn mcp_resource(&self) -> String {
        format!("{}/mcp", self.issuer.trim_end_matches('/'))
    }

    /// Build from env + the bind address. `issuer`/`audience` default to the
    /// bind-derived localhost URL when `HIVE_PUBLIC_URL` is unset (dev), and to
    /// the public URL in production.
    pub fn from_env(bind: SocketAddr) -> Self {
        let public_url = std::env::var("HIVE_PUBLIC_URL")
            .ok()
            .filter(|s| !s.is_empty());
        let issuer = public_url
            .clone()
            .unwrap_or_else(|| format!("http://{bind}"));
        let audience = issuer.clone();

        AuthConfig {
            issuer,
            audience,
            mode: EnforcementMode::from_env(),
            prod_markers_present: detect_prod_markers(bind),
        }
    }
}

/// Detect production markers (§5.8): a public URL, in-process TLS, a
/// non-loopback bind, or an explicit `HIVE_ENV=production`. Any one means
/// "this is not a throwaway local dev process," so the dev-bypass must not arm.
pub fn detect_prod_markers(bind: SocketAddr) -> bool {
    let public_url = std::env::var("HIVE_PUBLIC_URL").is_ok_and(|s| !s.is_empty());
    let tls = std::env::var("HIVE_TLS").is_ok_and(|s| !s.is_empty());
    let env_prod = std::env::var("HIVE_ENV").is_ok_and(|s| s.eq_ignore_ascii_case("production"));
    let non_loopback = !bind.ip().is_loopback();
    public_url || tls || env_prod || non_loopback
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback() -> SocketAddr {
        "127.0.0.1:7878".parse().unwrap()
    }

    #[test]
    fn non_loopback_bind_is_a_prod_marker() {
        let public_bind: SocketAddr = "0.0.0.0:7878".parse().unwrap();
        assert!(detect_prod_markers(public_bind));
    }

    #[test]
    fn loopback_with_no_env_is_not_a_prod_marker() {
        // NOTE: relies on the prod-marker env vars being unset in the test env.
        // The CI/test runner does not set HIVE_PUBLIC_URL / HIVE_TLS / HIVE_ENV.
        // Loopback bind + no markers => false.
        if std::env::var("HIVE_PUBLIC_URL").is_err()
            && std::env::var("HIVE_TLS").is_err()
            && std::env::var("HIVE_ENV").is_err()
        {
            assert!(!detect_prod_markers(loopback()));
        }
    }
}
