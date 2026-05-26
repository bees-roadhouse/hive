//! Risk-based adaptive token rotation (hive-auth-mcp-design.md §5.7).
//!
//! Scope: the NON-EXPIRING AI/MCP token class only. Human tokens expire
//! naturally and are never scored here. Every MCP-token use is compared to a
//! per-session behavioral baseline (seen IPs / user-agents / cadence); an
//! anomaly produces a score + band. On a MEDIUM/HIGH band the response is a
//! **re-key** (invalidate the current jti so the next request must re-mint),
//! NOT a revoke — the grant + AI identity stay intact (§5.7 invalidate-and-rekey).
//!
//! SHADOW-FIRST: by default (`HIVE_RISK_ENFORCE` unset/false) the engine only
//! scores + logs ("would force re-key") and records a `risk_events` row; it does
//! NOT touch the token. Flip `HIVE_RISK_ENFORCE=1` to actually invalidate.
//!
//! Signals here are IP + UA + cadence (cheap, no external dependency). Coarse
//! geo / ASN + impossible-travel velocity are the documented next signals (need
//! a geo-IP DB) — `RiskSignals` carries the fields so they slot in without a
//! schema change, but Phase 7 populates IP/UA/cadence only.

use chrono::{DateTime, Utc};
use hive_db::PgPool;
use uuid::Uuid;

use super::store::StoreError;

/// CAEP-aligned risk band (§5.7). LOW serves normally; MEDIUM/HIGH trigger a
/// re-key (MEDIUM = silent re-key, HIGH = the design's step-up-MFA tier — both
/// invalidate the jti; the step-up nuance is the connect-side concern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RiskBand {
    Low,
    Medium,
    High,
}

impl RiskBand {
    pub fn as_str(&self) -> &'static str {
        match self {
            RiskBand::Low => "low",
            RiskBand::Medium => "medium",
            RiskBand::High => "high",
        }
    }

    /// MEDIUM/HIGH force a re-key; LOW serves normally.
    pub fn forces_rekey(&self) -> bool {
        matches!(self, RiskBand::Medium | RiskBand::High)
    }
}

/// Signal weights (config-shaped; conservative defaults favoring false-negatives
/// over re-auth storms, §5.7). Additive, with a dominant-signal override: any
/// single HIGH-weight signal lifts the whole decision to HIGH.
const W_NEW_IP: i32 = 1; // a new IP within known behavior is weak on its own
const W_NEW_UA: i32 = 2; // a different client family mid-session
const W_CADENCE_SPIKE: i32 = 2; // request-rate far above baseline
const MEDIUM_THRESHOLD: i32 = 2;
const HIGH_THRESHOLD: i32 = 4;
/// Warmup: cadence signals are suppressed until the token has this many uses
/// (a fresh token has no rhythm to deviate from, §5.7).
const WARMUP_USES: i32 = 3;
/// A poll faster than this (seconds) below the rolling median counts as a spike.
const CADENCE_MIN_GAP_SECS: i64 = 1;

/// The per-request signals captured for one MCP-token use.
#[derive(Debug, Clone, Default)]
pub struct RiskSignals {
    pub ip: Option<String>,
    pub user_agent: Option<String>,
    // Reserved for the geo seam (Phase 7 leaves these None).
    pub geo_country: Option<String>,
    pub asn: Option<i32>,
}

