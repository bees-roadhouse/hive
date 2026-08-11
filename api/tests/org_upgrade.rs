// The upgrade path: an install that predates orgs must keep working.
//
// The greenfield case is easy and proves little — a fresh database gets the
// tenancy layer at creation. The case that matters is Nate's: a v0.6 database
// full of prose, no `orgs` table, no `org_id` anywhere, booting against this
// code for the first time. Everything has to land in the default org, every
// existing login has to become a member of it, and nothing may be dropped.
//
// The v0.6 shape is reconstructed by stripping the tenancy layer back off a
// migrated schema — the same statements `apply_org_scoping` added, reversed —
// then legacy-shaped rows are written and `migrate` is run over them.

use hive_core::acting::{self, ActingScope};
use hive_core::db;
use sqlx::PgPool;
use uuid::Uuid;

/// Undo the tenancy layer on a few representative tables, so what is left is
/// what v0.6 actually had: content with no org column, and no orgs at all.
async fn rewind_to_v0_6(admin: &PgPool) {
    for table in ["journal", "people", "wire", "search"] {
        for sql in [
            format!("ALTER TABLE {table} DISABLE ROW LEVEL SECURITY"),
            format!("ALTER TABLE {table} NO FORCE ROW LEVEL SECURITY"),
            format!("DROP POLICY IF EXISTS {table}_org ON {table}"),
            format!("ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {table}_org_fk"),
            format!("ALTER TABLE {table} DROP COLUMN IF EXISTS org_id"),
        ] {
            sqlx::raw_sql(&sql).execute(admin).await.expect("rewind");
        }
    }
    // v0.6's uniqueness was global, not per-org.
    for sql in [
        "DROP INDEX IF EXISTS people_org_uniq",
        "CREATE UNIQUE INDEX IF NOT EXISTS people_slug_key ON people (slug)",
        "ALTER TABLE sessions   DROP COLUMN IF EXISTS org_id",
        "ALTER TABLE api_tokens DROP COLUMN IF EXISTS org_id",
        "DROP TABLE IF EXISTS memberships",
        "DROP TABLE IF EXISTS user_identities",
        "DROP TABLE IF EXISTS orgs CASCADE",
    ] {
        sqlx::raw_sql(sql).execute(admin).await.expect("rewind");
    }
}

/// Write rows the way v0.6 wrote them: no org column, so no org value.
async fn seed_v0_6(admin: &PgPool) {
    let now = hive_core::store::now_iso();
    sqlx::raw_sql(
        "INSERT INTO people (id, slug, name, kind, created_at) \
         VALUES ('ppl_legacy1', 'nate', 'Nate', 'human', now()::text)",
    )
    .execute(admin)
    .await
    .expect("seed person");
    hive_core::pgq::query(
        "INSERT INTO users (id, actor, email, name, role, password_hash, created_at) \
         VALUES ('usr_legacy1', 'nate', 'nate@example.com', 'Nate', 'admin', 'x', ?)",
    )
    .bind(&now)
    .execute(admin)
    .await
    .expect("seed user");
    for (id, body) in [
        ("jrnl_legacy1", "prose written before orgs existed"),
        ("jrnl_legacy2", "more of the same"),
    ] {
        hive_core::pgq::query(
            "INSERT INTO journal (id, author, body, tags, mentions, user_scope, created_at) \
             VALUES (?, 'nate', ?, '[]', '[]', NULL, ?)",
        )
        .bind(id)
        .bind(body)
        .bind(&now)
        .execute(admin)
        .await
        .expect("seed journal");
    }
    hive_core::pgq::query(
        "INSERT INTO sessions (id, token_hash, user_id, created_at, expires_at, last_seen) \
         VALUES ('ses_legacy1', 'deadbeef', 'usr_legacy1', ?, ?, ?)",
    )
    .bind(&now)
    .bind("2099-01-01T00:00:00.000Z")
    .bind(&now)
    .execute(admin)
    .await
    .expect("seed session");
}

