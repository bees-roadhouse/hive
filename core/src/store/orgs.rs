// PLACEHOLDER — the orgs / memberships / RLS workstream owns this.
//
// It exists so `artifacts` can carry its `org_id` FK and its RLS policy in
// their FINAL shape from the first commit rather than being retrofitted onto a
// live table later. Two things live here and both are meant to be replaced
// wholesale, not extended:
//
//   * `orgs_default` — the acting org, until sessions carry one. Real
//     resolution is "the org this session authenticated into" (docs/WEB-APP.md:
//     an agent authenticates into ONE org per session and cannot switch).
//   * `org_tx` — the transaction that sets `hive.acting_org`, which is what
//     every RLS policy keys off. Once middleware sets the session variable per
//     request this becomes redundant; setting it again inside a transaction to
//     the same value is harmless, so the two can overlap during integration.

use anyhow::Result;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use super::Store;

/// Slug of the placeholder org every artifact lands in until sessions carry one.
pub const DEFAULT_ORG_SLUG: &str = "default";

impl Store {
    /// The acting org. Get-or-create so a fresh install can store a file before
    /// anyone has created an organisation.
    pub async fn orgs_default(&self) -> Result<Uuid> {
        let existing: Option<Uuid> =
            crate::pgq::query_scalar::<Uuid>("SELECT id FROM orgs WHERE slug = ?")
                .bind(DEFAULT_ORG_SLUG)
                .fetch_optional(self.db())
                .await?;
        if let Some(id) = existing {
            return Ok(id);
        }
        let id: Uuid = crate::pgq::query_scalar::<Uuid>(
            "INSERT INTO orgs (id, slug, name) VALUES (?, ?, ?) \
             ON CONFLICT (slug) DO UPDATE SET slug = excluded.slug \
             RETURNING id",
        )
        .bind(Uuid::new_v4())
        .bind(DEFAULT_ORG_SLUG)
        .bind("Default")
        .fetch_one(self.db())
        .await?;
        Ok(id)
    }

    /// A transaction with `hive.acting_org` set, which is the whole of what the
    /// RLS policies read. `set_config(..., true)` IS `SET LOCAL`, so it dies
    /// with the transaction and cannot leak onto the next borrower of this
    /// pooled connection.
    pub async fn org_tx(&self, org: Uuid) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.db().begin().await?;
        sqlx::query("SELECT set_config('hive.acting_org', $1, true)")
            .bind(org.to_string())
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }
}
