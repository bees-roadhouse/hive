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
    let test_db = db::test_pool_strict().await;
    let pool = test_db.pool.clone();
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

    // The foreign key came BACK. `rewind_to_v0_6` drops `{table}_org_fk`, and
    // the probe that decides whether to re-add it used to match on `conname`
    // alone — which is unique per table, not per database, so the first test
    // schema to run took the name and every other schema (this one included)
    // silently skipped its FKs. This test passed for that reason rather than
    // for the right one, because nothing here asked. It asks now, and it asks
    // for `convalidated` too: an FK added NOT VALID and never validated
    // enforces new rows only, which is not what "the FK is back" means.
    for table in ["journal", "people", "wire", "search"] {
        let valid: Option<bool> = hive_core::pgq::query_scalar::<bool>(
            "SELECT convalidated FROM pg_constraint \
             WHERE conname = ? AND conrelid = to_regclass(?) AND contype = 'f'",
        )
        .bind(format!("{table}_org_fk"))
        .bind(table)
        .fetch_optional(&admin)
        .await
        .expect("fk probe");
        assert_eq!(
            valid,
            Some(true),
            "{table}_org_fk must exist and be validated after the upgrade"
        );
    }

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
    let test_db = db::test_pool_strict().await;
    let pool = test_db.pool.clone();
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
    // 30 at the orgs workstream, +1 for `artifacts`, +2 for `blobs` and
    // `runtime_oauth_states`. The number is not the point ... running the
    // migration three times must not triple it, which is what a CREATE POLICY
    // without a DROP-if-exists would do.
    assert_eq!(policies, 33, "one policy per content table, not three");
}

/// Two databases on ONE Postgres cluster — a self-hoster with a staging copy,
/// and the shape that nothing covered.
///
/// The serving role lives in `pg_authid`, which is cluster-wide, while its
/// password was derived from `DATABASE_URL`, which is not. One `hive_app` for
/// every database therefore meant every boot overwrote the other's password:
/// each instance healed, connected, and then failed `28P01` at some later,
/// unpredictable dial. The role is per-database now, so this asserts the thing
/// that was actually broken — both instances still serve after both have
/// booted, in that order.
#[tokio::test]
async fn two_databases_on_one_cluster_both_keep_serving() {
    // The first instance: the usual migrated schema, on DATABASE_URL's database.
    let first = db::test_pool_strict().await;

    // The second: a real second database on the same cluster, booted through
    // the actual boot path rather than a test helper.
    let base = db::database_url();
    let name = format!("hive_two_db_{}", uuid::Uuid::new_v4().simple());
    let owner = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .expect("connect owner");
    sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\""))
        .execute(&owner)
        .await
        .expect("create second database");
    let second_url = base
        .rsplit_once('/')
        .map(|(head, _)| head)
        .expect("url")
        .to_string()
        + "/"
        + &name;

    let second = db::init_with(&second_url).await;

    // Whatever happened, drop the database before asserting, or a failure
    // leaks it. It has to be closed first: DROP DATABASE refuses while a
    // session is attached.
    let outcome = match second {
        Ok(pool) => {
            // Both roles exist, and they are DIFFERENT roles — which is the
            // property that makes the password fight impossible rather than
            // merely unlikely.
            let mine = db::app_role(&base);
            let theirs = db::app_role(&second_url);
            assert_ne!(mine, theirs, "two databases must not share one role");

            // The second instance serves …
            let ok: i32 = hive_core::pgq::query_scalar::<i32>("SELECT 1")
                .fetch_one(&pool)
                .await
                .expect("second instance serves");
            pool.close().await;
            ok
        }
        Err(e) => {
            sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                .execute(&owner)
                .await
                .ok();
            panic!("second instance failed to boot: {e:#}");
        }
    };
    assert_eq!(outcome, 1);
    sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
        .execute(&owner)
        .await
        .expect("drop second database");
    // And its role. Roles are cluster-wide, so DROP DATABASE does not take one
    // with it — without this the test leaks a `hive_app_*` per run, which is
    // also the note for operators decommissioning an instance.
    sqlx::raw_sql(&format!(
        "DROP ROLE IF EXISTS {}",
        db::app_role(&second_url)
    ))
    .execute(&owner)
    .await
    .expect("drop second role");
    owner.close().await;

    // … and the FIRST one still does, which is the half that used to break:
    // the second boot rotated the shared role's password out from under it.
    let ok: i32 = hive_core::pgq::query_scalar::<i32>("SELECT 1")
        .fetch_one(&first.pool)
        .await
        .expect("first instance still serves after the second booted");
    assert_eq!(ok, 1);
}

