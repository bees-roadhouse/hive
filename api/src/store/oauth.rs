// OAuth 2.1 authorization server: clients + codes (store.ts `oauthClients`,
// `oauthCodes`).

use anyhow::Result;
use chrono::Utc;
use hive_shared::OAuthClient;
use sqlx::Row;

use crate::auth::{generate_token, iso_in_secs, token_hash, AUTH_CODE_PREFIX, AUTH_CODE_TTL_SECS};

use super::{new_id, now_iso, Store};

#[derive(Debug, Clone)]
pub struct AuthCodeGrant {
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub ai_actor: String,
    pub granted_by: String,
    pub scope: String,
}

pub enum RedeemOutcome {
    Ok(AuthCodeGrant),
    Replay,
    Expired,
    Unknown,
}

impl Store {
    pub async fn oauth_clients_register(
        &self,
        client_name: &str,
        redirect_uris: &[String],
    ) -> Result<OAuthClient> {
        let client = OAuthClient {
            client_id: new_id("oauthc"),
            client_name: client_name.to_string(),
            redirect_uris: redirect_uris.to_vec(),
            grant_types: vec!["authorization_code".to_string()],
            created_at: now_iso(),
        };
        sqlx::query(
            "INSERT INTO oauth_clients (client_id, client_name, redirect_uris, grant_types, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&client.client_id)
        .bind(&client.client_name)
        .bind(serde_json::to_string(&client.redirect_uris)?)
        .bind(serde_json::to_string(&client.grant_types)?)
        .bind(&client.created_at)
        .execute(self.db())
        .await?;
        Ok(client)
    }

    pub async fn oauth_clients_get(&self, client_id: &str) -> Result<Option<OAuthClient>> {
        let row = sqlx::query("SELECT * FROM oauth_clients WHERE client_id = ?")
            .bind(client_id)
            .fetch_optional(self.db())
            .await?;
        row.map(|r| {
            Ok(OAuthClient {
                client_id: r.try_get("client_id")?,
                client_name: r.try_get("client_name")?,
                redirect_uris: serde_json::from_str(
                    r.try_get::<String, _>("redirect_uris")?.as_str(),
                )?,
                grant_types: serde_json::from_str(r.try_get::<String, _>("grant_types")?.as_str())?,
                created_at: r.try_get("created_at")?,
            })
        })
        .transpose()
    }

    pub async fn oauth_clients_count(&self) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT COUNT(*) FROM oauth_clients")
            .fetch_one(self.db())
            .await?)
    }

    /// Issue a single-use auth code (60s TTL); returns the plaintext code.
    pub async fn oauth_codes_create(&self, grant: &AuthCodeGrant) -> Result<String> {
        let code = generate_token(AUTH_CODE_PREFIX);
        sqlx::query(
            "INSERT INTO oauth_auth_codes (code_hash, client_id, redirect_uri, code_challenge, ai_actor, granted_by, scope, created_at, expires_at, used_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
        )
        .bind(token_hash(&code))
        .bind(&grant.client_id)
        .bind(&grant.redirect_uri)
        .bind(&grant.code_challenge)
        .bind(&grant.ai_actor)
        .bind(&grant.granted_by)
        .bind(&grant.scope)
        .bind(now_iso())
        .bind(iso_in_secs(AUTH_CODE_TTL_SECS))
        .execute(self.db())
        .await?;
        Ok(code)
    }

    /// Single-use redemption under a transaction. Marks the code used on success.
    pub async fn oauth_codes_redeem(&self, code: &str) -> Result<RedeemOutcome> {
        // Opportunistic sweep of expired codes.
        sqlx::query("DELETE FROM oauth_auth_codes WHERE expires_at < ?")
            .bind(now_iso())
            .execute(self.db())
            .await?;

        let mut tx = self.db().begin().await?;
        let row = sqlx::query("SELECT * FROM oauth_auth_codes WHERE code_hash = ?")
            .bind(token_hash(code))
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Ok(RedeemOutcome::Unknown);
        };
        let used_at: Option<String> = row.try_get("used_at")?;
        if used_at.is_some() {
            return Ok(RedeemOutcome::Replay);
        }
        let expires_at: String = row.try_get("expires_at")?;
        let expired = chrono::DateTime::parse_from_rfc3339(&expires_at)
            .map(|t| t.with_timezone(&Utc) < Utc::now())
            .unwrap_or(true);
        if expired {
            return Ok(RedeemOutcome::Expired);
        }
        sqlx::query("UPDATE oauth_auth_codes SET used_at = ? WHERE code_hash = ?")
            .bind(now_iso())
            .bind(token_hash(code))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(RedeemOutcome::Ok(AuthCodeGrant {
            client_id: row.try_get("client_id")?,
            redirect_uri: row.try_get("redirect_uri")?,
            code_challenge: row.try_get("code_challenge")?,
            ai_actor: row.try_get("ai_actor")?,
            granted_by: row.try_get("granted_by")?,
            scope: row.try_get("scope")?,
        }))
    }
}
