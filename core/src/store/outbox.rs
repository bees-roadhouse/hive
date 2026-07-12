// Outbound work queue (store.ts `outbox` + the worker's drainOutbox).
// RUNTIME state (see index/mod.rs): a transient retry queue writes directly —
// attempt bookkeeping does not belong in the append-only log, and losing
// pending webhook jobs on an index rebuild is the accepted trade.

use anyhow::Result;
use hive_shared::{OutboxJob, OutboxStatus, WorkerOutboxCounts};
use rusqlite::OptionalExtension;
use serde_json::json;

use super::{new_id, now_iso, placeholders_or_never, Store};

/// The job kinds this drainer owns. The claim is narrowed to these so foreign
/// kinds (`mail.send`, drained by the mail module when it returns in Phase 3)
/// stay queued for their own drainer instead of being swallowed as no-op
/// successes (DIRECTION.md Phase 0 item 6).
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
        let row = job.clone();
        self.run(move |core| {
            core.conn().execute(
                "INSERT INTO outbox (id, kind, payload, status, attempts, last_error, run_after, created_at, completed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, NULL)",
                rusqlite::params![
                    row.id,
                    row.kind,
                    row.payload.to_string(),
                    row.status.as_str(),
                    row.attempts,
                    row.run_after,
                    row.created_at
                ],
            )?;
            Ok(())
        })
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
        self.run(move |core| {
            let mut stmt = core
                .conn()
                .prepare("SELECT * FROM outbox ORDER BY created_at DESC LIMIT ?1")?;
            let rows = stmt.query_map(rusqlite::params![limit], row_to_job)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// Pending jobs of the given kinds whose run_after has elapsed, oldest
    /// first. Kinds are explicit so each drainer claims only work it owns.
    pub async fn outbox_claim(&self, kinds: &[&str], limit: i64) -> Result<Vec<OutboxJob>> {
        let kinds: Vec<String> = kinds.iter().map(|k| k.to_string()).collect();
        self.run(move |core| {
            let sql = format!(
                "SELECT * FROM outbox WHERE status = 'pending' AND run_after <= ? \
                 AND kind IN ({}) ORDER BY run_after LIMIT ?",
                placeholders_or_never(kinds.len())
            );
            let mut stmt = core.conn().prepare(&sql)?;
            let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(now_iso())];
            for k in &kinds {
                binds.push(Box::new(k.clone()));
            }
            binds.push(Box::new(limit));
            let rows = stmt.query_map(
                rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())),
                row_to_job,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    /// One job by id, or `None` when it's gone. The compose UI polls this to
    /// turn a queued send into a Sent/Failed status.
    pub async fn outbox_get(&self, job_id: &str) -> Result<Option<OutboxJob>> {
        let job_id = job_id.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT * FROM outbox WHERE id = ?1",
                    rusqlite::params![job_id],
                    row_to_job,
                )
                .optional()?)
        })
        .await
    }

    pub async fn outbox_complete(&self, job_id: &str) -> Result<()> {
        let job_id = job_id.to_string();
        self.run(move |core| {
            core.conn().execute(
                "UPDATE outbox SET status='done', completed_at=?1 WHERE id=?2",
                rusqlite::params![now_iso(), job_id],
            )?;
            Ok(())
        })
        .await
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
        let (job_id, error) = (job_id.to_string(), error.to_string());
        self.run(move |core| {
            core.conn().execute(
                "UPDATE outbox SET status=?1, attempts=?2, last_error=?3, run_after=?4 WHERE id=?5",
                rusqlite::params![status.as_str(), attempts, error, run_after, job_id],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn outbox_counts(&self) -> Result<WorkerOutboxCounts> {
        self.run(|core| {
            let count = |status: &str| -> rusqlite::Result<i64> {
                core.conn().query_row(
                    "SELECT count(*) FROM outbox WHERE status = ?1",
                    rusqlite::params![status],
                    |r| r.get(0),
                )
            };
            Ok(WorkerOutboxCounts {
                pending: count("pending")?,
                done: count("done")?,
                failed: count("failed")?,
            })
        })
        .await
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

/// Shared by the outbox drainer and the mail account scheduler (DIRECTION.md
/// D5 reuses this exact arithmetic).
pub(crate) fn backoff_secs(attempts: i64) -> i64 {
    let exp = 2i64
        .checked_pow(attempts.clamp(0, 30) as u32)
        .unwrap_or(i64::MAX);
    exp.saturating_mul(30).min(3600)
}

fn row_to_job(r: &rusqlite::Row) -> rusqlite::Result<OutboxJob> {
    Ok(OutboxJob {
        id: r.get("id")?,
        kind: r.get("kind")?,
        payload: serde_json::from_str(&r.get::<_, String>("payload")?)
            .unwrap_or(serde_json::Value::Null),
        status: OutboxStatus::from_str_lossy(r.get::<_, String>("status")?.as_str()),
        attempts: r.get("attempts")?,
        last_error: r.get("last_error")?,
        run_after: r.get("run_after")?,
        created_at: r.get("created_at")?,
        completed_at: r.get("completed_at")?,
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
