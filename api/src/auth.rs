// HTTP-side auth: policy toggles, PKCE, CSRF, the session cookie name. The
// shared token/password/time primitives moved to hive-core (the store layer
// uses them) and are re-exported here so `crate::auth::*` paths keep resolving.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub use hive_core::auth::{
    generate_token, hash_password, iso_in_days, iso_in_secs, token_hash, verify_password,
    API_TOKEN_PREFIX, AUTH_CODE_PREFIX, AUTH_CODE_TTL_SECS, OAUTH_TOKEN_TTL_MAX_SECS,
    OAUTH_TOKEN_TTL_MIN_SECS, OAUTH_TOKEN_TTL_NEVER, OAUTH_TOKEN_TTL_SECS, SESSION_PREFIX,
    SESSION_TTL_SECS,
};
pub use hive_core::store::now_iso;

pub const SESSION_COOKIE: &str = "hive_session";

pub fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

/// Local email/password login. Keep onboarding available so a fresh instance can
/// still bootstrap, but the login route can be globally disabled for SSO-only
/// deployments.
pub fn local_auth_enabled() -> bool {
    env_bool("HIVE_LOCAL_AUTH_ENABLED", true)
}

/// OIDC can be explicitly disabled even when issuer/client env is present.
pub fn oidc_enabled() -> bool {
    env_bool("HIVE_OIDC_ENABLED", true)
}

/// Whether the OAuth consent screen may request a non-expiring MCP/API token.
pub fn oauth_never_expires_enabled() -> bool {
    env_bool("HIVE_OAUTH_ALLOW_NEVER_EXPIRES", true)
}

/// PKCE S256 verify: base64url(sha256(verifier)) === challenge (constant-time).
pub fn verify_pkce(verifier: &str, challenge: &str) -> bool {
    let computed = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    computed.as_bytes().ct_eq(challenge.as_bytes()).into()
}

/// CSRF token bound to the session cookie (stateless double-submit).
pub fn csrf_for(session_cookie: &str) -> String {
    token_hash(&format!("{session_cookie}:oauth-csrf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        assert!(verify_pkce(verifier, challenge));
        assert!(!verify_pkce("wrong", challenge));
    }

    #[test]
    fn env_bool_accepts_common_toggles_and_defaults() {
        const KEY: &str = "HIVE_TEST_BOOL_TOGGLE";
        std::env::remove_var(KEY);
        assert!(env_bool(KEY, true));
        assert!(!env_bool(KEY, false));

        std::env::set_var(KEY, "off");
        assert!(!env_bool(KEY, true));
        std::env::set_var(KEY, "yes");
        assert!(env_bool(KEY, false));
        std::env::set_var(KEY, "not-a-bool");
        assert!(env_bool(KEY, true));
        assert!(!env_bool(KEY, false));
        std::env::remove_var(KEY);
    }
}
