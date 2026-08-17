// Bearer tokens for programmatic clients (store.ts `tokens`).

use anyhow::Result;
use chrono::Utc;
use hive_shared::{
    is_ai, ActorKind, ApiToken, API_TOKEN_DEFAULT_EXPIRY_DAYS, API_TOKEN_MAX_EXPIRY_DAYS,
};
use serde_json::json;
use sqlx::Row;

use crate::auth::{
    generate_token, iso_in_days, iso_in_secs, token_hash, API_TOKEN_PREFIX,
    OAUTH_TOKEN_TTL_MAX_SECS, OAUTH_TOKEN_TTL_MIN_SECS, OAUTH_TOKEN_TTL_NEVER,
    OAUTH_TOKEN_TTL_SECS,
};

use super::{new_id, now_iso, Store};

const TOKEN_COLS: &str =
    "id, actor, label, created_by, created_at, last_used_at, kind, client_id, granted_by, expires_at, scope";

/// What a bearer token authenticates as. The `org` is the acting-org pin:
/// one org per credential, decided at mint time and never rewritten.
pub struct ResolvedToken {
    pub actor: String,
    pub namespace_user: String,
    pub org: Option<uuid::Uuid>,
}

impl Store {
    /// Every token minted in the ACTING org, newest first.
    ///
    /// `api_tokens` is on the tenancy plane and deliberately has no row
    /// policy: `tokens_resolve` below runs in the auth middleware, before an
    /// acting org exists, and a policy would make the credential unreadable
    /// by the lookup that discovers which org it pins. The price of that is
    /// that every OTHER query on this table has to carry the predicate itself
    /// — which this one did not, so one org's admin listed another org's
    /// tokens (id, actor, label, expiry) through `GET /api/tokens`.
    ///
    /// No acting org is deny: the predicate is NULL, and NULL is not true.
    pub async fn tokens_list(&self) -> Result<Vec<ApiToken>> {
        let rows = crate::pgq::query(&format!(
            "SELECT {TOKEN_COLS} FROM api_tokens WHERE org_id = {org} \
             ORDER BY created_at DESC",
            org = crate::db::ACTING_ORG
        ))
        .fetch_all(self.db())
        .await?;
        rows.iter().map(row_to_token).collect()
    }

