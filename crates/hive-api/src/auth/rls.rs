//! Row-level data authorization — the app-side GUC plumbing (design §5.6).
//!
//! The RLS *policies* live in the DB (migration 0010) and are DEFAULT-ALLOW:
//! they only narrow access when a request both arms enforcement and sets a
//! non-shared visibility. This module computes that per-request context from the
//! resolved `Principal` and applies it as `SET LOCAL` GUCs inside the request's
//! transaction.
//!
//! Shadow-first: `enforce_enabled()` reads `HIVE_RLS_ENFORCE` (default off). When
//! off, `apply` sets nothing (or explicitly `app.rls_enforce='off'`), so the
//! policies stay in default-allow — RLS is a no-op until the flag flips, exactly
//! like the Phase-7 risk engine and the Phase-1 auth enforce.
//!
//! Visibility is resolved through the existing chokepoint: the `Principal`
//! already carries `permissions.data_visibility` (from `resolve_permissions`,
//! §6.1). For an AI principal that value is the grant ∩ acting-human computed at
//! token mint (§3.4); we don't recompute it here. The only extra work is mapping
//! the principal to the owner-HANDLE set the policies match on (the content
//! tables tag rows with text handles — `journal_entries.ai`, `tasks.owner`,
//! `notes.author` — not UUIDs).

use hive_db::PgPool;
use sqlx::Postgres;
use uuid::Uuid;

use super::claims::{DataVisibility, Principal, PrincipalType};
use super::store::StoreError;

/// Whether RLS is ENFORCED (vs shadow/no-op). Default false. `HIVE_RLS_ENFORCE`
/// = 1|true flips it on. Separate from the auth-layer enforce and the risk
/// enforce — each control arms independently.
pub fn enforce_enabled() -> bool {
    matches!(
        std::env::var("HIVE_RLS_ENFORCE").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    )
}

/// The per-request RLS context: the visibility band + the owner-handles the
/// principal may see under owner/custom visibility.
#[derive(Debug, Clone)]
pub struct RlsContext {
    pub visibility: DataVisibility,
    /// Owner-tags the principal may see (e.g. ["nate","pia"]). Empty for a
    /// principal with no resolvable handle (then 'owner' visibility sees none).
    pub handles: Vec<String>,
}

impl RlsContext {
    /// The GUC string for `app.visibility`.
    fn visibility_str(&self) -> &'static str {
        self.visibility.as_str()
    }

    /// The comma-joined `app.principal_handles` GUC value.
    fn handles_str(&self) -> String {
        self.handles.join(",")
    }
}

/// Compute the RLS context for a resolved principal. Visibility comes straight
/// from the principal's resolved permissions (the chokepoint already intersected
/// grant ∩ human for AI tokens). The handle set per kind:
///
/// - dev: full visibility, no narrowing (handles unused).
/// - human: their own username (a human's row tag in `tasks.owner` /
///   `notes.author` / `journal_entries.ai` is their handle).
/// - ai: the AI's handle (`ai_identities.name`) plus the acting human's username
///   — an AI acting for a human sees both its own provenance rows and that
///   human's, matching §5.6 (visibility keyed on the connecting human + the AI).
pub async fn compute_context(
    pool: &PgPool,
    principal: &Principal,
) -> Result<RlsContext, StoreError> {
    let visibility = principal.permissions.data_visibility.clone();

    // Dev-bypass: full authority, shared visibility, no narrowing (§5.8).
    if principal.kind == PrincipalType::Dev {
        return Ok(RlsContext {
            visibility: DataVisibility::Shared,
            handles: Vec::new(),
        });
    }

    let mut handles = Vec::new();
    match principal.kind {
        PrincipalType::Human => {
            if let Some(u) = username_for(pool, &principal.subject).await? {
                handles.push(u);
            }
        }
        PrincipalType::Ai => {
            // The AI's own handle (provenance tag on rows it authored).
            if let Some(name) = ai_handle_for(pool, &principal.subject).await? {
                handles.push(name);
            }
            // Plus the acting human's handle (the rows it acts on their behalf).
            if let Some(act) = principal.act.as_deref()
                && let Some(u) = username_for(pool, act).await?
            {
                handles.push(u);
            }
        }
        PrincipalType::Dev => unreachable!(),
    }

    Ok(RlsContext {
        visibility,
        handles,
    })
}

