//! Auth policy + the session-lifetime math (hive-auth-mcp-design.md §2, §5).
//!
//! The single-row `auth_policy` table holds the global knobs; this module loads
//! it and computes the effective session/access lifetimes for a login.

use hive_db::PgPool;

/// The policy row (seeded by migration 0005). Phase 2 reads the session +
/// password fields; the mode fields are carried for Phase 4/9.
#[derive(Debug, Clone)]
pub struct AuthPolicy {
    pub global_default_session_secs: i64,
    pub global_max_session_secs: i64,
    pub access_token_secs: i64,
    pub password_min_length: i64,
}

impl AuthPolicy {
    pub async fn load(pool: &PgPool) -> Result<Self, hive_db::Error> {
        let row = sqlx::query_as::<_, (i32, i32, i32, i32)>(
            "SELECT global_default_session_secs, global_max_session_secs, \
             access_token_secs, password_min_length FROM auth_policy WHERE id = 1",
        )
        .fetch_one(pool)
        .await?;
        Ok(AuthPolicy {
            global_default_session_secs: row.0 as i64,
            global_max_session_secs: row.1 as i64,
            access_token_secs: row.2 as i64,
            password_min_length: row.3 as i64,
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
        }
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
