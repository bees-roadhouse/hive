//! Auth policy + the session-lifetime math (hive-auth-mcp-design.md §2, §5).
//!
//! The single-row `auth_policy` table holds the global knobs; this module loads
//! it and computes the effective session/access lifetimes for a login.

use hive_db::PgPool;

/// The MFA enforcement mode (§4). `internal` = hive prompts for TOTP;
/// `delegated` = an external IdP owns MFA (don't prompt); `off` = no second
/// factor (dev / single-user only). The seam that lets "external IdP present →
/// internal MFA off" be a config flip, not a code change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MfaMode {
    Internal,
    Delegated,
    Off,
}

impl MfaMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "internal" => Some(MfaMode::Internal),
            "delegated" => Some(MfaMode::Delegated),
            "off" => Some(MfaMode::Off),
            _ => None,
        }
    }

    /// Does hive enforce its own TOTP second factor at login? Only `internal`;
    /// `delegated` (IdP owns MFA) and `off` skip it.
    pub fn enforces_internal(&self) -> bool {
        matches!(self, MfaMode::Internal)
    }
}

/// Where AUTHENTICATION comes from (§6). `builtin` = hive's own AS verifies
/// password/MFA + issues tokens (today). `external` = an external OIDC IdP is
/// the AS; hive validates the IdP's tokens. Phase 9 lands the switch + the seam;
/// `external` is INERT until a provider adapter is wired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    Builtin,
    External,
}

impl AuthMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            // accept both the design's name and the lead's alias
            "builtin" | "internal" => Some(AuthMode::Builtin),
            "external" => Some(AuthMode::External),
            _ => None,
        }
    }

    pub fn is_external(&self) -> bool {
        matches!(self, AuthMode::External)
    }
}

/// Where AUTHORIZATION (roles/scopes) comes from (§6.1). `internal` = hive's own
/// grant tables (today). `external` = the IdP's claims, mapped through
/// `idp_permission_map`. Parallel to `auth_mode`; `external` is INERT until the
/// map is populated (empty map = deny-by-default, fails closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthzMode {
    Internal,
    External,
}

impl AuthzMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "internal" => Some(AuthzMode::Internal),
            "external" => Some(AuthzMode::External),
            _ => None,
        }
    }

    pub fn is_external(&self) -> bool {
        matches!(self, AuthzMode::External)
    }
}

/// The policy row (seeded by migration 0005). Phase 2 reads the session +
/// password fields; Phase 4 reads `mfa_mode`; Phase 9 reads `auth_mode` +
/// `authz_mode`.
#[derive(Debug, Clone)]
pub struct AuthPolicy {
    pub global_default_session_secs: i64,
    pub global_max_session_secs: i64,
    pub access_token_secs: i64,
    pub password_min_length: i64,
    pub mfa_mode: MfaMode,
    pub auth_mode: AuthMode,
    pub authz_mode: AuthzMode,
}

impl AuthPolicy {
    pub async fn load(pool: &PgPool) -> Result<Self, hive_db::Error> {
        let row = sqlx::query_as::<_, (i32, i32, i32, i32, String, String, String)>(
            "SELECT global_default_session_secs, global_max_session_secs, \
             access_token_secs, password_min_length, mfa_mode, auth_mode, authz_mode \
             FROM auth_policy WHERE id = 1",
        )
        .fetch_one(pool)
        .await?;
        // Env overrides let an operator flip modes without a DB write when an
        // external IdP is wired. Each falls back to the stored row, then a safe
        // default (builtin/internal).
        let mfa_mode = std::env::var("HIVE_MFA_MODE")
            .ok()
            .and_then(|s| MfaMode::parse(&s))
            .or_else(|| MfaMode::parse(&row.4))
            .unwrap_or(MfaMode::Internal);
        let auth_mode = std::env::var("HIVE_AUTH_MODE")
            .ok()
            .and_then(|s| AuthMode::parse(&s))
            .or_else(|| AuthMode::parse(&row.5))
            .unwrap_or(AuthMode::Builtin);
        let authz_mode = std::env::var("HIVE_AUTHZ_MODE")
            .ok()
            .and_then(|s| AuthzMode::parse(&s))
            .or_else(|| AuthzMode::parse(&row.6))
            .unwrap_or(AuthzMode::Internal);
        Ok(AuthPolicy {
            global_default_session_secs: row.0 as i64,
            global_max_session_secs: row.1 as i64,
            access_token_secs: row.2 as i64,
            password_min_length: row.3 as i64,
            mfa_mode,
            auth_mode,
            authz_mode,
        })
    }

