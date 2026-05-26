//! In-memory revoked-`jti` set for the non-expiring MCP-token class
//! (hive-auth-mcp-design.md §5.5).
//!
//! MCP/AI tokens default to no expiry (§2), so revocation is the *only* off
//! switch. Every AI-token validation must check the token's `jti` against the
//! revocation set. A per-request DB round-trip would be too costly, so we keep
//! a small in-memory `HashSet<Uuid>` backed by the `revocations` table: loaded
//! once at startup, and refreshed in-process whenever we revoke (the API is the
//! only writer, single-node). If hive-api ever runs multi-node, this becomes a
//! shed-and-reload-on-notify or a shared cache — flagged, not needed now.

use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use hive_db::PgPool;
use uuid::Uuid;

/// Cheap-to-clone handle to the shared revoked-`jti` set.
#[derive(Clone, Default)]
pub struct RevocationSet {
    inner: Arc<RwLock<HashSet<Uuid>>>,
}

impl RevocationSet {
    /// Build an empty set (used before the DB load / in tests).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load all revoked `jti`s from the `revocations` table into memory.
    pub async fn load(pool: &PgPool) -> Result<Self, hive_db::Error> {
        let rows = sqlx::query_as::<_, (Uuid,)>("SELECT jti FROM revocations")
            .fetch_all(pool)
            .await?;
        let set: HashSet<Uuid> = rows.into_iter().map(|r| r.0).collect();
        Ok(Self {
            inner: Arc::new(RwLock::new(set)),
        })
    }

    /// Is this `jti` revoked? Poisoned-lock-safe: a poisoned set fails CLOSED
    /// (treats the token as revoked) so a panic mid-write can't open a hole.
    pub fn is_revoked(&self, jti: &Uuid) -> bool {
        match self.inner.read() {
            Ok(set) => set.contains(jti),
            Err(_) => true,
        }
    }

    /// Mark one `jti` revoked in memory (call after the DB row is written).
    pub fn insert(&self, jti: Uuid) {
        if let Ok(mut set) = self.inner.write() {
            set.insert(jti);
        }
    }

    /// Mark many `jti`s revoked at once (the multi-token revocation scopes).
    pub fn insert_many(&self, jtis: impl IntoIterator<Item = Uuid>) {
        if let Ok(mut set) = self.inner.write() {
            set.extend(jtis);
        }
    }

    /// Test/seam: current size.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.inner.read().map(|s| s.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_then_is_revoked() {
        let set = RevocationSet::empty();
        let jti = Uuid::now_v7();
        assert!(!set.is_revoked(&jti));
        set.insert(jti);
        assert!(set.is_revoked(&jti));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn insert_many_adds_all() {
        let set = RevocationSet::empty();
        let a = Uuid::now_v7();
        let b = Uuid::now_v7();
        set.insert_many([a, b]);
        assert!(set.is_revoked(&a));
        assert!(set.is_revoked(&b));
        assert_eq!(set.len(), 2);
    }
}
