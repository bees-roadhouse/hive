//! DB access for the AS core (hive-auth-mcp-design.md §8 Phase 2, §5).
//!
//! Users, password credentials, sessions, rotating refresh tokens, and the
//! short-lived authorization-code state. hive-db owns the schema + migrations;
//! these are the hive-api-side queries against the Phase-2 tables.

use chrono::{DateTime, Utc};
use hive_db::PgPool;
use uuid::Uuid;

use super::tokens::{self, RefreshToken};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Db(#[from] hive_db::Error),
    #[error("sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("refresh token reuse detected; chain revoked")]
    RefreshReuse,
    #[error("refresh token not found or expired")]
    RefreshInvalid,
}

/// A user row (the columns the AS needs at login + mint time).
#[derive(Debug, Clone)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub is_admin: bool,
    pub granted_scopes: Vec<String>,
    pub session_lifetime_secs: Option<i64>,
    pub status: String,
}

/// Look up an active user by username (login path).
pub async fn find_user_by_username(
    pool: &PgPool,
    username: &str,
) -> Result<Option<User>, StoreError> {
    let row = sqlx::query_as::<_, (Uuid, String, bool, Vec<String>, Option<i32>, String)>(
        "SELECT id, username, is_admin, granted_scopes, session_lifetime_secs, status \
         FROM users WHERE username = $1",
    )
    .bind(username)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| User {
        id: r.0,
        username: r.1,
        is_admin: r.2,
        granted_scopes: r.3,
        session_lifetime_secs: r.4.map(|v| v as i64),
        status: r.5,
    }))
}

/// Look up an active user by id (MFA enrollment needs the username for the
/// otpauth:// label). Returns the same shape as the username lookup.
pub async fn find_user_by_id(pool: &PgPool, id: Uuid) -> Result<Option<User>, StoreError> {
    let row = sqlx::query_as::<_, (Uuid, String, bool, Vec<String>, Option<i32>, String)>(
        "SELECT id, username, is_admin, granted_scopes, session_lifetime_secs, status \
         FROM users WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| User {
        id: r.0,
        username: r.1,
        is_admin: r.2,
        granted_scopes: r.3,
        session_lifetime_secs: r.4.map(|v| v as i64),
        status: r.5,
    }))
}

/// Fetch the argon2id PHC string for a user, if they have a password credential.
pub async fn password_hash_for(pool: &PgPool, user_id: Uuid) -> Result<Option<String>, StoreError> {
    let row = sqlx::query_as::<_, (String,)>(
        "SELECT argon2_hash FROM password_credentials WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| r.0))
}

/// Count users — used by the bootstrap path to decide whether the first user
/// should be auto-admin.
pub async fn user_count(pool: &PgPool) -> Result<i64, StoreError> {
    let row = sqlx::query_as::<_, (i64,)>("SELECT count(*) FROM users")
        .fetch_one(pool)
        .await?;
    Ok(row.0)
}

/// Create a user + password credential in one transaction (bootstrap / admin
/// create). `is_admin` + `granted_scopes` set the user's authority. Returns the
/// new user id.
pub async fn create_user(
    pool: &PgPool,
    username: &str,
    argon2_hash: &str,
    is_admin: bool,
    granted_scopes: &[String],
) -> Result<Uuid, StoreError> {
    let mut tx = pool.begin().await?;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (username, is_admin, granted_scopes) VALUES ($1, $2, $3) RETURNING id",
    )
    .bind(username)
    .bind(is_admin)
    .bind(granted_scopes)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO password_credentials (user_id, argon2_hash) VALUES ($1, $2)")
        .bind(id)
        .bind(argon2_hash)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}

/// A short-lived authorization code (PKCE auth-code flow). Inserted by
/// `/authorize`, consumed by `/token`.
pub struct NewAuthCode<'a> {
    pub code: &'a str,
    pub client_id: &'a str,
    pub user_id: Uuid,
    pub redirect_uri: &'a str,
    pub code_challenge: &'a str,
    pub scopes: &'a [String],
    pub resource: Option<&'a str>,
    pub expires_at: DateTime<Utc>,
    /// Auth methods established at /authorize (e.g. ["pwd"] or ["pwd","otp"]),
    /// carried through to the session + token (§4).
    pub amr: &'a [String],
}

pub async fn insert_auth_code(pool: &PgPool, c: &NewAuthCode<'_>) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO authorization_codes \
         (code, client_id, user_id, redirect_uri, code_challenge, scopes, resource, expires_at, amr) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(c.code)
    .bind(c.client_id)
    .bind(c.user_id)
    .bind(c.redirect_uri)
    .bind(c.code_challenge)
    .bind(c.scopes)
    .bind(c.resource)
    .bind(c.expires_at)
    .bind(c.amr)
    .execute(pool)
    .await?;
    Ok(())
}

/// A consumed authorization code's contents.
pub struct AuthCodeRow {
    pub client_id: String,
    pub user_id: Uuid,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub scopes: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub amr: Vec<String>,
}

