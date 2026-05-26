//! External-IdP federation SEAM (hive-auth-mcp-design.md §6, §6.1).
//!
//! Phase 9, the last phase. This is a SEAM, not a provider: it lands the
//! abstraction (the `IdentityProvider` trait), the builtin default impl, and the
//! external-authZ resolver that reads `idp_permission_map` — so a concrete OIDC
//! provider (authentik/keycloak/Entra/…) drops in later as a thin trait impl +
//! config, with zero changes to call sites (routes, RLS, the resolve chokepoint
//! all consume `ResolvedPermissions`, never the source).
//!
//! Nothing here authenticates against a real IdP. `external` mode is INERT until
//! someone (a) writes a provider adapter and (b) populates `idp_permission_map`.
//! With the map empty, the external authZ resolver grants nothing — fails closed.

use hive_db::PgPool;

use super::claims::{Claims, DataVisibility, ResolvedPermissions};
use super::policy::AuthzMode;
use super::store::StoreError;

/// The federation abstraction (§6). The built-in AS is the default impl; an
/// external provider is a future drop-in. Kept deliberately small — the three
/// things that differ between "hive is the AS" and "an external IdP is the AS":
/// how a token verifies, how an external identity maps to a local user, and how
/// claims map to hive's internal permission vocabulary.
///
/// Not wired into the request hot path yet (the builtin path stays direct);
/// it's the boundary a provider implements. `async_trait`-free: the methods that
/// would be async on a real provider (network calls to the IdP) are modeled as
/// the resolver functions below for the builtin case, and a real impl can take
/// its own async shape behind this trait when added.
pub trait IdentityProvider: Send + Sync {
    /// Stable provider id (matches `users.external_idp` + `idp_permission_map.provider`).
    fn id(&self) -> &str;

    /// Does this provider own MFA? External IdPs do (hive then runs `delegated`
    /// MFA mode, §4); the builtin provider does not.
    fn owns_mfa(&self) -> bool;

    /// Does this provider own AUTHORIZATION? When true, hive sources roles from
    /// the provider's claims via `idp_permission_map` instead of local grants
    /// (§6.1). The builtin provider owns its own authZ (returns false).
    fn owns_authz(&self) -> bool;
}

/// The built-in AS as an `IdentityProvider` — the default, today's behavior.
/// hive verifies its own tokens, maps nothing externally, owns its own authZ.
pub struct BuiltinProvider;

impl IdentityProvider for BuiltinProvider {
    fn id(&self) -> &str {
        "builtin"
    }
    fn owns_mfa(&self) -> bool {
        false
    }
    fn owns_authz(&self) -> bool {
        false
    }
}

/// A row from `idp_permission_map` (§6.1): an external claim value → internal
/// permission grant.
#[derive(Debug, Clone)]
pub struct IdpMapping {
    pub grant_scopes: Vec<String>,
    pub data_visibility: Option<String>,
    pub is_admin: bool,
    pub priority: i32,
}

/// Resolve permissions from an EXTERNAL IdP's claims (§6.1) by mapping the
/// principal's role/group claim values through `idp_permission_map`. This is the
/// `authz_mode = external` branch of the resolve chokepoint.
///
/// INERT by construction: with an empty map (no provider wired) this returns
/// deny-by-default (`ResolvedPermissions::none()`), so flipping the mode on
/// before configuring the map fails CLOSED rather than open. When rows exist, a
/// user matching multiple gets the UNION of scopes, the highest data_visibility,
/// and admin-if-any (merged by priority) — per §6.1.
///
/// The claim that carries roles + the values are read from the token `Claims`.
/// Phase 9 reads them generically (the builtin token has none, so external mode
/// on a builtin token = deny — correct, since a builtin token isn't an IdP
/// token). A real provider populates these claims from the IdP token.
pub async fn resolve_external(
    pool: &PgPool,
    provider: &str,
    claim: &str,
    claim_values: &[String],
) -> Result<ResolvedPermissions, StoreError> {
    if claim_values.is_empty() {
        return Ok(ResolvedPermissions::none());
    }
    let rows = sqlx::query_as::<_, (Vec<String>, Option<String>, bool, i32)>(
        "SELECT grant_scopes, data_visibility, is_admin, priority \
         FROM idp_permission_map \
         WHERE provider = $1 AND claim = $2 AND claim_value = ANY($3) \
         ORDER BY priority DESC",
    )
    .bind(provider)
    .bind(claim)
    .bind(claim_values)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        // Unmapped claims grant nothing (deny-by-default, §6.1).
        return Ok(ResolvedPermissions::none());
    }

    let mappings: Vec<IdpMapping> = rows
        .into_iter()
        .map(|r| IdpMapping {
            grant_scopes: r.0,
            data_visibility: r.1,
            is_admin: r.2,
            priority: r.3,
        })
        .collect();
    Ok(merge_mappings(&mappings))
}