/// The session's rolling baseline (denormalized on the sessions row).
#[derive(Debug, Clone, Default)]
pub struct Baseline {
    pub seen_ips: Vec<String>,
    pub seen_uas: Vec<String>,
    pub use_count: i32,
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// A scored decision: the band, the numeric score, and which signals fired.
#[derive(Debug, Clone)]
pub struct RiskDecision {
    pub band: RiskBand,
    pub score: i32,
    pub reasons: Vec<String>,
}

/// Score one use against a baseline. Pure + deterministic (no clock except the
/// `now` passed in) so it's unit-testable. The dominant-signal override is
/// reserved for the geo/impossible-travel HIGH signal (not yet populated); the
/// IP/UA/cadence signals are additive and band by threshold.
pub fn score(signals: &RiskSignals, base: &Baseline, now: DateTime<Utc>) -> RiskDecision {
    // A brand-new session (no baseline yet) is seeded, never flagged.
    if base.use_count == 0 {
        return RiskDecision {
            band: RiskBand::Low,
            score: 0,
            reasons: vec![],
        };
    }

    let mut score = 0;
    let mut reasons = Vec::new();

    if let Some(ip) = signals.ip.as_deref()
        && !base.seen_ips.iter().any(|s| s == ip)
    {
        score += W_NEW_IP;
        reasons.push("new_ip".to_string());
    }
    if let Some(ua) = signals.user_agent.as_deref()
        && !base.seen_uas.iter().any(|s| s == ua)
    {
        score += W_NEW_UA;
        reasons.push("new_user_agent".to_string());
    }

    // Cadence spike: suppressed during warmup. Fires when this use lands
    // implausibly close after the previous one (a burst the baseline hasn't
    // shown). Conservative: a single near-instant repeat.
    if base.use_count >= WARMUP_USES
        && let Some(last) = base.last_seen_at
    {
        let gap = (now - last).num_seconds();
        if gap < CADENCE_MIN_GAP_SECS {
            score += W_CADENCE_SPIKE;
            reasons.push("cadence_spike".to_string());
        }
    }

    // Dominant-signal override (reserved): a HIGH-weight geo signal would set
    // band=High here regardless of sum. Phase 7 has no HIGH signal populated.
    let band = if score >= HIGH_THRESHOLD {
        RiskBand::High
    } else if score >= MEDIUM_THRESHOLD {
        RiskBand::Medium
    } else {
        RiskBand::Low
    };

    RiskDecision {
        band,
        score,
        reasons,
    }
}

/// Whether risk rotation is ENFORCED (vs shadow). Default false (shadow): score
/// + log only. `HIVE_RISK_ENFORCE=1|true` flips to enforce.
pub fn enforce_enabled() -> bool {
    matches!(
        std::env::var("HIVE_RISK_ENFORCE").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    )
}

/// Load a session's baseline (the denormalized summary columns).
pub async fn load_baseline(pool: &PgPool, session_id: Uuid) -> Result<Baseline, StoreError> {
    let row = sqlx::query_as::<_, (Vec<String>, Vec<String>, i32, Option<DateTime<Utc>>)>(
        "SELECT risk_seen_ips, risk_seen_uas, risk_use_count, risk_last_seen_at \
         FROM sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?
    .unwrap_or_default();
    Ok(Baseline {
        seen_ips: row.0,
        seen_uas: row.1,
        use_count: row.2,
        last_seen_at: row.3,
    })
}

/// Cap on how many distinct IPs/UAs we keep in a baseline set (bounded, §5.7).
const SEEN_SET_CAP: usize = 20;

/// Fold a new value into a bounded seen-set (keeps the most recent up to the cap).
fn fold_seen(mut set: Vec<String>, value: Option<&str>) -> Vec<String> {
    if let Some(v) = value
        && !v.is_empty()
        && !set.iter().any(|s| s == v)
    {
        set.push(v.to_string());
        if set.len() > SEEN_SET_CAP {
            let overflow = set.len() - SEEN_SET_CAP;
            set.drain(0..overflow);
        }
    }
    set
}

/// Update the session baseline with this use (extend seen-sets, bump count,
/// advance last_seen). Called after scoring so the score compared against the
/// PRIOR baseline.
pub async fn update_baseline(
    pool: &PgPool,
    session_id: Uuid,
    base: &Baseline,
    signals: &RiskSignals,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let ips = fold_seen(base.seen_ips.clone(), signals.ip.as_deref());
    let uas = fold_seen(base.seen_uas.clone(), signals.user_agent.as_deref());
    sqlx::query(
        "UPDATE sessions SET risk_seen_ips = $2, risk_seen_uas = $3, \
           risk_use_count = risk_use_count + 1, \
           risk_first_seen_at = COALESCE(risk_first_seen_at, $4), \
           risk_last_seen_at = $4 \
         WHERE id = $1",
    )
    .bind(session_id)
    .bind(&ips)
    .bind(&uas)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a per-use signal row (bounded: prune to the last N for the session).
pub async fn record_signal(
    pool: &PgPool,
    session_id: Uuid,
    jti: Option<Uuid>,
    signals: &RiskSignals,
    decision: &RiskDecision,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO token_usage_signals \
           (session_id, jti, ip, user_agent, geo_country, asn, score, band) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(session_id)
    .bind(jti)
    .bind(signals.ip.as_deref())
    .bind(signals.user_agent.as_deref())
    .bind(signals.geo_country.as_deref())
    .bind(signals.asn)
    .bind(decision.score)
    .bind(decision.band.as_str())
    .execute(pool)
    .await?;
    // Bounded retention: keep the most recent 50 per session.
    sqlx::query(
        "DELETE FROM token_usage_signals WHERE session_id = $1 AND id NOT IN \
           (SELECT id FROM token_usage_signals WHERE session_id = $1 \
            ORDER BY used_at DESC LIMIT 50)",
    )
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Write the CAEP-shaped audit row for a risk decision (§5.7).
#[allow(clippy::too_many_arguments)]
pub async fn record_event(
    pool: &PgPool,
    session_id: Uuid,
    jti: Option<Uuid>,
    subject_id: Option<Uuid>,
    act_user_id: Option<Uuid>,
    decision: &RiskDecision,
    enforced: bool,
) -> Result<(), StoreError> {
    let action = if decision.band.forces_rekey() && enforced {
        "rekey_forced"
    } else {
        "observed"
    };
    let mode = if enforced { "enforced" } else { "shadow" };
    sqlx::query(
        "INSERT INTO risk_events \
           (session_id, jti, subject_id, act_user_id, band, score, reasons, mode, action) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(session_id)
    .bind(jti)
    .bind(subject_id)
    .bind(act_user_id)
    .bind(decision.band.as_str())
    .bind(decision.score)
    .bind(&decision.reasons)
    .bind(mode)
    .bind(action)
    .execute(pool)
    .await?;
    Ok(())
}

/// Mark a session as needing a re-key (enforced path): the live token is dead
/// (its jti was pushed into the revocation set), and the next MCP connect mints
/// a fresh token, then clears this. The grant + identity are untouched.
pub async fn mark_needs_rekey(pool: &PgPool, session_id: Uuid) -> Result<(), StoreError> {
    sqlx::query("UPDATE sessions SET needs_rekey = TRUE WHERE id = $1")
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn base(use_count: i32) -> Baseline {
        Baseline {
            seen_ips: vec!["192.0.2.10".to_string()],
            seen_uas: vec!["hive-mcp/1.0".to_string()],
            use_count,
            last_seen_at: Some(now() - chrono::Duration::seconds(60)),
        }
    }

    #[test]
    fn fresh_session_is_never_flagged() {
        // use_count == 0 => seed, band Low regardless of signals.
        let sig = RiskSignals {
            ip: Some("203.0.113.99".into()),
            user_agent: Some("totally-different".into()),
            ..Default::default()
        };
        let d = score(&sig, &Baseline::default(), now());
        assert_eq!(d.band, RiskBand::Low);
        assert!(d.reasons.is_empty());
    }

    #[test]
    fn known_ip_and_ua_score_zero() {
        let sig = RiskSignals {
            ip: Some("192.0.2.10".into()),
            user_agent: Some("hive-mcp/1.0".into()),
            ..Default::default()
        };
        let d = score(&sig, &base(10), now());
        assert_eq!(d.score, 0);
        assert_eq!(d.band, RiskBand::Low);
    }

    #[test]
    fn new_ip_alone_is_low() {
        let sig = RiskSignals {
            ip: Some("203.0.113.5".into()),
            user_agent: Some("hive-mcp/1.0".into()),
            ..Default::default()
        };
        let d = score(&sig, &base(10), now());
        assert!(d.reasons.contains(&"new_ip".to_string()));
        assert_eq!(d.band, RiskBand::Low, "a new IP alone shouldn't re-key");
    }

    #[test]
    fn new_ip_plus_new_ua_reaches_medium() {
        let sig = RiskSignals {
            ip: Some("203.0.113.5".into()),
            user_agent: Some("curl/8.0".into()),
            ..Default::default()
        };
        let d = score(&sig, &base(10), now());
        // W_NEW_IP(1) + W_NEW_UA(2) = 3 >= MEDIUM_THRESHOLD(2)
        assert_eq!(d.band, RiskBand::Medium);
        assert!(d.band.forces_rekey());
    }

    #[test]
    fn cadence_spike_suppressed_during_warmup() {
        let mut b = base(1); // below WARMUP_USES
        b.last_seen_at = Some(now()); // ~0s gap
        let sig = RiskSignals {
            ip: Some("192.0.2.10".into()),
            user_agent: Some("hive-mcp/1.0".into()),
            ..Default::default()
        };
        let d = score(&sig, &b, now());
        assert!(!d.reasons.contains(&"cadence_spike".to_string()));
    }

    #[test]
    fn fold_seen_dedupes_and_caps() {
        let set = fold_seen(vec!["a".into()], Some("a"));
        assert_eq!(set.len(), 1, "dedupes");
        let set = fold_seen(set, Some("b"));
        assert_eq!(set, vec!["a".to_string(), "b".to_string()]);
        // cap
        let mut big: Vec<String> = (0..SEEN_SET_CAP).map(|i| i.to_string()).collect();
        big = fold_seen(big, Some("overflow"));
        assert_eq!(big.len(), SEEN_SET_CAP);
        assert_eq!(big.last().unwrap(), "overflow");
        assert_ne!(big.first().unwrap(), "0", "oldest dropped");
    }
}