    /// Mint a bearer token. `expires_in_days` is clamped to [1, MAX]; omitted →
    /// DEFAULT unless `never_expires` is true. The plaintext is returned once
    /// and never stored.
    pub async fn tokens_create(
        &self,
        actor: &str,
        label: &str,
        expires_in_days: Option<i64>,
        never_expires: bool,
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
        let expires_at = if never_expires {
            None
        } else {
            let requested = expires_in_days.unwrap_or(API_TOKEN_DEFAULT_EXPIRY_DAYS);
            let days = requested.clamp(1, API_TOKEN_MAX_EXPIRY_DAYS);
            Some(iso_in_days(days))
        };
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
            expires_at,
            scope: None,
        };
        crate::pgq::query(
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

    /// Mint a long-lived OAuth access token (consent flow). Plaintext returned
    /// once. `expires_in_secs=Some(0)` means never expires; any positive value
    /// is clamped to [MIN, MAX]; omitted → DEFAULT.
    pub async fn tokens_create_oauth(
        &self,
        actor: &str,
        client_id: &str,
        granted_by: &str,
        scope: &str,
        expires_in_secs: Option<i64>,
    ) -> Result<(String, ApiToken)> {
        let token = generate_token(API_TOKEN_PREFIX);
        let expires_at = match expires_in_secs {
            Some(OAUTH_TOKEN_TTL_NEVER) => None,
            Some(secs) => Some(iso_in_secs(
                secs.clamp(OAUTH_TOKEN_TTL_MIN_SECS, OAUTH_TOKEN_TTL_MAX_SECS),
            )),
            None => Some(iso_in_secs(OAUTH_TOKEN_TTL_SECS)),
        };
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
            expires_at,
            scope: Some(scope.to_string()),
        };
        crate::pgq::query(
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
    /// expiry (NULL = legacy non-expiring; past expiry → reject + reap). The
    /// namespace user is the human the token acts for — `granted_by` for OAuth
    /// tokens, else `created_by` — which keys per-user memory visibility (an AI
    /// sees the namespace of whoever granted its token).
    ///
    /// `org` is the one org this token may ever act in, fixed when it was
    /// minted. An agent authenticates into one org per session and cannot
    /// switch: nothing writes this column after the INSERT.
    pub async fn tokens_resolve(&self, token: &str) -> Result<Option<ResolvedToken>> {
        let row = crate::pgq::query(
            "SELECT id, actor, granted_by, created_by, org_id, expires_at \
             FROM api_tokens WHERE token_hash = ?",
        )
        .bind(token_hash(token))
        .fetch_optional(self.db())
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: String = row.try_get("id")?;
        let actor: String = row.try_get("actor")?;
        let granted_by: Option<String> = row.try_get("granted_by")?;
        let created_by: String = row.try_get("created_by")?;
        let namespace_user = granted_by.unwrap_or(created_by);
        let org: Option<uuid::Uuid> = row.try_get("org_id")?;
        let expires_at: Option<String> = row.try_get("expires_at")?;
        if let Some(exp) = expires_at {
            let expired = chrono::DateTime::parse_from_rfc3339(&exp)
                .map(|t| t.with_timezone(&Utc) < Utc::now())
                .unwrap_or(true);
            if expired {
                crate::pgq::query("DELETE FROM api_tokens WHERE id = ?")
                    .bind(&id)
                    .execute(self.db())
                    .await?;
                return Ok(None);
            }
        }
        // A token outlives its granting human's membership otherwise. Sessions
        // re-check this on every resolve (store/sessions.rs) and delete the
        // session when the membership is gone; a token that skipped the check
        // kept acting in an org its human had been removed from — which is the
        // revocation-only-stops-future-grants failure, in the credential that
        // lives longest.
        //
        // Only a namespace user that RESOLVES to a login account is checked.
        // `created_by` is also 'onboarding' or an operator label for tokens
        // minted outside any user's session, and those have no membership to
        // lose; revoking on a failed lookup would kill them at first use.
        if let Some(org_id) = org {
            if let Some(user) = self.users_by_actor(&namespace_user).await? {
                if self.membership_of(&user.id, org_id).await?.is_none() {
                    crate::pgq::query("DELETE FROM api_tokens WHERE id = ?")
                        .bind(&id)
                        .execute(self.db())
                        .await?;
                    return Ok(None);
                }
            }
        }
        crate::pgq::query("UPDATE api_tokens SET last_used_at = ? WHERE id = ?")
            .bind(now_iso())
            .bind(&id)
            .execute(self.db())
            .await?;
        Ok(Some(ResolvedToken {
            actor,
            namespace_user,
            org,
        }))
    }

    /// Revoke one token, if it belongs to the acting org. A token id from
    /// another org is `false` (→ 404), not a deletion: an admin in one org
    /// used to be able to revoke another org's agent credential by id, which
    /// is a denial-of-service across a tenancy boundary.
    pub async fn tokens_remove(&self, token_id: &str) -> Result<bool> {
        let res = crate::pgq::query(&format!(
            "DELETE FROM api_tokens WHERE id = ? AND org_id = {org}",
            org = crate::db::ACTING_ORG
        ))
        .bind(token_id)
        .execute(self.db())
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Revoke every token an OAuth client holds IN THE ACTING ORG.
    ///
    /// `oauth_clients` is instance-global (registration is unauthenticated by
    /// construction — RFC 7591 — so there is no org at that point), but the
    /// tokens minted for a client are not: each one pins the org of the human
    /// who consented. Revoking across all of them would let a disconnect in
    /// one org cut every other org's agents off from the same client.
    ///
    /// The replay case is handled where the org is actually known: see
    /// `oauth_codes_redeem`, which revokes in the REPLAYED CODE's org. The
    /// route calls this afterwards from an unauthenticated endpoint with no
    /// acting org, where it correctly does nothing.
    pub async fn tokens_revoke_by_client(&self, client_id: &str) -> Result<u64> {
        let res = crate::pgq::query(&format!(
            "DELETE FROM api_tokens WHERE client_id = ? AND org_id = {org}",
            org = crate::db::ACTING_ORG
        ))
        .bind(client_id)
        .execute(self.db())
        .await?;
        Ok(res.rows_affected())
    }
}

fn row_to_token(r: &sqlx::postgres::PgRow) -> Result<ApiToken> {
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
