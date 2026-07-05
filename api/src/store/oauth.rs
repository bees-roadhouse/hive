// OAuth 2.1 authorization server: clients + codes (store.ts `oauthClients`,
// `oauthCodes`).

use anyhow::Result;
use chrono::Utc;
use hive_shared::{OAuthClient, OAuthClientStatus};
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
    /// Requested access-token lifetime (seconds); None → server default.
    pub token_ttl_secs: Option<i64>,
}

pub enum RedeemOutcome {
    Ok(AuthCodeGrant),
    Replay { client_id: String },
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
        crate::pgq::query(
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
        let row = crate::pgq::query("SELECT * FROM oauth_clients WHERE client_id = ?")
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
        Ok(
            crate::pgq::query_scalar("SELECT COUNT(*) FROM oauth_clients")
                .fetch_one(self.db())
                .await?,
        )
    }

    /// List every OAuth client with its live token stats: a count of currently
    /// active (non-expired) oauth tokens and the most-recent `last_used_at`. A
    /// token is active when `expires_at` is NULL (legacy) or still in the future.
    pub async fn oauth_clients_list(&self) -> Result<Vec<OAuthClientStatus>> {
        let now = now_iso();
        let rows = crate::pgq::query(
            "SELECT c.client_id, c.client_name, c.created_at, \
                    COUNT(t.id) FILTER (WHERE t.expires_at IS NULL OR t.expires_at > ?) AS active_tokens, \
                    MAX(t.last_used_at) AS last_used_at \
             FROM oauth_clients c \
             LEFT JOIN api_tokens t ON t.client_id = c.client_id AND t.kind = 'oauth' \
             GROUP BY c.client_id, c.client_name, c.created_at \
             ORDER BY c.created_at DESC",
        )
        .bind(&now)
        .fetch_all(self.db())
        .await?;
        rows.iter()
            .map(|r| {
                Ok(OAuthClientStatus {
                    client_id: r.try_get("client_id")?,
                    client_name: r.try_get("client_name")?,
                    created_at: r.try_get("created_at")?,
                    active_tokens: r.try_get("active_tokens")?,
                    last_used_at: r.try_get("last_used_at")?,
                })
            })
            .collect()
    }

    /// Issue a single-use auth code (60s TTL); returns the plaintext code.
    pub async fn oauth_codes_create(&self, grant: &AuthCodeGrant) -> Result<String> {
        let code = generate_token(AUTH_CODE_PREFIX);
        crate::pgq::query(
            "INSERT INTO oauth_auth_codes (code_hash, client_id, redirect_uri, code_challenge, ai_actor, granted_by, scope, created_at, expires_at, used_at, token_ttl_secs) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
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
        .bind(grant.token_ttl_secs)
        .execute(self.db())
        .await?;
        Ok(code)
    }

    /// Single-use redemption under a transaction. Marks the code used on success.
    pub async fn oauth_codes_redeem(&self, code: &str) -> Result<RedeemOutcome> {
        // Opportunistic sweep of expired codes.
        crate::pgq::query("DELETE FROM oauth_auth_codes WHERE expires_at < ?")
            .bind(now_iso())
            .execute(self.db())
            .await?;

        let mut tx = self.db().begin().await?;
        let row = crate::pgq::query("SELECT * FROM oauth_auth_codes WHERE code_hash = ?")
            .bind(token_hash(code))
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Ok(RedeemOutcome::Unknown);
        };
        let used_at: Option<String> = row.try_get("used_at")?;
        if used_at.is_some() {
            return Ok(RedeemOutcome::Replay {
                client_id: row.try_get("client_id")?,
            });
        }
        let expires_at: String = row.try_get("expires_at")?;
        let expired = chrono::DateTime::parse_from_rfc3339(&expires_at)
            .map(|t| t.with_timezone(&Utc) < Utc::now())
            .unwrap_or(true);
        if expired {
            return Ok(RedeemOutcome::Expired);
        }
        crate::pgq::query("UPDATE oauth_auth_codes SET used_at = ? WHERE code_hash = ?")
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
            token_ttl_secs: row.try_get("token_ttl_secs")?,
        }))
    }
}
