//! The acting scope: ONE org per session, ambient for the duration of a task.
//!
//! `docs/WEB-APP.md`'s rule is that a session carries an acting org, not a set
//! of orgs. This module is that rule made mechanical, and it is deliberately
//! shaped so the rule cannot be broken by a caller:
//!
//! * The scope lives in a `tokio` task-local. There is no setter — the ONLY
//!   way to establish one is [`scope`], which wraps a future, so a scope can
//!   never outlive the request that opened it and can never be swapped
//!   mid-request. An agent cannot switch orgs because there is no call that
//!   switches orgs.
//! * [`current`] is the single read point. `core/src/db.rs` uses it in the
//!   pool's `before_acquire` / `after_connect` hooks to stamp
//!   `hive.acting_org` onto every connection at the moment it enters use, so
//!   every statement the store issues runs under the acting org's RLS policy.
//! * Absence is deny-all, not bypass. With no scope the GUC is stamped empty,
//!   the policy predicate is NULL, reads return nothing and writes fail the
//!   `NOT NULL` on `org_id`. Unscoped code fails closed and loudly.
//!
//! Work moved off the request task (`tokio::spawn`) loses the scope and hits
//! that deny-all. That is the intended failure: a detached task has no session
//! and therefore no acting org.

use std::future::Future;

use uuid::Uuid;

/// Who a unit of work is acting as, and inside which org.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActingScope {
    /// The one org this task reads and writes. Never a set.
    pub org: Uuid,
    /// The authenticated human whose namespace this work runs in
    /// (`users.actor`); an agent token carries the granting human.
    pub user: String,
    /// Admin inside `org` — widens the RLS predicate past the per-user
    /// namespace, never past the org.
    pub admin: bool,
}

impl ActingScope {
    pub fn new(org: Uuid, user: impl Into<String>, admin: bool) -> Self {
        Self {
            org,
            user: user.into(),
            admin,
        }
    }
}

tokio::task_local! {
    static ACTING: ActingScope;
}

/// The acting scope of the current task, or `None` outside a [`scope`] (boot,
/// migrations, unauthenticated requests, detached tasks).
pub fn current() -> Option<ActingScope> {
    ACTING.try_with(|s| s.clone()).ok()
}

/// Run `fut` with `scope` as the acting scope. The only way to set one.
pub async fn scope<F: Future>(scope: ActingScope, fut: F) -> F::Output {
    ACTING.scope(scope, fut).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_scope_does_not_leak_outside_its_future() {
        assert!(current().is_none());
        let org = Uuid::from_u128(7);
        scope(ActingScope::new(org, "nate", false), async {
            assert_eq!(current().unwrap().org, org);
        })
        .await;
        assert!(current().is_none());
    }

    #[tokio::test]
    async fn sibling_tasks_do_not_see_each_other_s_scope() {
        let a = tokio::spawn(scope(
            ActingScope::new(Uuid::from_u128(1), "a", false),
            async {
                tokio::task::yield_now().await;
                current().unwrap().org
            },
        ));
        let b = tokio::spawn(scope(
            ActingScope::new(Uuid::from_u128(2), "b", false),
            async {
                tokio::task::yield_now().await;
                current().unwrap().org
            },
        ));
        assert_eq!(a.await.unwrap(), Uuid::from_u128(1));
        assert_eq!(b.await.unwrap(), Uuid::from_u128(2));
    }

    #[tokio::test]
    async fn a_detached_task_inherits_nothing() {
        let org = Uuid::from_u128(3);
        let seen = scope(ActingScope::new(org, "nate", false), async {
            tokio::spawn(async { current() }).await.unwrap()
        })
        .await;
        assert!(seen.is_none(), "spawned work must fall out of scope");
    }
}