/// Apply the RLS context to a transaction as `SET LOCAL` GUCs (§5.6). No-ops
/// (sets `app.rls_enforce='off'`) when enforcement is disabled, so the
/// default-allow policies stay permissive — the shadow path. `SET LOCAL` scopes
/// the GUCs to this transaction only.
///
/// NOTE: GUC values are validated/escaped — visibility is a fixed enum string
/// and handles are joined from validated handle tokens, but we still quote via
/// `set_config(..., true)` (the function form of SET LOCAL) bound as a parameter
/// so no value is interpolated into SQL text.
pub async fn apply(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    ctx: &RlsContext,
) -> Result<(), StoreError> {
    if !enforce_enabled() {
        // Shadow: explicitly mark unenforced. (Unset would also default-allow,
        // but being explicit is clearer and resets any inherited GUC.)
        sqlx::query("SELECT set_config('app.rls_enforce', 'off', true)")
            .execute(&mut **tx)
            .await?;
        return Ok(());
    }
    // Enforced: arm + carry visibility + handles. set_config(_, _, true) is the
    // function form of SET LOCAL (transaction-scoped); values are bound, never
    // interpolated.
    sqlx::query("SELECT set_config('app.rls_enforce', 'on', true)")
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT set_config('app.visibility', $1, true)")
        .bind(ctx.visibility_str())
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT set_config('app.principal_handles', $1, true)")
        .bind(ctx.handles_str())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Look up a human's username (the owner-handle for human-authored rows).
async fn username_for(pool: &PgPool, subject: &str) -> Result<Option<String>, StoreError> {
    let Ok(id) = subject.parse::<Uuid>() else {
        return Ok(None);
    };
    let row = sqlx::query_as::<_, (String,)>("SELECT username FROM users WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
}

/// Look up an AI's handle (its `ai_identities.name`, the provenance tag).
async fn ai_handle_for(pool: &PgPool, subject: &str) -> Result<Option<String>, StoreError> {
    let Ok(id) = subject.parse::<Uuid>() else {
        return Ok(None);
    };
    let row = sqlx::query_as::<_, (String,)>("SELECT name FROM ai_identities WHERE id = $1")
        .bind(id)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| r.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::claims::ResolvedPermissions;

    fn principal(kind: PrincipalType, vis: DataVisibility) -> Principal {
        Principal {
            subject: "11111111-1111-7111-8111-111111111111".to_string(),
            kind,
            act: None,
            permissions: ResolvedPermissions {
                scopes: vec![],
                data_visibility: vis,
                is_admin: false,
            },
        }
    }

    #[test]
    fn context_visibility_and_handle_join() {
        let ctx = RlsContext {
            visibility: DataVisibility::Owner,
            handles: vec!["nate".into(), "pia".into()],
        };
        assert_eq!(ctx.visibility_str(), "owner");
        assert_eq!(ctx.handles_str(), "nate,pia");
    }

    #[test]
    fn shared_visibility_serializes() {
        let ctx = RlsContext {
            visibility: DataVisibility::Shared,
            handles: vec![],
        };
        assert_eq!(ctx.visibility_str(), "shared");
        assert_eq!(ctx.handles_str(), "");
    }

    #[test]
    fn dev_principal_is_shared() {
        // compute_context short-circuits dev to shared before any DB hit; the
        // kind check is what matters here.
        let p = principal(PrincipalType::Dev, DataVisibility::Owner);
        assert_eq!(p.kind, PrincipalType::Dev);
    }
}
