// Per-instance key/value config (store.ts `config`).

use anyhow::Result;

use super::{now_iso, Store};

impl Store {
    pub async fn config_get(&self, key: &str) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT value FROM config WHERE key = ?")
            .bind(key)
            .fetch_optional(self.db())
            .await?)
    }

    pub async fn config_set(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO config (key, value, updated_at) VALUES (?, ?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(key)
        .bind(value)
        .bind(now_iso())
        .execute(self.db())
        .await?;
        Ok(())
    }

    pub async fn config_bool(&self, key: &str) -> Result<bool> {
        Ok(self.config_get(key).await?.as_deref() == Some("true"))
    }
}