/// Atomically consume (delete + return) an authorization code. Single-use:
/// the DELETE ... RETURNING guarantees a code can't be redeemed twice.
pub async fn consume_auth_code(
    pool: &PgPool,
    code: &str,
) -> Result<Option<AuthCodeRow>, StoreError> {
    let row = sqlx::query_as::<
        _,
        (
            String,
            Uuid,
            String,
            String,
            Vec<String>,
            DateTime<Utc>,
            Vec<String>,
        ),
    >(
        "DELETE FROM authorization_codes WHERE code = $1 \
             RETURNING client_id, user_id, redirect_uri, code_challenge, scopes, expires_at, amr",
    )
    .bind(code)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| AuthCodeRow {
        client_id: r.0,
        user_id: r.1,
        redirect_uri: r.2,
        code_challenge: r.3,
        scopes: r.4,
        expires_at: r.5,
        amr: r.6,
    }))
}

/// A created session + its first refresh token.
pub struct IssuedSession {
    pub session_id: Uuid,
    pub refresh: RefreshToken,
    pub session_expires_at: DateTime<Utc>,
}

/// Create a session and its initial refresh token (hashed). `session_secs` is
/// the effective session lifetime from policy; the refresh TTL equals it.
#[allow(clippy::too_many_arguments)]
pub async fn create_session(
    pool: &PgPool,
    user_id: Uuid,
    client_id: &str,
    scopes: &[String],
    amr: &[String],
    session_secs: i64,
) -> Result<IssuedSession, StoreError> {
    let expires = Utc::now() + chrono::Duration::seconds(session_secs);
    let mut tx = pool.begin().await?;
    let session_id: Uuid = sqlx::query_scalar(
        "INSERT INTO sessions (kind, user_id, client_id, scopes, amr, expires_at) \
         VALUES ('human', $1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(user_id)
    .bind(client_id)
    .bind(scopes)
    .bind(amr)
    .bind(expires)
    .fetch_one(&mut *tx)
    .await?;

    let refresh = tokens::generate_refresh_token();
    sqlx::query(
        "INSERT INTO refresh_tokens (session_id, token_hash, expires_at) VALUES ($1, $2, $3)",
    )
    .bind(session_id)
    .bind(&refresh.hash)
    .bind(expires)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    Ok(IssuedSession {
        session_id,
        refresh,
        session_expires_at: expires,
    })
}

/// The session context needed to re-mint an access token on refresh.
pub struct RefreshedSession {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub new_refresh: RefreshToken,
    pub session_expires_at: DateTime<Utc>,
}

/// Rotate a refresh token (hive-auth-mcp-design.md §2): verify the presented
/// token, mark it used + superseded, issue a fresh one. REUSE DETECTION: if the
/// presented token was already used (superseded_by set), the whole session is
/// revoked and an error returned — that's the theft signal.
pub async fn rotate_refresh_token(
    pool: &PgPool,
    presented_raw: &str,
) -> Result<RefreshedSession, StoreError> {
    let presented_hash = tokens::hash_token(presented_raw);
    let mut tx = pool.begin().await?;

    // Look up the presented token + its session, locking the row.
    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            Uuid,
            Option<Uuid>,
            DateTime<Utc>,
            Option<DateTime<Utc>>,
        ),
    >(
        "SELECT rt.id, rt.session_id, rt.superseded_by, rt.expires_at, s.revoked_at \
         FROM refresh_tokens rt JOIN sessions s ON s.id = rt.session_id \
         WHERE rt.token_hash = $1 FOR UPDATE OF rt",
    )
    .bind(&presented_hash)
    .fetch_optional(&mut *tx)
    .await?;

    let (rt_id, session_id, superseded_by, rt_expires, session_revoked) = match row {
        Some(r) => (r.0, r.1, r.2, r.3, r.4),
        None => {
            tx.rollback().await?;
            return Err(StoreError::RefreshInvalid);
        }
    };

    // Reuse detection: a token that's already been rotated is being replayed.
    // Revoke the entire session (kill the chain) and reject.
    if superseded_by.is_some() {
        sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1 AND revoked_at IS NULL")
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Err(StoreError::RefreshReuse);
    }

    // Session revoked or token expired => invalid.
    if session_revoked.is_some() || rt_expires <= Utc::now() {
        tx.rollback().await?;
        return Err(StoreError::RefreshInvalid);
    }

    // Pull the session context for the new access token.
    let sess = sqlx::query_as::<_, (Uuid, String, Vec<String>, DateTime<Utc>)>(
        "SELECT user_id, client_id, scopes, expires_at FROM sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_one(&mut *tx)
    .await?;

    // Issue the replacement refresh token (same expiry as the session).
    let new_refresh = tokens::generate_refresh_token();
    let new_id: Uuid = sqlx::query_scalar(
        "INSERT INTO refresh_tokens (session_id, token_hash, expires_at) VALUES ($1,$2,$3) RETURNING id",
    )
    .bind(session_id)
    .bind(&new_refresh.hash)
    .bind(sess.3)
    .fetch_one(&mut *tx)
    .await?;

    // Mark the presented token used + superseded.
    sqlx::query("UPDATE refresh_tokens SET used_at = now(), superseded_by = $2 WHERE id = $1")
        .bind(rt_id)
        .bind(new_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    Ok(RefreshedSession {
        session_id,
        user_id: sess.0,
        client_id: sess.1,
        scopes: sess.2,
        new_refresh,
        session_expires_at: sess.3,
    })
}