#[tokio::test]
async fn a_pre_org_database_backfills_into_the_default_org() {
    // Start from a migrated schema, rewind it, seed it, then migrate again —
    // which is exactly what booting this code over a v0.6 database does.
    let pool = db::test_pool_strict().await;
    let admin = db::test_admin_pool(&pool).await;
    rewind_to_v0_6(&admin).await;
    seed_v0_6(&admin).await;

    db::migrate(&admin).await.expect("upgrade migrate");

    // Nothing was dropped, and everything landed in the default org.
    let orgs: Vec<Uuid> =
        hive_core::pgq::query_scalar::<Uuid>("SELECT org_id FROM journal ORDER BY id")
            .fetch_all(&admin)
            .await
            .expect("journal orgs");
    assert_eq!(orgs.len(), 2, "both legacy entries survived");
    assert!(
        orgs.iter().all(|o| *o == db::DEFAULT_ORG_ID),
        "legacy prose belongs to the default org"
    );

    // The existing login became a member of it, so it can still log in.
    let role: Option<String> = hive_core::pgq::query_scalar::<String>(
        "SELECT role FROM memberships WHERE user_id = 'usr_legacy1' AND org_id = ?",
    )
    .bind(db::DEFAULT_ORG_ID)
    .fetch_optional(&admin)
    .await
    .expect("membership");
    assert_eq!(role.as_deref(), Some("admin"), "admins stay admins");

    // The live session kept working rather than being invalidated by upgrade.
    let session_org: Option<Uuid> = hive_core::pgq::query_scalar::<Uuid>(
        "SELECT org_id FROM sessions WHERE id = 'ses_legacy1'",
    )
    .fetch_optional(&admin)
    .await
    .expect("session org");
    assert_eq!(session_org, Some(db::DEFAULT_ORG_ID));

    // And the policy is back on, so the upgraded rows read through it.
    let seen = acting::scope(
        ActingScope::new(db::DEFAULT_ORG_ID, "nate".to_string(), true),
        async {
            hive_core::pgq::query_scalar::<String>("SELECT id FROM journal ORDER BY id")
                .fetch_all(&pool)
                .await
                .expect("scoped read")
        },
    )
    .await;
    assert_eq!(seen, vec!["jrnl_legacy1", "jrnl_legacy2"]);

    // A different org sees none of it — the backfill did not make it global.
    let other = hive_core::store::Store::new(pool.clone())
        .orgs_create("beta", "Beta")
        .await
        .expect("second org");
    let seen = acting::scope(ActingScope::new(other.id, "bob".to_string(), true), async {
        hive_core::pgq::query_scalar::<String>("SELECT id FROM journal")
            .fetch_all(&pool)
            .await
            .expect("scoped read")
    })
    .await;
    assert!(seen.is_empty(), "beta must not inherit the legacy prose");
}

/// Migrating twice must be a no-op, because that is what every restart is.
#[tokio::test]
async fn the_tenancy_migration_is_idempotent() {
    let pool = db::test_pool_strict().await;
    let admin = db::test_admin_pool(&pool).await;
    for _ in 0..3 {
        db::migrate(&admin).await.expect("re-migrate");
    }
    let orgs: i64 = hive_core::pgq::query_scalar::<i64>("SELECT count(*) FROM orgs")
        .fetch_one(&admin)
        .await
        .expect("orgs");
    assert_eq!(orgs, 1, "the default org is created once, not per boot");
    let policies: i64 = hive_core::pgq::query_scalar::<i64>(
        "SELECT count(*) FROM pg_policy p JOIN pg_class c ON c.oid = p.polrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = current_schema()",
    )
    .fetch_one(&admin)
    .await
    .expect("policies");
    // 30 at the orgs workstream, +1 for `artifacts`. The number is not the
    // point ... running the migration three times must not triple it, which is
    // what a CREATE POLICY without a DROP-if-exists would do.
    assert_eq!(policies, 31, "one policy per content table, not three");
}