    /// Effective session lifetime for a user (§2):
    /// `min(user_override ?? global_default, global_max)`. The per-user override
    /// is re-clamped to the global max at issue time, so lowering the max later
    /// tightens everyone immediately.
    pub fn effective_session_secs(&self, user_override: Option<i64>) -> i64 {
        let base = user_override.unwrap_or(self.global_default_session_secs);
        base.min(self.global_max_session_secs).max(1)
    }

    /// Access-token TTL for a freshly minted token (§2): the access default,
    /// but never longer than the whole session.
    pub fn access_secs_within(&self, session_secs: i64) -> i64 {
        self.access_token_secs.min(session_secs).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> AuthPolicy {
        AuthPolicy {
            global_default_session_secs: 28800, // 8h
            global_max_session_secs: 86400,     // 24h
            access_token_secs: 600,             // 10m
            password_min_length: 14,
            mfa_mode: MfaMode::Internal,
            auth_mode: AuthMode::Builtin,
            authz_mode: AuthzMode::Internal,
        }
    }

    #[test]
    fn mfa_mode_parse_and_enforce() {
        assert_eq!(MfaMode::parse("internal"), Some(MfaMode::Internal));
        assert_eq!(MfaMode::parse("DELEGATED"), Some(MfaMode::Delegated));
        assert_eq!(MfaMode::parse("off"), Some(MfaMode::Off));
        assert_eq!(MfaMode::parse("bogus"), None);
        assert!(MfaMode::Internal.enforces_internal());
        assert!(!MfaMode::Delegated.enforces_internal());
        assert!(!MfaMode::Off.enforces_internal());
    }

    #[test]
    fn auth_and_authz_mode_parse() {
        // auth_mode accepts the design name + the lead's alias.
        assert_eq!(AuthMode::parse("builtin"), Some(AuthMode::Builtin));
        assert_eq!(AuthMode::parse("internal"), Some(AuthMode::Builtin));
        assert_eq!(AuthMode::parse("EXTERNAL"), Some(AuthMode::External));
        assert_eq!(AuthMode::parse("bogus"), None);
        assert!(AuthMode::External.is_external());
        assert!(!AuthMode::Builtin.is_external());

        assert_eq!(AuthzMode::parse("internal"), Some(AuthzMode::Internal));
        assert_eq!(AuthzMode::parse("external"), Some(AuthzMode::External));
        assert_eq!(AuthzMode::parse("bogus"), None);
        assert!(AuthzMode::External.is_external());
        assert!(!AuthzMode::Internal.is_external());
    }

    #[test]
    fn effective_session_uses_default_when_no_override() {
        assert_eq!(policy().effective_session_secs(None), 28800);
    }

    #[test]
    fn per_user_override_is_capped_at_global_max() {
        // user asks for 48h, max is 24h => clamped to 24h.
        assert_eq!(policy().effective_session_secs(Some(172800)), 86400);
    }

    #[test]
    fn per_user_override_below_max_is_honored() {
        assert_eq!(policy().effective_session_secs(Some(3600)), 3600);
    }

    #[test]
    fn access_ttl_never_exceeds_session() {
        // short 5-min session => access capped to 300, not the 600 default.
        assert_eq!(policy().access_secs_within(300), 300);
        // normal session => the 600 default applies.
        assert_eq!(policy().access_secs_within(28800), 600);
    }
}