/// Merge multiple matched mappings (§6.1): union of scopes, highest
/// data_visibility, admin-if-any. Pure + testable.
fn merge_mappings(mappings: &[IdpMapping]) -> ResolvedPermissions {
    let mut scopes: Vec<String> = Vec::new();
    let mut is_admin = false;
    let mut visibility = DataVisibility::Owner; // narrowest baseline

    for m in mappings {
        for s in &m.grant_scopes {
            if !scopes.contains(s) {
                scopes.push(s.clone());
            }
        }
        is_admin = is_admin || m.is_admin;
        if let Some(v) = m.data_visibility.as_deref() {
            let cand = DataVisibility::parse(Some(v));
            visibility = highest_visibility(visibility, cand);
        }
    }

    ResolvedPermissions {
        scopes,
        data_visibility: visibility,
        is_admin,
    }
}

/// "Highest" visibility = broadest: shared > custom > owner.
fn highest_visibility(a: DataVisibility, b: DataVisibility) -> DataVisibility {
    fn rank(v: &DataVisibility) -> u8 {
        match v {
            DataVisibility::Shared => 2,
            DataVisibility::Custom => 1,
            DataVisibility::Owner => 0,
        }
    }
    if rank(&a) >= rank(&b) { a } else { b }
}

/// The roles claim values a token carries for external authZ. Reads a
/// space/comma-delimited custom claim if present. Builtin tokens have none → the
/// external resolver then denies (correct: a builtin token isn't an IdP token).
/// A real provider sets these from the IdP token's `groups`/`roles` claim.
pub fn external_claim_values(claims: &Claims, _claim: &str) -> Vec<String> {
    // Phase 9 seam: the generic Claims struct has no roles claim yet (a provider
    // adapter would add one). Return empty so external mode is inert until then.
    let _ = claims;
    Vec::new()
}

/// Whether the resolve chokepoint should take the external-authZ branch: only
/// when `authz_mode = external`. Centralized so the chokepoint reads one thing.
pub fn use_external_authz(mode: AuthzMode) -> bool {
    mode.is_external()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_provider_owns_nothing_external() {
        let p = BuiltinProvider;
        assert_eq!(p.id(), "builtin");
        assert!(!p.owns_mfa());
        assert!(!p.owns_authz());
    }

    #[test]
    fn empty_mappings_deny() {
        // The inert case: no rows => no authority (fails closed).
        let merged = merge_mappings(&[]);
        assert!(merged.scopes.is_empty());
        assert!(!merged.is_admin);
        assert_eq!(merged.data_visibility, DataVisibility::Owner);
    }

    #[test]
    fn merge_unions_scopes_takes_highest_visibility_and_admin_if_any() {
        let mappings = vec![
            IdpMapping {
                grant_scopes: vec!["journal.read".into()],
                data_visibility: Some("owner".into()),
                is_admin: false,
                priority: 100,
            },
            IdpMapping {
                grant_scopes: vec!["journal.read".into(), "tasks.read".into()],
                data_visibility: Some("shared".into()),
                is_admin: true,
                priority: 50,
            },
        ];
        let merged = merge_mappings(&mappings);
        // union (dedup)
        assert!(merged.scopes.contains(&"journal.read".to_string()));
        assert!(merged.scopes.contains(&"tasks.read".to_string()));
        assert_eq!(merged.scopes.len(), 2);
        // highest visibility (shared beats owner)
        assert_eq!(merged.data_visibility, DataVisibility::Shared);
        // admin-if-any
        assert!(merged.is_admin);
    }

    #[test]
    fn highest_visibility_orders_shared_custom_owner() {
        assert_eq!(
            highest_visibility(DataVisibility::Owner, DataVisibility::Shared),
            DataVisibility::Shared
        );
        assert_eq!(
            highest_visibility(DataVisibility::Custom, DataVisibility::Owner),
            DataVisibility::Custom
        );
    }

    #[test]
    fn external_claim_values_empty_until_provider() {
        let claims = Claims {
            iss: "x".into(),
            sub: "y".into(),
            principal_type: Some("human".into()),
            act: None,
            aud: None,
            exp: None,
            iat: None,
            nbf: None,
            jti: None,
            scope: None,
            hive_admin: false,
            hive_visibility: None,
        };
        assert!(external_claim_values(&claims, "groups").is_empty());
    }
}
