//! Device Authorization Grant store (hive-auth-mcp-design.md §3.2, RFC 8628).
//!
//! The DB side of the device flow: create an authorization (device_code +
//! user_code), let an authenticated human approve/deny by user_code, and let
//! the polling CLI advance the flow by device_code. Runtime sqlx only.

use chrono::{DateTime, Duration, Utc};
use hive_db::PgPool;
use rand::Rng;
use uuid::Uuid;

use super::store::StoreError;
use super::tokens;

/// Default lifetime of a device authorization (RFC 8628 `expires_in`).
pub const DEVICE_CODE_TTL_SECS: i64 = 600; // 10 minutes
/// Default min seconds between polls (RFC 8628 `interval`).
pub const DEFAULT_INTERVAL_SECS: i32 = 5;
/// Extra seconds added to the interval when a client polls too fast.
pub const SLOW_DOWN_BUMP_SECS: i32 = 5;

/// A freshly created device authorization. `device_code` is returned raw to the
/// client ONCE (we store only its hash); `user_code` is shown to the human.
pub struct NewDeviceAuth {
    pub device_code: String,
    pub user_code: String,
    pub interval_secs: i32,
    pub expires_in: i64,
}

/// Create a device authorization. Generates a high-entropy device_code (stored
/// hashed) + a short human user_code, both unique among live rows.
pub async fn create(
    pool: &PgPool,
    client_id: &str,
    scopes: &[String],
    resource: Option<&str>,
) -> Result<NewDeviceAuth, StoreError> {
    let device_code = tokens::generate_refresh_token().raw; // 256-bit opaque
    let device_code_hash = tokens::hash_token(&device_code);
    let expires_at = Utc::now() + Duration::seconds(DEVICE_CODE_TTL_SECS);

    // Retry a few times on the (vanishingly rare) user_code collision.
    for _ in 0..5 {
        let user_code = generate_user_code();
        let res = sqlx::query(
            "INSERT INTO device_codes \
               (device_code_hash, user_code, client_id, scopes, resource, interval_secs, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&device_code_hash)
        .bind(&user_code)
        .bind(client_id)
        .bind(scopes)
        .bind(resource)
        .bind(DEFAULT_INTERVAL_SECS)
        .bind(expires_at)
        .execute(pool)
        .await;
        match res {
            Ok(_) => {
                return Ok(NewDeviceAuth {
                    device_code,
                    user_code,
                    interval_secs: DEFAULT_INTERVAL_SECS,
                    expires_in: DEVICE_CODE_TTL_SECS,
                });
            }
            Err(sqlx::Error::Database(db)) if db.is_unique_violation() => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(StoreError::Sqlx(sqlx::Error::Protocol(
        "could not allocate a unique device user_code".into(),
    )))
}

/// A pending device authorization as seen on the verification page.
#[derive(Debug, Clone)]
pub struct DeviceForApproval {
    pub id: Uuid,
    pub client_id: String,
    pub scopes: Vec<String>,
    pub status: String,
    pub expired: bool,
}

/// Look up a device authorization by its user_code (the human-entered code).
pub async fn find_by_user_code(
    pool: &PgPool,
    user_code: &str,
) -> Result<Option<DeviceForApproval>, StoreError> {
    let normalized = normalize_user_code(user_code);
    let row = sqlx::query_as::<_, (Uuid, String, Vec<String>, String, DateTime<Utc>)>(
        "SELECT id, client_id, scopes, status, expires_at FROM device_codes WHERE user_code = $1",
    )
    .bind(&normalized)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| DeviceForApproval {
        id: r.0,
        client_id: r.1,
        scopes: r.2,
        status: r.3,
        expired: r.4 <= Utc::now(),
    }))
}

/// Approve a pending device by user_code, binding it to the approving human +
/// their auth methods (amr). Returns true if a pending row was approved.
pub async fn approve(
    pool: &PgPool,
    user_code: &str,
    user_id: Uuid,
    amr: &[String],
) -> Result<bool, StoreError> {
    let normalized = normalize_user_code(user_code);
    let res = sqlx::query(
        "UPDATE device_codes SET status = 'approved', user_id = $2, amr = $3 \
         WHERE user_code = $1 AND status = 'pending' AND expires_at > now()",
    )
    .bind(&normalized)
    .bind(user_id)
    .bind(amr)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Deny a pending device by user_code. Returns true if a pending row was denied.
pub async fn deny(pool: &PgPool, user_code: &str) -> Result<bool, StoreError> {
    let normalized = normalize_user_code(user_code);
    let res = sqlx::query(
        "UPDATE device_codes SET status = 'denied' \
         WHERE user_code = $1 AND status = 'pending'",
    )
    .bind(&normalized)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// The outcome of a poll against /token with a device_code (RFC 8628 §3.5).
pub enum PollOutcome {
    /// User hasn't approved yet — keep polling.
    AuthorizationPending,
    /// Polling faster than `interval` — back off (interval was bumped).
    SlowDown,
    /// Code expired — stop.
    ExpiredToken,
    /// User denied — stop.
    AccessDenied,
    /// Approved: issue tokens for this user with these scopes + amr.
    Approved {
        device_id: Uuid,
        user_id: Uuid,
        client_id: String,
        scopes: Vec<String>,
        amr: Vec<String>,
    },
    /// No such device_code.
    Unknown,
}

/// Advance a poll: look up by device_code, enforce expiry + the polling
/// interval (slow_down), and report the state. On too-fast polling the row's
/// interval is bumped by SLOW_DOWN_BUMP_SECS (RFC 8628 §3.5). `last_polled_at`
/// is updated on every poll so the rate check is honest.
pub async fn poll(pool: &PgPool, device_code: &str) -> Result<PollOutcome, StoreError> {
    let hash = tokens::hash_token(device_code.trim());
    let mut tx = pool.begin().await?;

    let row = sqlx::query_as::<
        _,
        (
            Uuid,
            String,
            Vec<String>,
            String,
            Option<Uuid>,
            Vec<String>,
            i32,
            Option<DateTime<Utc>>,
            DateTime<Utc>,
        ),
    >(
        "SELECT id, client_id, scopes, status, user_id, amr, interval_secs, last_polled_at, expires_at \
         FROM device_codes WHERE device_code_hash = $1 FOR UPDATE",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?;

    let Some((id, client_id, scopes, status, user_id, amr, interval_secs, last_polled, expires_at)) =
        row
    else {
        tx.rollback().await?;
        return Ok(PollOutcome::Unknown);
    };

    // Expired regardless of status.
    if expires_at <= Utc::now() {
        tx.commit().await?;
        return Ok(PollOutcome::ExpiredToken);
    }

    // slow_down: if the client polled within `interval`, bump the interval and
    // tell it to back off. Still record this poll time.
    let now = Utc::now();
    let too_fast = last_polled
        .map(|t| (now - t) < Duration::seconds(interval_secs as i64))
        .unwrap_or(false);
    let new_interval = if too_fast {
        interval_secs + SLOW_DOWN_BUMP_SECS
    } else {
        interval_secs
    };
    sqlx::query("UPDATE device_codes SET last_polled_at = $2, interval_secs = $3 WHERE id = $1")
        .bind(id)
        .bind(now)
        .bind(new_interval)
        .execute(&mut *tx)
        .await?;

    if too_fast {
        tx.commit().await?;
        return Ok(PollOutcome::SlowDown);
    }

    let outcome = match status.as_str() {
        "denied" => PollOutcome::AccessDenied,
        "approved" => match user_id {
            Some(uid) => PollOutcome::Approved {
                device_id: id,
                user_id: uid,
                client_id,
                scopes,
                amr,
            },
            None => PollOutcome::AuthorizationPending, // shouldn't happen
        },
        _ => PollOutcome::AuthorizationPending,
    };
    tx.commit().await?;
    Ok(outcome)
}

/// Consume an approved device row after tokens are issued (single redemption):
/// delete it so the device_code can't be replayed.
pub async fn consume(pool: &PgPool, device_id: Uuid) -> Result<(), StoreError> {
    sqlx::query("DELETE FROM device_codes WHERE id = $1")
        .bind(device_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------- code generation ----------

/// A short, human-friendly user_code: 8 chars from an unambiguous alphabet
/// (no 0/O/1/I), grouped XXXX-XXXX. ~40 bits, fine for a short-TTL one-time code.
fn generate_user_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let pick = |rng: &mut rand::rngs::ThreadRng| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char;
    let g1: String = (0..4).map(|_| pick(&mut rng)).collect();
    let g2: String = (0..4).map(|_| pick(&mut rng)).collect();
    format!("{g1}-{g2}")
}

/// Normalize a user-entered code: uppercase, strip spaces, and re-insert the
/// dash so "wdjb mjht" / "wdjbmjht" / "WDJB-MJHT" all match.
fn normalize_user_code(input: &str) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if cleaned.len() == 8 {
        format!("{}-{}", &cleaned[..4], &cleaned[4..])
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_code_shape_and_alphabet() {
        let c = generate_user_code();
        assert_eq!(c.len(), 9, "XXXX-XXXX");
        assert_eq!(c.as_bytes()[4], b'-');
        // no ambiguous chars
        assert!(!c.contains('0') && !c.contains('O') && !c.contains('1') && !c.contains('I'));
    }

    #[test]
    fn normalize_handles_spacing_case_and_dash() {
        assert_eq!(normalize_user_code("wdjb mjht"), "WDJB-MJHT");
        assert_eq!(normalize_user_code("wdjbmjht"), "WDJB-MJHT");
        assert_eq!(normalize_user_code("WDJB-MJHT"), "WDJB-MJHT");
    }
}
