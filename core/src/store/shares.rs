// Visibility shares (store.ts `shares`). Owned by core-stores.

use anyhow::Result;
use hive_shared::{NewShare, Share, ShareScope};
use serde_json::json;
use sqlx::Row;

use super::{new_id, now_iso, Store};

impl Store {
    /// Idempotent — returns the existing row if the (scope, ref, viewer) triple exists.
    pub async fn shares_create(&self, input: NewShare) -> Result<Share> {
        let existing =
            crate::pgq::query("SELECT * FROM shares WHERE scope=? AND ref=? AND viewer=?")
                .bind(input.scope.as_str())
                .bind(&input.ref_)
                .bind(&input.viewer)
                .fetch_optional(self.db())
                .await?;
        if let Some(row) = existing {
            return row_to_share(&row);
        }
        let s = Share {
            id: new_id("shr"),
            scope: input.scope,
            ref_: input.ref_,
            viewer: input.viewer,
            created_at: now_iso(),
        };
        crate::pgq::query(
            "INSERT INTO shares (id, scope, ref, viewer, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&s.id)
        .bind(s.scope.as_str())
        .bind(&s.ref_)
        .bind(&s.viewer)
        .bind(&s.created_at)
        .execute(self.db())
        .await?;
        self.emit(
            "share.created",
            "system",
            json!({"scope": s.scope.as_str(), "ref": s.ref_, "viewer": s.viewer}),
        )
        .await?;
        Ok(s)
    }

    pub async fn shares_for_viewer(&self, viewer: &str) -> Result<Vec<Share>> {
        let rows =
            crate::pgq::query("SELECT * FROM shares WHERE viewer=? ORDER BY created_at DESC")
                .bind(viewer)
                .fetch_all(self.db())
                .await?;
        rows.iter().map(row_to_share).collect()
    }
}

pub(crate) fn row_to_share(r: &sqlx::postgres::PgRow) -> Result<Share> {
    Ok(Share {
        id: r.try_get("id")?,
        scope: ShareScope::from_str_lossy(r.try_get::<String, _>("scope")?.as_str()),
        ref_: r.try_get("ref")?,
        viewer: r.try_get("viewer")?,
        created_at: r.try_get("created_at")?,
    })
}
