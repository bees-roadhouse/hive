// Artifacts: stored FILES — photos, scans, PDFs, audio, mail attachments. The
// row here is the artifact; its bytes live in `crate::artifact_storage`.
//
// (Claude Code skills / agents / slash-commands are `identity_artifacts`, a
// different table, a different module, and no longer called just "artifacts".)
//
// ── Dedup: one row per upload, one stored file per (org, sha256) ────────────
//
// Uploading the same bytes twice writes the bytes once and inserts two rows.
// Collapsing to one row would be cheaper by exactly one row and would lose
// things that are per-upload facts rather than per-content facts: who uploaded
// it, when, and under what name. It would also fuse two derivation trees —
// derived text and thumbnails hang off an artifact ID (docs/ARTIFACTS.md
// Part 3), so the same scan arriving from two mails must stay two artifacts or
// one of them silently inherits the other's OCR and provenance.
//
// ── The critical section, and what it has to contain ───────────────────────
//
// The cost of that choice is that delete has to refcount, and a refcount is
// only worth anything if the filesystem work sits INSIDE the lock it is taken
// under. It did not use to: create published its bytes before locking anything
// (a dedup hit against a file it held no lock on), and delete unlinked after
// committing (so the lock was already gone). That left a window a full round
// trip wide in which an upload could dedup against bytes a concurrent delete
// was about to unlink, then insert a row addressing nothing.
//
// So both filesystem operations happen under `lock_blob(org, sha256)`:
//
//   create   seal (hash, no publish) → LOCK → land (dedup re-check + rename)
//                                           → INSERT → commit/unlock
//   delete   DELETE … RETURNING → LOCK → count → move bytes aside
//                                           → commit/unlock → unlink
//
// Whichever takes the lock first wins cleanly. Delete-then-create: the create's
// re-check finds the address empty and re-lands the temp it still holds.
// Create-then-delete: the delete's count sees the new row and leaves the bytes
// alone. Neither order loses bytes a committed row needs.
//
// Delete moves bytes to trash and unlinks only after the row's removal is
// committed, so the two failure modes are litter (recoverable, and the sweeper
// collects it) rather than a live row pointing at nothing (not recoverable).
// Nothing here can reach another org's bytes: the content address is org-scoped
// — see artifact_storage.rs.

use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use hive_shared::Artifact;
use serde_json::json;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use super::Store;
use crate::acting::{self, ActingScope};
use crate::artifact_storage::{ArtifactStorage, ArtifactWrite, Sweepable};

const ARTIFACT_COLS: &str = "id, org_id, sha256, mime, bytes, filename, created_by, created_at";

/// Serialize same-blob writes and deletes within an org. Transaction-scoped, so
/// it releases on commit or rollback with nothing to clean up — which is also
/// why every filesystem call it guards has to happen before that commit.
async fn lock_blob(tx: &mut Transaction<'_, Postgres>, org: Uuid, sha256: &str) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("artifact:{org}:{sha256}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Rows in this org still referencing these bytes. Only meaningful under
/// [`lock_blob`], which is why it is never called anywhere else.
async fn blob_refcount(tx: &mut Transaction<'_, Postgres>, org: Uuid, sha256: &str) -> Result<i64> {
    Ok(crate::pgq::query_scalar::<i64>(
        "SELECT count(*) FROM artifacts WHERE org_id = ? AND sha256 = ?",
    )
    .bind(org)
    .bind(sha256)
    .fetch_one(&mut **tx)
    .await?)
}

/// What one sweep reclaimed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactSweep {
    /// Partial uploads nobody finished.
    pub temps_removed: u64,
    /// Trashed bytes a row still needs, put back at their address.
    pub trash_restored: u64,
    /// Trashed bytes nothing references any more, unlinked.
    pub trash_purged: u64,
    /// Objects at a content address with no row pointing at them.
    pub orphans_removed: u64,
}

impl ArtifactSweep {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn add(&mut self, other: Self) {
        self.temps_removed += other.temps_removed;
        self.trash_restored += other.trash_restored;
        self.trash_purged += other.trash_purged;
        self.orphans_removed += other.orphans_removed;
    }
}

