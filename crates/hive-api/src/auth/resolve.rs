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
/// Builtin-mode resolution (Phase 2+):
/// - dev principals (only ever from the dev-bypass, §5.8) get full authority —
///   handled before this is called, but covered here for completeness with a
///   no-authority result so a *forged* dev-type JWT can never gain anything.
/// - HUMAN principals resolve from the token's own claims (`scope`,
///   `hive_admin`, `hive_visibility`). The AS read the user's record once at
///   mint time and baked the resolved permissions in (§6.1), so the RS stays
///   stateless — no per-request DB hit.
/// - AI principals (Phase 6) resolve from the token's baked claims too. The AS
///   computed the **intersection** (the grant ∩ the connecting human's own
///   permissions, §3.4) at mint time and baked the result into `scope` /
///   `hive_visibility`, so the RS rebuilds it statelessly here — same as the
///   human path. The intersection ceiling lives at issue time; RLS (Phase 8)
///   re-enforces it at query time. `hive_admin` is never set on an AI token
///   (mint forces it false), so even a baked-admin claim can't elevate an AI.
///
/// Phase 9 adds the `external` (IdP-claim) branch behind `auth_policy.authz_mode`;
/// because everything reads this one function's output, that's a local change.
pub fn resolve_permissions(claims: &Claims) -> ResolvedPermissions {
    match claims.principal_kind() {
        // A real JWT must never assert principal_type=dev; treat it as no-authority.
        PrincipalType::Dev => ResolvedPermissions::none(),
        // Humans: trust the AS-baked claims (resolved once at mint).
        PrincipalType::Human => ResolvedPermissions::from_claims(claims),
        // AI: the AS baked the intersection at mint (§3.4). Force is_admin off
        // regardless of the claim — an AI is never an admin (§5.5).
        PrincipalType::Ai => {
            let mut perms = ResolvedPermissions::from_claims(claims);
            perms.is_admin = false;
            perms
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
            hive_admin: true,
            hive_visibility: Some("shared".to_string()),
        }
    }

    #[test]
    fn human_resolves_from_baked_claims() {
        // Phase 2: humans get the permissions the AS baked into the token.
        let human = resolve_permissions(&claims_with_type(Some("human")));
        assert!(human.has_scope("hive.read"));
        assert!(human.has_scope("hive.write"));
        assert!(human.is_admin);
        assert_eq!(
            human.data_visibility,
            super::super::claims::DataVisibility::Shared
        );
    }

    #[test]
    fn ai_resolves_from_baked_intersection_but_never_admin() {
        // Phase 6: an AI token carries the AS-baked intersection in its claims;
        // resolve reads it like a human — EXCEPT is_admin is forced off, even
        // though claims_with_type sets hive_admin=true. An AI is never admin.
        let ai = resolve_permissions(&claims_with_type(Some("ai")));
        assert!(ai.has_scope("hive.read"));
        assert!(ai.has_scope("hive.write"));
        assert!(!ai.is_admin, "an AI principal must never resolve to admin");
    }

    #[test]
    fn a_token_claiming_dev_type_gets_no_authority() {
        // Defense: a real JWT must not be able to assert dev authority, even if
        // it also carries hive_admin=true + scopes.
        let forged = resolve_permissions(&claims_with_type(Some("dev")));
        assert!(forged.scopes.is_empty());
        assert!(!forged.is_admin);
    }
}
