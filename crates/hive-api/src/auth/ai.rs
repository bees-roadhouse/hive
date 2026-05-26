//! AI identities, access grants, MCP sessions, and revocation queries
//! (hive-auth-mcp-design.md §1.5, §3.4, §5.5).
//!
//! This is the Phase-6 store layer: the DB side of AI principals. Runtime sqlx
//! only (no compile-time macros), matching the rest of hive-api so it builds
//! without a live DB. hive-db owns the schema (migration 0006).

use chrono::{DateTime, Utc};
use hive_db::PgPool;
use uuid::Uuid;

use super::store::StoreError;

/// An AI identity row (§1.5).
#[derive(Debug, Clone)]
pub struct AiIdentity {
    pub id: Uuid,
    pub name: String,
    pub display_name: Option<String>,
    pub kind: String,
    pub owned_by: Uuid,
    pub status: String,
    pub created_at: Option<DateTime<Utc>>,
}

/// A per-(AI, user) access grant (§3.4).
#[derive(Debug, Clone)]
pub struct AiAccessGrant {
    pub id: Uuid,
    pub ai_id: Uuid,
    pub user_id: Uuid,
    pub granted_scopes: Vec<String>,
    pub data_visibility: String,
    pub mcp_token_no_expiry: bool,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// Create an AI identity owned by `owned_by`. Returns the new id.
pub async fn create_ai_identity(
    pool: &PgPool,
    name: &str,
    display_name: Option<&str>,
    kind: &str,
    owned_by: Uuid,
) -> Result<Uuid, StoreError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ai_identities (name, display_name, kind, owned_by) \
         VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(name)
    .bind(display_name)
    .bind(kind)
    .bind(owned_by)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// List AI identities. When `owner` is `Some`, only those owned by that user.
pub async fn list_ai_identities(
    pool: &PgPool,
    owner: Option<Uuid>,
) -> Result<Vec<AiIdentity>, StoreError> {
    let rows = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Option<String>,
            String,
            Uuid,
            String,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT id, name, display_name, kind, owned_by, status, created_at \
         FROM ai_identities WHERE ($1::uuid IS NULL OR owned_by = $1) ORDER BY name",
    )
    .bind(owner)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| AiIdentity {
            id: r.0,
            name: r.1,
            display_name: r.2,
            kind: r.3,
            owned_by: r.4,
            status: r.5,
            created_at: r.6,
        })
        .collect())
}

/// Fetch one AI identity by handle (name).
pub async fn find_ai_by_name(pool: &PgPool, name: &str) -> Result<Option<AiIdentity>, StoreError> {
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Option<String>,
            String,
            Uuid,
            String,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT id, name, display_name, kind, owned_by, status, created_at \
         FROM ai_identities WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AiIdentity {
        id: r.0,
        name: r.1,
        display_name: r.2,
        kind: r.3,
        owned_by: r.4,
        status: r.5,
        created_at: r.6,
    }))
}

/// Upsert a per-(AI, user) grant (§3.4). Re-granting the same pair updates the
/// scopes/visibility/expiry and clears any prior `revoked_at`. Returns the id.
pub async fn upsert_grant(
    pool: &PgPool,
    ai_id: Uuid,
    user_id: Uuid,
    granted_scopes: &[String],
    data_visibility: &str,
    mcp_token_no_expiry: bool,
) -> Result<Uuid, StoreError> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO ai_access_grants \
           (ai_id, user_id, granted_scopes, data_visibility, mcp_token_no_expiry) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (ai_id, user_id) DO UPDATE SET \
           granted_scopes = EXCLUDED.granted_scopes, \
           data_visibility = EXCLUDED.data_visibility, \
           mcp_token_no_expiry = EXCLUDED.mcp_token_no_expiry, \
           revoked_at = NULL \
         RETURNING id",
    )
    .bind(ai_id)
    .bind(user_id)
    .bind(granted_scopes)
    .bind(data_visibility)
    .bind(mcp_token_no_expiry)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Fetch the active (non-revoked) grant for a (AI, user) pair — the one applied