impl Store {
    /// Land `write`'s bytes and record them. The long part — streaming the
    /// payload into a temp file — has already happened by the time this is
    /// called, and sealing it costs a flush, an fsync, and a hash. The
    /// transaction covers only the rename into the content address plus the
    /// INSERT, so a 200 MB upload never holds a database transaction open while
    /// it transfers, and the rename it does hold open is microseconds.
    pub async fn artifacts_create(
        &self,
        org: Uuid,
        write: Box<dyn ArtifactWrite>,
        mime: &str,
        filename: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<Artifact> {
        // Seal first: the lock is keyed on the content address, which is only
        // known once the bytes are through the hasher. NOTHING is published at
        // that address yet — publishing before the lock is exactly the bug.
        let staged = write.finish().await?;

        let mut tx = self.db().begin().await?; // acting org is stamped on the connection by the pool hooks (core/src/acting.rs); the explicit org_id predicates below are belt to that policy's braces
        lock_blob(&mut tx, org, staged.sha256()).await?;

        // Inside the lock: the dedup check is re-taken here, so a delete that
        // unlinked these bytes while this upload streamed is seen, and the temp
        // this still holds is re-landed rather than thrown away.
        //
        // If the INSERT below fails, the bytes are left at their address with
        // no row — an orphan, which is what `artifacts_sweep` is for. That is
        // the deliberate direction to fail in.
        let stored = staged.land().await?;

        let row = crate::pgq::query(&format!(
            "INSERT INTO artifacts (id, org_id, sha256, mime, bytes, filename, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING {ARTIFACT_COLS}"
        ))
        .bind(Uuid::new_v4())
        .bind(org)
        .bind(&stored.sha256)
        .bind(mime)
        .bind(stored.bytes as i64)
        .bind(filename)
        .bind(created_by)
        .fetch_one(&mut *tx)
        .await?;
        let artifact = row_to_artifact(&row)?;
        tx.commit().await?;

        self.emit(
            "artifact.created",
            created_by.unwrap_or("anon"),
            json!({
                "id": artifact.id,
                "mime": artifact.mime,
                "bytes": artifact.bytes,
                "filename": artifact.filename,
                "deduped": stored.deduped,
            }),
        )
        .await?;
        Ok(artifact)
    }

    /// One artifact's metadata, or None when it is absent or belongs to another
    /// org. The `org_id` predicate is belt to RLS's braces: the policy is the
    /// authority, but a role with BYPASSRLS (local dev is one today) would
    /// otherwise read across orgs without anyone noticing.
    pub async fn artifacts_get(&self, org: Uuid, id: Uuid) -> Result<Option<Artifact>> {
        let mut tx = self.db().begin().await?; // acting org is stamped on the connection by the pool hooks (core/src/acting.rs); the explicit org_id predicates below are belt to that policy's braces
        let row = crate::pgq::query(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifacts WHERE id = ? AND org_id = ?"
        ))
        .bind(id)
        .bind(org)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        row.as_ref().map(row_to_artifact).transpose()
    }

