//! What is left of application-level authorization after RLS took the rest.
//!
//! Row visibility is no longer decided here. `org_id = current_setting(...)`
//! is a Postgres policy on every content table, applied to every statement,
//! with no ordering to compose wrong and no code path that can forget it. That
//! was the D16 defect ("the ACL ordering defects") and it is gone with the
//! surface that carried it.
//!
//! Two gates remain, and neither is row filtering:
//!
//! * **Role.** "Only an admin may list users / mint tokens / reassign
//!   namespaces." A predicate on rows cannot say that — it is about the route,
//!   not the row.
//! * **Impersonation.** "May this principal act as identity X?" Also not a row
//!   predicate: it decides whose voice a write is made in, before any row
//!   exists.
//!
//! They live here rather than in `middleware.rs` so that file is authentication
//! and the acting-org rule, full stop. Anything tempted to grow into a
//! visibility check belongs in the policy in `core/src/db.rs`, not in this file.

use crate::middleware::AuthCtx;
use crate::store::Store;

/// May this principal read/act for `identity`'s private surfaces (recall
/// brief, inbox)? Admins: anyone in their org. Tokens: only their own actor.
/// Sessions: also the AIs the logged-in user owns. Shared by the MCP tools and
/// the HTTP routes so the two doors can't drift apart.
///
/// Every lookup this makes is already inside the acting org — `people` is
/// RLS-scoped — so an identity in another org is not merely refused, it is not
/// visible to be asked about.
pub async fn can_act_for_identity(
    store: &Store,
    ctx: &AuthCtx,
    identity: &str,
) -> anyhow::Result<bool> {
    if ctx.is_admin() || identity == ctx.actor() {
        return Ok(true);
    }
    if ctx.principal == Some("session") {
        let owner = ctx.namespace_user();
        return Ok(store
            .people_ais_owned_by(owner)
            .await?
            .iter()
            .any(|p| p.slug == identity));
    }
    Ok(false)
}
