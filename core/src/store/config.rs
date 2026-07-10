// Per-instance key/value config (store.ts `config`). Writes are `config.set`
// records; reads are index SQL.

use anyhow::Result;
use rusqlite::OptionalExtension;
use serde_json::json;

use super::{now_iso, Draft, Store};

impl Store {
    pub async fn config_get(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT value FROM config WHERE key = ?1",
                    rusqlite::params![key],
                    |r| r.get(0),
                )
                .optional()?)
        })
        .await
    }

    pub async fn config_set(&self, key: &str, value: &str) -> Result<()> {
        let (key, value) = (key.to_string(), value.to_string());
        self.run(move |core| {
            core.commit(vec![Draft::new(
                crate::oplog::kind::CONFIG_SET,
                "system",
                &now_iso(),
                json!({"key": key, "value": value}),
            )])
        })
        .await
    }

    pub async fn config_bool(&self, key: &str) -> Result<bool> {
        Ok(self.config_get(key).await?.as_deref() == Some("true"))
    }
}