    /// Newest first, for an org. Paging is a `limit` for now — listing UI is
    /// not this workstream.
    pub async fn artifacts_list(&self, org: Uuid, limit: i64) -> Result<Vec<Artifact>> {
        let mut tx = self.db().begin().await?; // acting org is stamped on the connection by the pool hooks (core/src/acting.rs); the explicit org_id predicates below are belt to that policy's braces
        let rows = crate::pgq::query(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifacts WHERE org_id = ? \
             ORDER BY created_at DESC, id DESC LIMIT ?"
        ))
        .bind(org)
        .bind(limit.clamp(1, 500))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        rows.iter().map(row_to_artifact).collect()
    }

    /// Delete the row and, when it was the last reference to those bytes in
    /// this org, unlink them. Returns the deleted artifact, or None when there
    /// was nothing to delete.
    ///
    /// `DELETE … RETURNING` rather than SELECT-then-DELETE: it takes the row
    /// lock and reports whether THIS statement is the one that removed the row,
    /// so two concurrent deletes of the same id cannot both answer 204 and both
    /// try to unlink the bytes.
    ///
    /// The bytes are moved aside under the blob lock — a concurrent upload's
    /// dedup re-check therefore sees them gone and re-lands its own temp — and
    /// unlinked only once the row's removal is committed. A crash in between
    /// leaves a trash entry, which `artifacts_sweep` either restores (if a row
    /// still needs it) or finishes removing. Unlinking inside the transaction
    /// instead would strand a live row on rollback, which is the failure this
    /// whole shape exists to avoid.
    pub async fn artifacts_delete(
        &self,
        org: Uuid,
        id: Uuid,
        storage: &dyn ArtifactStorage,
    ) -> Result<Option<Artifact>> {
        let mut tx = self.db().begin().await?; // acting org is stamped on the connection by the pool hooks (core/src/acting.rs); the explicit org_id predicates below are belt to that policy's braces

        let Some(row) = crate::pgq::query(&format!(
            "DELETE FROM artifacts WHERE id = ? AND org_id = ? RETURNING {ARTIFACT_COLS}"
        ))
        .bind(id)
        .bind(org)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let artifact = row_to_artifact(&row)?;

        lock_blob(&mut tx, org, &artifact.sha256).await?;
        let remaining = blob_refcount(&mut tx, org, &artifact.sha256).await?;
        let trashed = if remaining == 0 {
            storage.trash(org, &artifact.sha256).await?
        } else {
            None
        };

        if let Err(e) = tx.commit().await {
            // The row is back and the bytes are not. Put them back now; the
            // sweeper is the backstop if even that fails.
            if let Some(t) = &trashed {
                if let Err(restore) = storage.restore(org, t).await {
                    tracing::error!(
                        error = %restore,
                        sha256 = %t.sha256,
                        "artifact delete rolled back but its bytes are still aside; the sweeper will restore them"
                    );
                }
            }
            return Err(e.into());
        }

        // The row is gone for good, so the bytes can go. A failure here leaves
        // a trash entry rather than a live row addressing nothing.
        if let Some(t) = &trashed {
            if let Err(e) = storage.purge(org, t).await {
                tracing::warn!(
                    error = %e,
                    sha256 = %t.sha256,
                    "artifact bytes moved aside but not unlinked; the sweeper will finish it"
                );
            }
        }

        self.emit(
            "artifact.deleted",
            "system",
            json!({"id": artifact.id, "bytesUnlinked": remaining == 0}),
        )
        .await?;
        Ok(Some(artifact))
    }

    /// Reconcile one org's stored bytes against its rows, in both directions.
    /// Nothing else collects the three kinds of litter this layer can produce:
    ///
    /// * a partial upload whose connection died,
    /// * bytes a delete moved aside and did not finish disposing of (or that a
    ///   rolled-back delete still needs),
    /// * an object at a content address that no row points at, left by an
    ///   upload that landed its bytes and then failed to commit its row.
    ///
    /// Every decision is taken under the same blob lock the create and delete
    /// paths use, so an in-flight upload cannot have its bytes swept out from
    /// under it. `min_age` guards the one case the lock cannot: a temp file
    /// belonging to a request that is still streaming.
    pub async fn artifacts_sweep(
        &self,
        org: Uuid,
        storage: &dyn ArtifactStorage,
        min_age: Duration,
    ) -> Result<ArtifactSweep> {
        let mut swept = ArtifactSweep::default();

        for item in storage.sweepable(org).await? {
            match item {
                Sweepable::Temp { key, age } => {
                    if age < min_age {
                        continue;
                    }
                    storage.discard_temp(org, &key).await?;
                    swept.temps_removed += 1;
                }

                // No age guard: a live delete holds the blob lock across its
                // whole transaction, so getting the lock means the answer here
                // is final. A rolled-back delete shows up as a trash entry
                // whose row came back, and gets its bytes back with it.
                Sweepable::Trashed(trashed) => {
                    let mut tx = self.db().begin().await?;
                    lock_blob(&mut tx, org, &trashed.sha256).await?;
                    let referenced = blob_refcount(&mut tx, org, &trashed.sha256).await? > 0;
                    if referenced {
                        storage.restore(org, &trashed).await?;
                        swept.trash_restored += 1;
                    } else {
                        storage.purge(org, &trashed).await?;
                        swept.trash_purged += 1;
                    }
                    tx.commit().await?;
                }

                Sweepable::Object { sha256, age } => {
                    if age < min_age {
                        continue;
                    }
                    let mut tx = self.db().begin().await?;
                    lock_blob(&mut tx, org, &sha256).await?;
                    let orphan = blob_refcount(&mut tx, org, &sha256).await? == 0;
                    // Move it aside under the lock, unlink it after — the same
                    // ordering a delete uses, for the same reason.
                    let trashed = if orphan {
                        storage.trash(org, &sha256).await?
                    } else {
                        None
                    };
                    tx.commit().await?;
                    if let Some(t) = trashed {
                        storage.purge(org, &t).await?;
                        swept.orphans_removed += 1;
                    }
                }
            }
        }
        Ok(swept)
    }

    /// [`Store::artifacts_sweep`] over every org holding stored bytes. Driven
    /// off the STORAGE rather than the `orgs` table on purpose: bytes belonging
    /// to an org whose rows are all gone are exactly what needs reclaiming, and
    /// a table-driven sweep would never look at them.
    ///
    /// Each org gets its own acting scope, because a sweep is not a request and
    /// inherits none — and without one, RLS denies everything and the sweep
    /// would decide every object was an orphan.
    pub async fn artifacts_sweep_all(
        &self,
        storage: &dyn ArtifactStorage,
        min_age: Duration,
    ) -> Result<ArtifactSweep> {
        let mut total = ArtifactSweep::default();
        for org in storage.orgs().await? {
            let swept = acting::scope(
                ActingScope::new(org, "system", true),
                self.artifacts_sweep(org, storage, min_age),
            )
            .await?;
            total.add(swept);
        }
        Ok(total)
    }
}

fn row_to_artifact(r: &sqlx::postgres::PgRow) -> Result<Artifact> {
    let created_at: DateTime<Utc> = r.try_get("created_at")?;
    Ok(Artifact {
        id: r.try_get::<Uuid, _>("id")?.to_string(),
        org_id: r.try_get::<Uuid, _>("org_id")?.to_string(),
        sha256: r.try_get("sha256")?,
        mime: r.try_get("mime")?,
        bytes: r.try_get("bytes")?,
        filename: r.try_get("filename")?,
        created_by: r.try_get("created_by")?,
        // Same millisecond-precision ISO shape every other row in this schema
        // serializes as, so a mixed listing sorts and renders identically.
        created_at: created_at.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string(),
    })
}