/// at MCP-token-issue time.
pub async fn active_grant(
    pool: &PgPool,
    ai_id: Uuid,
    user_id: Uuid,
) -> Result<Option<AiAccessGrant>, StoreError> {
    let row = sqlx::query_as::<_, (Uuid, Uuid, Uuid, Vec<String>, String, bool, Option<DateTime<Utc>>)>(
        "SELECT id, ai_id, user_id, granted_scopes, data_visibility, mcp_token_no_expiry, revoked_at \
         FROM ai_access_grants WHERE ai_id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(ai_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AiAccessGrant {
        id: r.0,
        ai_id: r.1,
        user_id: r.2,
        granted_scopes: r.3,
        data_visibility: r.4,
        mcp_token_no_expiry: r.5,
        revoked_at: r.6,
    }))
}

/// Mark a grant revoked (the reconfig→revoke and explicit grant-revoke paths).
pub async fn revoke_grant(pool: &PgPool, ai_id: Uuid, user_id: Uuid) -> Result<(), StoreError> {
    sqlx::query(
        "UPDATE ai_access_grants SET revoked_at = now() \
         WHERE ai_id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(ai_id)
    .bind(user_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// A created MCP/AI session + its token jti (§3.4, §5.5). The jti is the
/// revocation handle and is stored on the session row + carried in the token.
pub struct IssuedMcpSession {
    pub session_id: Uuid,
    pub jti: Uuid,
}

/// Create an `mcp_ai` session row for a freshly issued MCP token. `expires_at`
/// is `None` for the non-expiring class (the default). Records the issuing jti
/// so revocation by session can find it.
pub async fn create_mcp_session(
    pool: &PgPool,
    ai_id: Uuid,
    act_user_id: Uuid,
    client_id: &str,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
) -> Result<IssuedMcpSession, StoreError> {
    let jti = Uuid::now_v7();
    let session_id: Uuid = sqlx::query_scalar(
        "INSERT INTO sessions (kind, ai_id, act_user_id, client_id, scopes, amr, expires_at, jti, last_seen_at) \
         VALUES ('mcp_ai', $1, $2, $3, $4, $5, $6, $7, now()) RETURNING id",
    )
    .bind(ai_id)
    .bind(act_user_id)
    .bind(client_id)
    .bind(scopes)
    // amr is the connecting human's auth methods; threaded from the human's
    // session in a fuller flow. Phase 6 records the connect happened via pwd.
    .bind(vec!["pwd".to_string()])
    .bind(expires_at)
    .bind(jti)
    .fetch_one(pool)
    .await?;
    Ok(IssuedMcpSession { session_id, jti })
}

/// One active MCP connection, for the listing surfaces (§5.5).
#[derive(Debug, Clone)]
pub struct AiConnection {
    pub session_id: Uuid,
    pub jti: Option<Uuid>,
    pub ai_id: Uuid,
    pub ai_name: String,
    pub act_user_id: Uuid,
    pub scopes: Vec<String>,
    pub client_id: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
}

/// Common SELECT for the connection listings: active (non-revoked, unexpired)
/// `mcp_ai` sessions joined to the AI handle.
const CONNECTIONS_SELECT: &str = "SELECT s.id, s.jti, s.ai_id, a.name, s.act_user_id, s.scopes, \
     s.client_id, s.expires_at, s.last_seen_at, s.created_at \
     FROM sessions s JOIN ai_identities a ON a.id = s.ai_id \
     WHERE s.kind = 'mcp_ai' AND s.revoked_at IS NULL \
       AND (s.expires_at IS NULL OR s.expires_at > now())";

type ConnRow = (
    Uuid,
    Option<Uuid>,
    Uuid,
    String,
    Uuid,
    Vec<String>,
    String,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
    Option<DateTime<Utc>>,
);

fn conn_from_row(r: ConnRow) -> AiConnection {
    AiConnection {
        session_id: r.0,
        jti: r.1,
        ai_id: r.2,
        ai_name: r.3,
        act_user_id: r.4,
        scopes: r.5,
        client_id: r.6,
        expires_at: r.7,
        last_seen_at: r.8,
        created_at: r.9,
    }
}

/// Connections a human is the actor on (`GET /me/ai-connections`, §5.5).
pub async fn connections_for_actor(
    pool: &PgPool,
    act_user_id: Uuid,
) -> Result<Vec<AiConnection>, StoreError> {
    let q = format!("{CONNECTIONS_SELECT} AND s.act_user_id = $1 ORDER BY s.created_at DESC");
    let rows = sqlx::query_as::<_, ConnRow>(&q)
        .bind(act_user_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(conn_from_row).collect())
}

/// All connections for one AI (`GET /ai/{handle}/connections`, §5.5).
pub async fn connections_for_ai(
    pool: &PgPool,
    ai_id: Uuid,
) -> Result<Vec<AiConnection>, StoreError> {
    let q = format!("{CONNECTIONS_SELECT} AND s.ai_id = $1 ORDER BY s.created_at DESC");
    let rows = sqlx::query_as::<_, ConnRow>(&q)
        .bind(ai_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(conn_from_row).collect())
}

/// Every active MCP connection (`GET /admin/ai-connections`, §5.5).
pub async fn all_connections(pool: &PgPool) -> Result<Vec<AiConnection>, StoreError> {
    let q = format!("{CONNECTIONS_SELECT} ORDER BY s.created_at DESC");
    let rows = sqlx::query_as::<_, ConnRow>(&q).fetch_all(pool).await?;
    Ok(rows.into_iter().map(conn_from_row).collect())
}

/// The five revocation scopes (§5.5). Each marks the matching `mcp_ai` sessions
/// revoked and writes `revocations` rows for their jtis, returning the revoked
/// jtis so the caller can update the in-memory set. All run in one transaction.
#[derive(Debug, Clone)]
pub enum RevokeScope {
    /// #1 single token by jti.
    Jti(Uuid),
    /// #2 one AI's tokens (all actors). "Disconnect pia everywhere."
    Ai(Uuid),
    /// #3 per-(AI, owner): only the tokens a specific human lit up for an AI.
    AiActor { ai_id: Uuid, act_user_id: Uuid },
    /// #4 per-human: every AI token a human is the actor on.
    Actor(Uuid),
    /// #5 global kill-switch: all mcp_ai sessions.
    All,
}

/// Apply a revocation scope. `revoked_by` is the acting human/admin (audit).
/// Returns the jtis that were revoked (to feed the in-memory set).
pub async fn revoke(
    pool: &PgPool,
    scope: &RevokeScope,
    revoked_by: Uuid,
    reason: &str,
) -> Result<Vec<Uuid>, StoreError> {
    // Build the WHERE that selects the target live mcp_ai sessions.
    let (where_sql, binds): (&str, Vec<Uuid>) = match scope {
        RevokeScope::Jti(jti) => ("s.jti = $1", vec![*jti]),
        RevokeScope::Ai(ai) => ("s.ai_id = $1", vec![*ai]),
        RevokeScope::AiActor { ai_id, act_user_id } => (
            "s.ai_id = $1 AND s.act_user_id = $2",
            vec![*ai_id, *act_user_id],
        ),
        RevokeScope::Actor(human) => ("s.act_user_id = $1", vec![*human]),
        RevokeScope::All => ("TRUE", vec![]),
    };

    let mut tx = pool.begin().await?;

    // Select the targets first (need their jti + subject for the revocations rows).
    let select = format!(
        "SELECT s.id, s.jti, s.ai_id, s.act_user_id FROM sessions s \
         WHERE s.kind = 'mcp_ai' AND s.revoked_at IS NULL AND ({where_sql})"
    );
    let mut q = sqlx::query_as::<_, (Uuid, Option<Uuid>, Uuid, Option<Uuid>)>(&select);
    for b in &binds {
        q = q.bind(*b);
    }
    let targets = q.fetch_all(&mut *tx).await?;

    let mut revoked_jtis = Vec::new();
    for (session_id, jti, ai_id, act_user_id) in targets {
        sqlx::query("UPDATE sessions SET revoked_at = now(), revoked_by = $2, revoke_reason = $3 WHERE id = $1")
            .bind(session_id)
            .bind(revoked_by)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
        if let Some(jti) = jti {
            sqlx::query(
                "INSERT INTO revocations (jti, session_id, subject_id, act_user_id, revoked_by, reason) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (jti) DO NOTHING",
            )
            .bind(jti)
            .bind(session_id)
            .bind(ai_id)
            .bind(act_user_id)
            .bind(revoked_by)
            .bind(reason)
            .execute(&mut *tx)
            .await?;
            revoked_jtis.push(jti);
        }
    }

    tx.commit().await?;
    Ok(revoked_jtis)
}
