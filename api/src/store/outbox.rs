// Outbound work queue (store.ts `outbox` + the worker's drainOutbox).
// Owned by the admin workstream; the worker crate calls these via the lib.

use anyhow::Result;
use hive_shared::{OutboxJob, OutboxStatus, WorkerOutboxCounts};
use serde_json::json;
use sqlx::Row;

use super::{new_id, now_iso, placeholders_or_never, Store};

/// The job kinds the worker's drainer owns. The claim is narrowed to these so
/// foreign kinds (Phase 2 `mail.send`, drained by hive-mail) stay queued for
/// their own drainer instead of being swallowed as no-op successes
/// (DIRECTION.md Phase 0 item 6).
const WORKER_OUTBOX_KINDS: &[&str] = &["webhook", "log"];

impl Store {
    pub async fn outbox_enqueue(
        &self,
        kind: &str,
        payload: serde_json::Value,
        run_after: Option<String>,
        actor: &str,
    ) -> Result<OutboxJob> {
        let job = OutboxJob {
            id: new_id("out"),
            kind: kind.to_string(),
            payload,
            status: OutboxStatus::Pending,
            attempts: 0,
            last_error: None,
            run_after: run_after.unwrap_or_else(now_iso),
            created_at: now_iso(),
            completed_at: None,
        };
        crate::pgq::query(
            "INSERT INTO outbox (id, kind, payload, status, attempts, last_error, run_after, created_at, completed_at) \
             VALUES (?, ?, ?, ?, ?, NULL, ?, ?, NULL)",
        )
        .bind(&job.id)
        .bind(&job.kind)
        .bind(job.payload.to_string())
        .bind(job.status.as_str())
        .bind(job.attempts)
        .bind(&job.run_after)
        .bind(&job.created_at)
        .execute(self.db())
        .await?;
        self.emit(
            "outbox.enqueued",
            actor,
            json!({"id": job.id, "kind": job.kind}),
        )
        .await?;
        Ok(job)
    }

    pub async fn outbox_list(&self, limit: i64) -> Result<Vec<OutboxJob>> {
        let rows = crate::pgq::query("SELECT * FROM outbox ORDER BY created_at DESC LIMIT ?")
            .bind(limit)
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_job).collect()
    }

    /// Pending jobs of the given kinds whose run_after has elapsed, oldest
    /// first. Kinds are explicit so each drainer claims only work it owns.
    pub async fn outbox_claim(&self, kinds: &[&str], limit: i64) -> Result<Vec<OutboxJob>> {
        let sql = format!(
            "SELECT * FROM outbox WHERE status = 'pending' AND run_after <= ? \
             AND kind IN ({}) ORDER BY run_after LIMIT ?",
            placeholders_or_never(kinds.len())
        );
        let mut q = crate::pgq::query(&sql).bind(now_iso());
        for k in kinds {
            q = q.bind(*k);
        }
        let rows = q.bind(limit).fetch_all(self.db()).await?;
        rows.iter().map(row_to_job).collect()
    }

    pub async fn outbox_complete(&self, job_id: &str) -> Result<()> {
        crate::pgq::query("UPDATE outbox SET status='done', completed_at=? WHERE id=?")
            .bind(now_iso())
            .bind(job_id)
            .execute(self.db())
            .await?;
        Ok(())
    }

    /// Exponential backoff 2^attempts × 30s capped at 3600s; permanently failed
    /// after 5 attempts.
    pub async fn outbox_fail(&self, job_id: &str, error: &str, attempts: i64) -> Result<()> {
        let run_after = (chrono::Utc::now() + chrono::Duration::seconds(backoff_secs(attempts)))
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        let status = if attempts >= 5 {
            OutboxStatus::Failed
        } else {
            OutboxStatus::Pending
        };
        crate::pgq::query(
            "UPDATE outbox SET status=?, attempts=?, last_error=?, run_after=? WHERE id=?",
        )
        .bind(status.as_str())
        .bind(attempts)
        .bind(error)
        .bind(&run_after)
        .bind(job_id)
        .execute(self.db())
        .await?;
        Ok(())
    }

    pub async fn outbox_counts(&self) -> Result<WorkerOutboxCounts> {
        let count = |status: &'static str| async move {
            crate::pgq::query_scalar::<i64>("SELECT count(*) FROM outbox WHERE status = ?")
                .bind(status)
                .fetch_one(self.db())
                .await
        };
        Ok(WorkerOutboxCounts {
            pending: count("pending").await?,
            done: count("done").await?,
            failed: count("failed").await?,
        })
    }

    /// The worker's drainOutbox: claim up to 20 due jobs of the kinds it owns
    /// ("webhook" POSTs JSON; "log" just succeeds), complete or fail with
    /// backoff. Returns the number completed.
    pub async fn drain_outbox(&self) -> Result<i64> {
        let mut done = 0;
        let client = reqwest::Client::new();
        for job in self.outbox_claim(WORKER_OUTBOX_KINDS, 20).await? {
            let run: Result<()> = async {
                if job.kind == "webhook" {
                    let url = job
                        .payload
                        .get("url")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| anyhow::anyhow!("webhook payload missing url"))?;
                    let body = job
                        .payload
                        .get("body")
                        .cloned()
                        .unwrap_or_else(|| json!({}));
                    let res = client.post(url).json(&body).send().await?;
                    if !res.status().is_success() {
                        anyhow::bail!("HTTP {}", res.status().as_u16());
                    }
                } else {
                    // "log": success is the whole job. Unknown kinds are never
                    // claimed (WORKER_OUTBOX_KINDS), so they can't be swallowed.
                    tracing::debug!(kind = %job.kind, "outbox job ran");
                }
                Ok(())
            }
            .await;
            match run {
                Ok(()) => {
                    self.outbox_complete(&job.id).await?;
                    done += 1;
                }
                Err(e) => {
                    // Expected/transient (a webhook 5xx, a flaky endpoint) — one
                    // clean line; the job is retried per its attempt count.
                    tracing::warn!(kind = %job.kind, attempt = job.attempts + 1, reason = %e, "outbox job failed, will retry");
                    self.outbox_fail(&job.id, &e.to_string(), job.attempts + 1)
                        .await?;
                }
            }
        }
        Ok(done)
    }
}

fn backoff_secs(attempts: i64) -> i64 {
    let exp = 2i64
        .checked_pow(attempts.clamp(0, 30) as u32)
        .unwrap_or(i64::MAX);
    exp.saturating_mul(30).min(3600)
}

fn row_to_job(r: &sqlx::postgres::PgRow) -> Result<OutboxJob> {
    Ok(OutboxJob {
        id: r.try_get("id")?,
        kind: r.try_get("kind")?,
        payload: serde_json::from_str(&r.try_get::<String, _>("payload")?)
            .unwrap_or(serde_json::Value::Null),
        status: OutboxStatus::from_str_lossy(r.try_get::<String, _>("status")?.as_str()),
        attempts: r.try_get("attempts")?,
        last_error: r.try_get("last_error")?,
        run_after: r.try_get("run_after")?,
        created_at: r.try_get("created_at")?,
        completed_at: r.try_get("completed_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::backoff_secs;

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(0), 30);
        assert_eq!(backoff_secs(1), 60);
        assert_eq!(backoff_secs(5), 960);
        assert_eq!(backoff_secs(7), 3600);
        assert_eq!(backoff_secs(40), 3600);
    }
}
