// Bearer tokens for programmatic clients (store.ts `tokens`).

use anyhow::Result;
use chrono::Utc;
use hive_shared::{
    is_ai, ActorKind, ApiToken, API_TOKEN_DEFAULT_EXPIRY_DAYS, API_TOKEN_MAX_EXPIRY_DAYS,
};
use serde_json::json;
use sqlx::Row;

use crate::auth::{
    generate_token, iso_in_days, iso_in_secs, token_hash, API_TOKEN_PREFIX, OAUTH_TOKEN_TTL_SECS,
};

use super::{new_id, now_iso, Store};

const TOKEN_COLS: &str =
    "id, actor, label, created_by, created_at, last_used_at, kind, client_id, granted_by, expires_at, scope";

impl Store {
    pub async fn tokens_list(&self) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query(&format!(
            "SELECT {TOKEN_COLS} FROM api_tokens ORDER BY created_at DESC"
        ))
        .fetch_all(self.db())
        .await?;
        rows.iter().map(row_to_token).collect()
    }

    /// Mint a bearer token. `expires_in_days` is clamped to [1, MAX]; omitted →
    /// DEFAULT. The plaintext is returned once and never stored.
    pub async fn tokens_create(
        &self,
        actor: &str,
        label: &str,
        expires_in_days: Option<i64>,
        by: &str,
    ) -> Result<(String, ApiToken)> {
        let person = self
            .people_ensure(
                actor,
                if is_ai(actor) {
                    ActorKind::Ai
                } else {
                    ActorKind::Human
                },
            )
            .await?;
        let token = generate_token(API_TOKEN_PREFIX);
        let requested = expires_in_days.unwrap_or(API_TOKEN_DEFAULT_EXPIRY_DAYS);
        let days = requested.clamp(1, API_TOKEN_MAX_EXPIRY_DAYS);
        let record = ApiToken {
            id: new_id("tok"),
            actor: person.slug,
            label: label.to_string(),
            created_by: by.to_string(),
            created_at: now_iso(),
            last_used_at: None,
            kind: Some("pat".to_string()),
            client_id: None,
            granted_by: None,
            expires_at: Some(iso_in_days(days)),
            scope: None,
        };
        sqlx::query(
            "INSERT INTO api_tokens (id, token_hash, actor, label, created_by, created_at, last_used_at, kind, expires_at) \
             VALUES (?, ?, ?, ?, ?, ?, NULL, 'pat', ?)",
        )
        .bind(&record.id)
        .bind(token_hash(&token))
        .bind(&record.actor)
        .bind(&record.label)
        .bind(&record.created_by)
        .bind(&record.created_at)
        .bind(&record.expires_at)
        .execute(self.db())
        .await?;
        self.emit(
            "token.created",
            by,
            json!({"id": record.id, "actor": record.actor, "label": record.label, "expires_at": record.expires_at}),
        )
        .await?;
        Ok((token, record))
    }

    /// Mint a long-lived OAuth access token (consent flow). Plaintext returned once.
    pub async fn tokens_create_oauth(
        &self,
        actor: &str,
        client_id: &str,
        granted_by: &str,
        scope: &str,
    ) -> Result<(String, ApiToken)> {
        let token = generate_token(API_TOKEN_PREFIX);
        let record = ApiToken {
            id: new_id("tok"),
            actor: actor.to_string(),
            label: format!("oauth · {client_id}"),
            created_by: granted_by.to_string(),
            created_at: now_iso(),
            last_used_at: None,
            kind: Some("oauth".to_string()),
            client_id: Some(client_id.to_string()),
            granted_by: Some(granted_by.to_string()),
            expires_at: Some(iso_in_secs(OAUTH_TOKEN_TTL_SECS)),
            scope: Some(scope.to_string()),
        };
        sqlx::query(
            "INSERT INTO api_tokens (id, token_hash, actor, label, created_by, created_at, last_used_at, kind, client_id, granted_by, expires_at, scope) \
             VALUES (?, ?, ?, ?, ?, ?, NULL, 'oauth', ?, ?, ?, ?)",
        )
        .bind(&record.id)
        .bind(token_hash(&token))
        .bind(&record.actor)
        .bind(&record.label)
        .bind(&record.created_by)
        .bind(&record.created_at)
        .bind(&record.client_id)
        .bind(&record.granted_by)
        .bind(&record.expires_at)
        .bind(&record.scope)
        .execute(self.db())
        .await?;
        self.emit(
            "token.granted",
            granted_by,
            json!({"id": record.id, "actor": record.actor, "client_id": client_id}),
        )
        .await?;
        Ok((token, record))
    }

    /// Resolve a bearer token to its actor (and stamp last_used), honoring
    /// expiry (NULL = legacy non-expiring; past expiry → reject + reap).
    pub async fn tokens_resolve(&self, token: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id, actor, expires_at FROM api_tokens WHERE token_hash = ?")
            .bind(token_hash(token))
            .fetch_optional(self.db())
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: String = row.try_get("id")?;
        let actor: String = row.try_get("actor")?;
        let expires_at: Option<String> = row.try_get("expires_at")?;
        if let Some(exp) = expires_at {
            let expired = chrono::DateTime::parse_from_rfc3339(&exp)
                .map(|t| t.with_timezone(&Utc) < Utc::now())
                .unwrap_or(true);
            if expired {
                sqlx::query("DELETE FROM api_tokens WHERE id = ?")
                    .bind(&id)
                    .execute(self.db())
                    .await?;
                return Ok(None);
            }
        }
        sqlx::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
            .bind(now_iso())
            .bind(&id)
            .execute(self.db())
            .await?;
        Ok(Some(actor))
    }

    pub async fn tokens_remove(&self, token_id: &str) -> Result<bool> {
        let res = sqlx::query("DELETE FROM api_tokens WHERE id = ?")
            .bind(token_id)
            .execute(self.db())
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Revoke every token minted by a given OAuth client_id (used on code replay).
    pub async fn tokens_revoke_by_client(&self, client_id: &str) -> Result<u64> {
        let res = sqlx::query("DELETE FROM api_tokens WHERE client_id = ?")
            .bind(client_id)
            .execute(self.db())
            .await?;
        Ok(res.rows_affected())
    }
}

fn row_to_token(r: &sqlx::sqlite::SqliteRow) -> Result<ApiToken> {
    Ok(ApiToken {
        id: r.try_get("id")?,
        actor: r.try_get("actor")?,
        label: r.try_get("label")?,
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
        last_used_at: r.try_get("last_used_at")?,
        kind: r.try_get("kind")?,
        client_id: r.try_get("client_id")?,
        granted_by: r.try_get("granted_by")?,
        expires_at: r.try_get("expires_at")?,
        scope: r.try_get("scope")?,
    })
}
