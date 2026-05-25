//! Token claims, the authenticated `Principal`, and the internal permission
//! vocabulary (`ResolvedPermissions`) from hive-auth-mcp-design.md §6.1.
//!
//! `ResolvedPermissions` is the *stable internal vocabulary* every downstream
//! enforcement point reads. Whether the permissions came from a validated JWT,
//! local role/grant tables, or (later) an external IdP's claims is decided in
//! one place — `resolve_permissions` (see `super::resolve`). Routes and RLS
//! consume the resolved output and stay agnostic to the source.

use serde::{Deserialize, Serialize};

/// JWT claims hive issues/validates. Phase 1 only *validates* (the AS core that
/// mints them lands in Phase 2); the shape matches §2's token model so Phase 2
/// can issue against it without churn.
///
/// `exp` is optional: human UI/CLI tokens carry it; non-expiring MCP/AI tokens
/// (§2 two-token-class) omit it. `act` is the RFC 8693 actor — the human who
/// connected an AI principal; `None` for human tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub iss: String,
    pub sub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_type: Option<String>, // "human" | "ai" (default human if absent)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub act: Option<String>, // connecting human's sub for an AI token (RFC 8693)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>, // absent => non-expiring (MCP/AI class)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    /// Space-delimited scope string (OAuth convention), e.g. "hive.read mcp".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Admin flag, resolved by the AS at mint time from the user's record so the
    /// RS stays stateless (§6.1: the AS reads roles once; the token carries the
    /// resolved result). Custom claim, namespaced to avoid colliding with OIDC.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hive_admin: bool,
    /// Data-visibility lever (§5.6), baked in at mint. "shared" | "owner" |
    /// "custom"; absent => owner (narrowest).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hive_visibility: Option<String>,
}

impl Claims {
    /// Parse the space-delimited `scope` claim into individual scopes.
    pub fn scopes(&self) -> Vec<String> {
        self.scope
            .as_deref()
            .map(|s| s.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    }

    pub fn principal_kind(&self) -> PrincipalType {
        match self.principal_type.as_deref() {
            Some("ai") => PrincipalType::Ai,
            Some("dev") => PrincipalType::Dev,
            _ => PrincipalType::Human,
        }
    }
}

/// The kind of subject a token authenticates. `Dev` only ever originates from
/// the dev-bypass (§5.8) and never from a real JWT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrincipalType {
    Human,
    Ai,
    Dev,
}

/// An authenticated principal as seen by handlers. Produced by the `AuthUser`
/// extractor after the auth layer resolves a request.
#[derive(Debug, Clone)]
pub struct Principal {
    pub subject: String,
    pub kind: PrincipalType,
    /// For an AI principal, the connecting human's subject (RFC 8693 `act`).
    pub act: Option<String>,
    pub permissions: ResolvedPermissions,
}

/// Data-visibility lever (§5.6). Maps to Postgres RLS in Phase 8; carried here
/// from Phase 1 so the vocabulary is stable from the start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataVisibility {
    /// Whole shared hive.
    Shared,
    /// Only the connecting human's own rows.
    Owner,
    /// Future fine-grained filter (jsonb-driven). Inert in Phase 1.
    Custom,
}

impl DataVisibility {
    /// Parse the `hive_visibility` claim; unknown/absent => narrowest (`Owner`).
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("shared") => DataVisibility::Shared,
            Some("custom") => DataVisibility::Custom,
            _ => DataVisibility::Owner,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            DataVisibility::Shared => "shared",
            DataVisibility::Owner => "owner",
            DataVisibility::Custom => "custom",
        }
    }
}

/// hive's stable INTERNAL permission vocabulary (§6.1). Every enforcement point
/// — route scope guards, RLS, admin checks, the AI-grant intersection — reads
/// this. It never changes when the *source* of authority changes.
#[derive(Debug, Clone)]
pub struct ResolvedPermissions {
    pub scopes: Vec<String>,
    pub data_visibility: DataVisibility,
    pub is_admin: bool,
}

impl ResolvedPermissions {
    /// Full authority. Phase 1 only hands this to the dev-bypass principal.
    pub fn full() -> Self {
        Self {
            scopes: vec!["*".to_string()],
            data_visibility: DataVisibility::Shared,
            is_admin: true,
        }
    }

    /// Deny-by-default: no scopes, narrowest visibility, not admin. Phase 1
    /// resolves real (non-dev) principals to this stub until Phase 2/6 add the
    /// role/grant tables. Safe because Phase 1 runs warn-only (§ layer).
    pub fn none() -> Self {
        Self {
            scopes: Vec::new(),
            data_visibility: DataVisibility::Owner,
            is_admin: false,
        }
    }

    /// Reconstruct from a validated token's claims (§6.1: the AS baked the
    /// resolved permissions into the token at mint, so the RS rebuilds them
    /// without a DB hit). This is the builtin-mode resolution for real
    /// principals from Phase 2 on.
    pub fn from_claims(claims: &Claims) -> Self {
        Self {
            scopes: claims.scopes(),
            data_visibility: DataVisibility::parse(claims.hive_visibility.as_deref()),
            is_admin: claims.hive_admin,
        }
    }

    /// Does the principal hold `scope` (or the `*` wildcard)?
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.iter().any(|s| s == "*" || s == scope)
    }
}
