//! The `resolve_permissions` chokepoint (hive-auth-mcp-design.md §6.1).
//!
//! This is the ONE place the source of authority is decided. Every enforcement
//! point downstream reads the `ResolvedPermissions` this returns and never
//! looks at where it came from. Today the source is internal (dev-bypass →
//! full; real principals → a deny stub until Phase 2/6 add the role/grant
//! tables). Later, an `external` branch keyed on IdP claims slots in here —
//! routes and RLS don't change. That's the whole point of routing through one
//! function from the start.

use super::claims::{Claims, PrincipalType, ResolvedPermissions};

/// Resolve a validated token's claims into hive's internal permission
/// vocabulary.
///
/// Phase 1 builtin behavior:
/// - dev principals (only ever from the dev-bypass, §5.8) get full authority
///   — handled before this is called, but covered here for completeness.
/// - every real principal resolves to deny-by-default (`none()`), because the
///   role/grant tables don't exist yet. This is SAFE: Phase 1 runs warn-only,
///   so a real token that resolves to no permissions is logged, not rejected.
///
/// Phase 2/6 replace the real-principal branch with reads of the local role +
/// `ai_access_grants` tables; Phase 9 adds the `external` (IdP-claim) branch
/// behind `auth_policy.authz_mode`. The signature is the stable seam.
pub fn resolve_permissions(claims: &Claims) -> ResolvedPermissions {
    match claims.principal_kind() {
        // A real JWT must never assert principal_type=dev; treat it as no-authority.
        PrincipalType::Dev => ResolvedPermissions::none(),
        PrincipalType::Human | PrincipalType::Ai => {
            // BUILTIN / today: no role tables yet -> deny-by-default stub.
            // (Phase 2/6: read local grants. Phase 9: external IdP-claim branch.)
            ResolvedPermissions::none()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims_with_type(pt: Option<&str>) -> Claims {
        Claims {
            iss: "http://127.0.0.1:7878".to_string(),
            sub: "u-123".to_string(),
            principal_type: pt.map(str::to_string),
            act: None,
            aud: Some("http://127.0.0.1:7878".to_string()),
            exp: None,
            iat: None,
            nbf: None,
            jti: None,
            scope: Some("hive.read hive.write".to_string()),
        }
    }

    #[test]
    fn real_principals_resolve_to_deny_stub_in_phase1() {
        let human = resolve_permissions(&claims_with_type(Some("human")));
        assert!(human.scopes.is_empty());
        assert!(!human.is_admin);

        let ai = resolve_permissions(&claims_with_type(Some("ai")));
        assert!(ai.scopes.is_empty());
        assert!(!ai.is_admin);
    }

    #[test]
    fn a_token_claiming_dev_type_gets_no_authority() {
        // Defense: a real JWT must not be able to assert dev authority.
        let forged = resolve_permissions(&claims_with_type(Some("dev")));
        assert!(forged.scopes.is_empty());
        assert!(!forged.is_admin);
    }
}
