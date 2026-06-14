// Browser sessions, cookie auth (store.ts `sessions`).

use anyhow::Result;
use chrono::Utc;
use hive_shared::User;
use sqlx::Row;

use crate::auth::{generate_token, iso_in_secs, token_hash, SESSION_PREFIX, SESSION_TTL_SECS};

use super::{new_id, now_iso, Store};

impl Store {
    /// Create a session; returns the plaintext cookie value (hash stored).
    pub async fn sessions_create(&self, user_id: &str) -> Result<String> {
        let token = generate_token(SESSION_PREFIX);
        let ts = now_iso();
        crate::pgq::query(
            "INSERT INTO sessions (id, token_hash, user_id, created_at, expires_at, last_seen) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(new_id("ses"))
        .bind(token_hash(&token))
        .bind(user_id)
        .bind(&ts)
        .bind(iso_in_secs(SESSION_TTL_SECS))
        .bind(&ts)
        .execute(self.db())
        .await?;
        Ok(token)
    }

    /// Resolve a session cookie to its user, or None if missing/expired.
    pub async fn sessions_resolve(&self, token: &str) -> Result<Option<User>> {
        let row =
            crate::pgq::query("SELECT id, user_id, expires_at FROM sessions WHERE token_hash = ?")
                .bind(token_hash(token))
                .fetch_optional(self.db())
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let session_id: String = row.try_get("id")?;
        let user_id: String = row.try_get("user_id")?;
        let expires_at: String = row.try_get("expires_at")?;
        let expired = chrono::DateTime::parse_from_rfc3339(&expires_at)
            .map(|t| t.with_timezone(&Utc) < Utc::now())
            .unwrap_or(true);
        if expired {
            crate::pgq::query("DELETE FROM sessions WHERE id = ?")
                .bind(&session_id)
                .execute(self.db())
                .await?;
            return Ok(None);
        }
        crate::pgq::query("UPDATE sessions SET last_seen = ? WHERE id = ?")
            .bind(now_iso())
            .bind(&session_id)
            .execute(self.db())
            .await?;
        self.users_by_id(&user_id).await
    }

    pub async fn sessions_destroy(&self, token: &str) -> Result<()> {
        crate::pgq::query("DELETE FROM sessions WHERE token_hash = ?")
            .bind(token_hash(token))
            .execute(self.db())
            .await?;
        Ok(())
    }
}
