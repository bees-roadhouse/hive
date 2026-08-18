// The legacy bio/role → profile-card backfill, which `main` runs on every boot.
//
// It reads `people`, which is row-secured, and boot is not a request: it
// carries no acting scope. Run bare, the SELECT matched zero rows in every org
// and the backfill reported a successful migration of nothing — a permanent
// silent no-op that looked like a completed migration. The driver gives each
// org its own scope, so this asserts across TWO of them: a single-org pass
// would still be green if the driver just borrowed the default org.

use hive_core::acting::{self, ActingScope};
use hive_core::db;
use hive_core::store::Store;
use uuid::Uuid;

/// Seed one legacy person carrying a `bio`, inside `org`.
///
/// Written as raw SQL on purpose. `people_update` mirrors every bio edit into
/// the profile card, so a person seeded through it is already migrated and the
/// backfill correctly has nothing to do. A legacy row is precisely the one that
/// predates that mirroring: a `people.bio` with no card behind it. `org_id`
/// comes from the column default, which reads the acting scope.
async fn seed_legacy_person(store: &Store, org: Uuid, slug: &str, bio: &str) {
    acting::scope(ActingScope::new(org, "tests".to_string(), true), async {
        hive_core::pgq::query(
            "INSERT INTO people (id, name, slug, kind, bio, created_at) \
             VALUES (?, ?, ?, 'human', ?, now()::text)",
        )
        .bind(format!("ppl_legacy_{slug}"))
        .bind(slug)
        .bind(slug)
        .bind(bio)
        .execute(store.db())
        .await
        .expect("seed legacy person");
    })
    .await;
}

/// Read back the profile card's `bio` section for `slug` in `org`.
async fn card_bio(store: &Store, org: Uuid, slug: &str) -> Option<String> {
    acting::scope(ActingScope::new(org, "tests".to_string(), true), async {
        store
            .profile_get(slug)
            .await
            .expect("profile_get")
            .and_then(|c| c.body.sections.get("bio").cloned())
    })
    .await
}

#[tokio::test]
async fn the_boot_backfill_reaches_every_org_without_a_scope() {
    // No fallback scope — the binary's condition at boot, not a helper's.
    let test_db = db::test_pool_strict().await;
    let store = Store::new(test_db.pool.clone());

    let default_org = acting::scope(
        ActingScope::new(db::DEFAULT_ORG_ID, "tests".to_string(), true),
        async { store.orgs_default().await.expect("default org") },
    )
    .await;
    let beta = store.orgs_create("beta", "Beta").await.expect("second org");

    seed_legacy_person(&store, default_org.id, "nate", "runs the roadhouse").await;
    seed_legacy_person(&store, beta.id, "bob", "runs beta").await;

    // Seed sanity, so a failure below points at the backfill and not the setup.
    let all = store.orgs_all().await.expect("orgs_all");
    assert_eq!(all.len(), 2, "orgs_all must see both orgs unscoped");
    let seeded_bio = acting::scope(
        ActingScope::new(default_org.id, "tests".to_string(), true),
        async {
            store
                .people_by_slug("nate")
                .await
                .expect("people_by_slug")
                .and_then(|p| p.bio)
        },
    )
    .await;
    assert_eq!(seeded_bio.as_deref(), Some("runs the roadhouse"));

    // Exactly how `main` calls it: bare, holding no scope of its own.
    let migrated = store
        .backfill_identity_cards_all()
        .await
        .expect("backfill must not error");

    assert_eq!(
        migrated, 2,
        "both orgs' legacy people must be folded into cards; \
         0 here is the silent no-op this test exists for"
    );
    assert_eq!(
        card_bio(&store, default_org.id, "nate").await.as_deref(),
        Some("runs the roadhouse")
    );
    assert_eq!(
        card_bio(&store, beta.id, "bob").await.as_deref(),
        Some("runs beta"),
        "a second org must be visited too, not just the default"
    );

    // Idempotent, because it runs on every boot: a second pass fills nothing.
    let again = store
        .backfill_identity_cards_all()
        .await
        .expect("second pass");
    assert_eq!(again, 0, "re-running the backfill must migrate nothing");
}

/// The trap itself, pinned: the scoped worker called bare — which is what
/// `main` used to do — silently matches nothing rather than failing. Anything
/// that reaches for `backfill_identity_cards` from a scopeless context gets
/// this, so the driver is the only correct caller from boot.
#[tokio::test]
async fn the_scoped_worker_alone_is_a_silent_no_op() {
    let test_db = db::test_pool_strict().await;
    let store = Store::new(test_db.pool.clone());
    let org = acting::scope(
        ActingScope::new(db::DEFAULT_ORG_ID, "tests".to_string(), true),
        async { store.orgs_default().await.expect("default org") },
    )
    .await;
    seed_legacy_person(&store, org.id, "nate", "runs the roadhouse").await;

    let bare = store
        .backfill_identity_cards()
        .await
        .expect("unscoped call does not error — that is the whole problem");
    assert_eq!(bare, 0, "no scope means no rows, quietly");

    // Same store, same data, through the driver: the work actually happens.
    let driven = store.backfill_identity_cards_all().await.expect("driver");
    assert_eq!(driven, 1);
}