/// The scoping pass runs under a pinned `t_*,public` search_path on what may
/// be a SHARED database whose public schema holds another install's tables.
/// The unique-rebuild step used to emit a bare `DROP INDEX IF EXISTS
/// projects_name_key`: the name resolved down search_path, landed on the
/// public index, and either errored on the constraint owning it (breaking
/// every migrate on the polluted dev database) or silently ate a bare index
/// belonging to another schema. The drops are schema-qualified now; this
/// pollutes public with the exact shapes and migrates over them.
#[tokio::test]
async fn org_scoping_ddl_cannot_escape_into_public() {
    let test_db = db::test_pool_unmigrated().await;
    let pool = test_db.pool.clone();

    // A constraint-backed projects_name_key in public (the original failure:
    // DROP INDEX cannot drop an index a constraint owns) plus bare indexes
    // named for every other rebuild target (the silent-eat case). Index names
    // are schema-scoped, so one probe table carries all of them.
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS public.zz_scope_escape (id TEXT NOT NULL)")
        .execute(&pool)
        .await
        .expect("probe table");
    sqlx::raw_sql(
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT 1 FROM pg_constraint \
                          WHERE conname = 'projects_name_key' \
                            AND conrelid = 'public.zz_scope_escape'::regclass) THEN \
             ALTER TABLE public.zz_scope_escape ADD CONSTRAINT projects_name_key UNIQUE (id); \
           END IF; \
         END $$",
    )
    .execute(&pool)
    .await
    .expect("constraint-backed probe index");
    let bare = [
        "people_slug_key",
        "topics_slug_key",
        "entity_types_slug_key",
        "identities_platform_platform_id_key",
        "identity_artifacts_actor_kind_name_key",
        "mail_accounts_owner_address_key",
        "shares_uniq",
        "cc_sessions_captured_ext",
    ];
    for idx in &bare {
        sqlx::raw_sql(&format!(
            "CREATE UNIQUE INDEX IF NOT EXISTS {idx} ON public.zz_scope_escape (id)"
        ))
        .execute(&pool)
        .await
        .expect("bare probe index");
    }

    // The whole migrate over the polluted database must succeed — pre-fix it
    // died at `DROP INDEX IF EXISTS projects_name_key`.
    db::migrate(&pool)
        .await
        .expect("migrate over a polluted public schema");

    // Every public probe survived: the drops never left the test schema.
    for idx in bare.iter().chain(["projects_name_key"].iter()) {
        let present: bool = hive_core::pgq::query_scalar::<bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_indexes \
                           WHERE schemaname = 'public' AND indexname = ?)",
        )
        .bind(idx)
        .fetch_one(&pool)
        .await
        .expect("public index probe");
        assert!(
            present,
            "public.{idx} must survive another schema's migrate"
        );
    }

    // And the pass did its real work in the test schema: per-org uniques exist.
    let rebuilt: i64 = hive_core::pgq::query_scalar::<i64>(
        "SELECT count(*) FROM pg_indexes \
         WHERE schemaname = current_schema() AND indexname LIKE '%\\_org\\_uniq'",
    )
    .fetch_one(&pool)
    .await
    .expect("per-org unique probe");
    assert!(
        rebuilt >= 8,
        "per-org unique indexes must still be built in the test schema, got {rebuilt}"
    );
}
