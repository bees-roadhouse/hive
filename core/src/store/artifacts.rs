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
// The cost of that choice is that delete has to refcount, which it does under
// an advisory lock keyed on (org, sha256) so a concurrent upload of the same
// bytes cannot slip between "no rows left" and the unlink. Bytes are unlinked
// only when the last row referencing them in that org is gone; nothing is ever
// orphaned, and no org's delete can reach another org's bytes (the content
// address is org-scoped — see artifact_storage.rs).

use anyhow::Result;
use chrono::{DateTime, Utc};
use hive_shared::Artifact;
use serde_json::json;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use super::Store;
use crate::artifact_storage::{ArtifactStorage, ArtifactWrite};

const ARTIFACT_COLS: &str = "id, org_id, sha256, mime, bytes, filename, created_by, created_at";

/// Serialize same-blob writes and deletes within an org. Transaction-scoped, so
/// it releases on commit or rollback with nothing to clean up.
async fn lock_blob(tx: &mut Transaction<'_, Postgres>, org: Uuid, sha256: &str) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("artifact:{org}:{sha256}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

impl Store {
    /// Land `write`'s bytes and record them. The long part — streaming the
    /// payload into a temp file — has already happened by the time this is
    /// called; the transaction here only covers the instant rename into the
    /// content address plus the INSERT, so a 200 MB upload never holds a
    /// database transaction open while it transfers.
    pub async fn artifacts_create(
        &self,
        org: Uuid,
        write: Box<dyn ArtifactWrite>,
        mime: &str,
        filename: Option<&str>,
        created_by: Option<&str>,
    ) -> Result<Artifact> {
        // Hash first: the advisory lock is keyed on the content address, which
        // is only known once the bytes are through the hasher.
        let staged = write.commit().await?;

        let mut tx = self.db().begin().await?; // acting org is stamped on the connection by the pool hooks (core/src/acting.rs); the explicit org_id predicates below are belt to that policy's braces
        lock_blob(&mut tx, org, &staged.sha256).await?;
        let row = crate::pgq::query(&format!(
            "INSERT INTO artifacts (id, org_id, sha256, mime, bytes, filename, created_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING {ARTIFACT_COLS}"
        ))
        .bind(Uuid::new_v4())
        .bind(org)
        .bind(&staged.sha256)
        .bind(mime)
        .bind(staged.bytes as i64)
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
                "deduped": staged.deduped,
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
    /// Row and refcount are one transaction under the blob lock, so a
    /// concurrent upload of the same bytes either takes the lock first (and is
    /// counted) or waits (and re-lands the bytes it holds a temp file for).
    /// The unlink follows the commit: unlinking first would strand a live row
    /// if the transaction then rolled back.
    pub async fn artifacts_delete(
        &self,
        org: Uuid,
        id: Uuid,
        storage: &dyn ArtifactStorage,
    ) -> Result<Option<Artifact>> {
        let mut tx = self.db().begin().await?; // acting org is stamped on the connection by the pool hooks (core/src/acting.rs); the explicit org_id predicates below are belt to that policy's braces

        let Some(row) = crate::pgq::query(&format!(
            "SELECT {ARTIFACT_COLS} FROM artifacts WHERE id = ? AND org_id = ?"
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
        crate::pgq::query("DELETE FROM artifacts WHERE id = ? AND org_id = ?")
            .bind(id)
            .bind(org)
            .execute(&mut *tx)
            .await?;
        let remaining: i64 = crate::pgq::query_scalar::<i64>(
            "SELECT count(*) FROM artifacts WHERE org_id = ? AND sha256 = ?",
        )
        .bind(org)
        .bind(&artifact.sha256)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;

        if remaining == 0 {
            storage.remove(org, &artifact.sha256).await?;
        }

        self.emit(
            "artifact.deleted",
            "system",
            json!({"id": artifact.id, "bytesUnlinked": remaining == 0}),
        )
        .await?;
        Ok(Some(artifact))
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
