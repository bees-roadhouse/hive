// Fixture-driven end-to-end tests for hive-import (PR 1.7).
//
// The DB-backed tests are DATABASE_URL-gated: without it they skip loudly
// and pass, which is what keeps the main CI job's `cargo test --workspace`
// green with no Postgres (the no-DATABASE_URL invariant). The importer CI
// job provides a pgvector/pg17 service and sets DATABASE_URL; locally, run
// them against the dev instance:
//
//   DATABASE_URL=postgres://hive:hive@localhost:5432/hive \
//     cargo test -p hive-import
//
// Isolation: each test rebirths the legacy schema + seed into a fresh,
// uniquely named Postgres SCHEMA and hands the importer a URL whose
// `options` parameter pins search_path there — full isolation against one
// shared server, the same trick the old core::db::test_pool used.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hive_core::keys::MemoryKeySource;
use hive_core::store::Store;
use hive_embed::HashEmbedder;
use hive_import::{preflight_data_dir, run, Opts, RunOutcome, Summary, IMPORT_DEVICE};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

/// Fixed test master key (both import runs and every verification reopen).
const KEY: [u8; 32] = [7u8; 32];

/// The seed attachment's bytes (matches fixtures/seed.sql's BYTEA literal)
/// and its legacy sha256 key in the old blobs table.
const ATTACHMENT_BYTES: &[u8] = b"hive fixture attachment bytes v1: %PDF-1.4 minimal";
const LEGACY_BLOB_HASH: &str = "b15b6a39a8ca5340b09cdd0af135e7d495b602a220cf1ee1c21593ea8336e577";

/// Skip-or-URL gate for the DB-backed tests.
macro_rules! require_pg {
    () => {
        match fixture_url().await {
            Some(url) => url,
            None => return, // skipped: no DATABASE_URL (main CI job / offline)
        }
    };
}

const ALPHA: [char; 36] = [
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', '0', '1', '2', '3', '4', '5', '6', '7', '8', '9',
];

/// Create a fresh schema on DATABASE_URL, apply the legacy schema + seed,
/// and return a URL pinned to that schema. None (with a loud skip note)
/// when DATABASE_URL is unset.
async fn fixture_url() -> Option<String> {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skipping: DATABASE_URL not set (the importer's DB tests need the fixture Postgres)"
        );
        return None;
    };
    // Serialize setup: concurrent CREATE EXTENSION IF NOT EXISTS races in
    // Postgres (duplicate pg_extension key) when two tests bootstrap at once.
    static SETUP: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _guard = SETUP.lock().await;

    let schema = format!("imp_{}", nanoid::nanoid!(12, &ALPHA));

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .expect("connect DATABASE_URL");
    sqlx::raw_sql(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .expect("create fixture schema");
    admin.close().await;

    // Pin every setup connection to the schema (public stays on the path so
    // the pgvector type resolves), then rebirth the old instance.
    let opts: PgConnectOptions = base.parse().expect("parse DATABASE_URL");
    let opts = opts.options([("search_path", format!("{schema},public"))]);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("connect fixture schema");
    sqlx::raw_sql(include_str!("fixtures/legacy_schema.sql"))
        .execute(&pool)
        .await
        .expect("apply legacy_schema.sql");
    sqlx::raw_sql(include_str!("fixtures/seed.sql"))
        .execute(&pool)
        .await
        .expect("apply seed.sql");
    pool.close().await;

    // The importer gets a plain URL; search_path rides the libpq `options`
    // startup parameter. Verify the pinning actually took before handing it
    // over — a mis-parsed options string would silently read public tables.
    let sep = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{sep}options=-csearch_path%3D{schema},public");
    let probe = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect via options-pinned URL");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM journal")
        .fetch_one(&probe)
        .await
        .expect("probe journal count");
    assert_eq!(n, 5, "options-pinned URL must see the fixture schema");
    probe.close().await;
    Some(url)
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn opts(from: &str, data_dir: &Path) -> Opts {
    Opts {
        from: from.to_string(),
        data_dir: data_dir.to_path_buf(),
        dry_run: false,
        master_key: Some(KEY),
    }
}

fn open_store(dir: &Path) -> Store {
    Store::new(dir, Arc::new(MemoryKeySource(KEY)), Arc::new(HashEmbedder)).expect("reopen store")
}

/// Run a real import and unwrap the Imported arm.
async fn import(opts: &Opts) -> Summary {
    match run(opts).await.expect("import") {
        RunOutcome::Imported(summary) => summary,
        RunOutcome::Plan(_) => panic!("a real run must return RunOutcome::Imported"),
    }
}

async fn one_cell(store: &Store, sql: &str) -> serde_json::Value {
    let rows = store.raw_sql(sql, vec![]).await.expect("raw_sql");
    rows[0][0].clone()
}

// ── no database needed ───────────────────────────────────────────────────────

#[tokio::test]
async fn refuses_a_data_dir_that_already_holds_a_store() {
    // A device file is enough.
    let dir = tmp();
    std::fs::write(dir.path().join("device"), "dev-existing\n").unwrap();
    let err = preflight_data_dir(dir.path()).unwrap_err().to_string();
    assert!(err.contains("one-shot"), "refusal explains one-shot: {err}");
    assert!(
        err.contains("move or remove"),
        "refusal tells the user what to do: {err}"
    );

    // Op-log segments are enough too, even with no device file.
    let dir = tmp();
    let seg = dir.path().join("log").join("dev-old");
    std::fs::create_dir_all(&seg).unwrap();
    std::fs::write(seg.join("0000000000000001.seg"), b"x").unwrap();
    assert!(preflight_data_dir(dir.path()).is_err());

    // The check runs before anything dials Postgres: a garbage URL never
    // gets the chance to fail first.
    let dir = tmp();
    std::fs::write(dir.path().join("device"), "dev-existing\n").unwrap();
    let err = run(&opts("postgres://never-dialed.invalid/nope", dir.path()))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("one-shot"),
        "preflight precedes connect: {err}"
    );

    // An empty (or absent) target passes preflight.
    let dir = tmp();
    preflight_data_dir(dir.path()).expect("empty dir is a valid target");
    preflight_data_dir(&dir.path().join("not-created-yet")).expect("absent dir is valid");
}

// ── fixture-driven, DATABASE_URL-gated ───────────────────────────────────────

#[tokio::test]
async fn dry_run_counts_and_writes_nothing() {
    let url = require_pg!();
    let target = tmp();
    let data_dir: PathBuf = target.path().join("hive");
    let outcome = run(&Opts {
        from: url,
        data_dir: data_dir.clone(),
        dry_run: true,
        master_key: None, // dry runs must not need (or mint) a key
    })
    .await
    .expect("dry run");

    let RunOutcome::Plan(plan) = outcome else {
        panic!("a dry run must return RunOutcome::Plan");
    };
    assert!(!data_dir.exists(), "dry run must not create the data dir");
    assert_eq!(plan.rows("people"), 2);
    assert_eq!(plan.rows("journal"), 5);
    assert_eq!(plan.rows("anchors"), 3);
    assert_eq!(plan.rows("entity_types"), 3);
    assert_eq!(plan.rows("links"), 6);
    assert_eq!(plan.rows("mail_messages"), 2);
    assert_eq!(plan.rows("mail_attachments"), 1);

    // The rollup the onboarding plan card renders: grouped() partitions the
    // tables (sum == total) and surfaces the fixture's headline numbers.
    let grouped: std::collections::HashMap<&str, i64> = plan.grouped().into_iter().collect();
    assert_eq!(grouped["journal entries"], 5);
    assert_eq!(grouped["links"], 6);
    assert_eq!(grouped["mail messages"], 2);
    assert_eq!(grouped["attachments"], 1);
    assert!(grouped["entities"] >= 2 + 3 + 3, "entity tables roll up");
    let grouped_sum: i64 = plan.grouped().iter().map(|(_, n)| n).sum();
    assert_eq!(grouped_sum, plan.total_rows());
}

#[tokio::test]
async fn import_maps_the_hosted_instance_onto_the_store() {
    let url = require_pg!();
    let target = tmp();
    let summary = import(&opts(&url, target.path())).await;

    // Record accounting, exactly: 2 people + 1 profile + 1 identity +
    // 1 artifact + 1 source + 2 config (app.version excluded) + 3 types +
    // 3 fields + 1 project + 1 topic + 1 phase + 2 tasks + 2 decisions +
    // 1 event + 1 custom instance + 5 journal + 6 links + 1 unread inbox +
    // 1 account + 1 cursor + 1 mailbox + 2 messages + 1 attachment doc +
    // 1 blob-store doc + 1 alias = 43.
    assert_eq!(summary.records, 43);
    assert_eq!(summary.blobs_stored, 1);
    assert_eq!(summary.blobs_rekeyed, 1);
    assert_eq!(summary.attachments_pending, 0);
    assert_eq!(summary.inbox_read_skipped, 1);
    assert_eq!(summary.mail_deleted_skipped, 0);
    assert_eq!(summary.mail_fts_rows, 1, "junk mail gets no FTS row");

    let store = open_store(target.path());
    assert_eq!(store.device(), IMPORT_DEVICE);

    // Journal: count, order (created_at DESC), ids + timestamps EXACT.
    let entries = store.journal_list(50, 0).await.expect("journal_list");
    let got: Vec<(&str, &str)> = entries
        .iter()
        .map(|v| (v.entry.id.as_str(), v.entry.created_at.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("jrnl_fixentry05", "2025-11-07T21:30:00.000Z"),
            ("jrnl_fixentry04", "2025-11-06T09:00:00.000Z"),
            ("jrnl_fixentry03", "2025-11-05T15:00:00.000Z"),
            ("jrnl_fixentry02", "2025-11-04T12:00:00.000Z"),
            ("jrnl_fixentry01", "2025-11-02T10:05:00.000Z"),
        ]
    );
    let first = entries.last().unwrap();
    assert_eq!(first.entry.author, "nate");
    assert_eq!(first.entry.tags, vec!["baking"]);
    assert_eq!(first.entry.mentions, vec!["pia"]);
    assert_eq!(entries[0].entry.user_scope.as_deref(), Some("nate"));

    // Anchors survive inline and resolve to their entities by original id.
    let view = store
        .journal_get("jrnl_fixentry01")
        .await
        .unwrap()
        .expect("entry 01");
    assert_eq!(view.anchors.len(), 1);
    let anchor = &view.anchors[0];
    assert_eq!(anchor.anchor.id, "anc_fixlevain01");
    assert_eq!(anchor.anchor.text, "Refresh the levain twice daily");
    assert_eq!(anchor.entity["id"], "task_fixlevain1");

    // Entities keep ids, state, and both original timestamps.
    let task = store
        .tasks_get("task_fixlevain1")
        .await
        .unwrap()
        .expect("task");
    assert_eq!(task.title, "Refresh the levain schedule");
    assert_eq!(task.status.as_str(), "doing");
    assert_eq!(task.created_at, "2025-11-02T10:05:00.000Z");
    assert_eq!(task.updated_at, "2025-11-06T18:00:00.000Z");
    assert_eq!(task.project.as_deref(), Some("proj_fixhomest1"));
    assert_eq!(task.assignees, vec!["nate", "pia"]);

    let new_dec = store
        .decisions_get("dec_fixoven0002")
        .await
        .unwrap()
        .expect("decision");
    assert_eq!(new_dec.supersedes.as_deref(), Some("dec_fixoven0001"));
    assert_eq!(new_dec.status.as_str(), "accepted");
    let old_dec = store
        .decisions_get("dec_fixoven0001")
        .await
        .unwrap()
        .expect("superseded decision");
    assert_eq!(old_dec.status.as_str(), "superseded");

    // Links resolve, including decision-supersedes, under original link ids.
    let links = store
        .links_for_entity("dec_fixoven0002")
        .await
        .expect("links");
    assert!(
        links.iter().any(|l| l.id == "link_fixsuper01"
            && l.rel == "supersedes"
            && l.target_id == "dec_fixoven0001"),
        "supersedes edge resolves: {links:?}"
    );

    // Custom entity instance + its type registry.
    let recipe = store
        .custom_entities_get("ent_fixrecipe01")
        .await
        .unwrap()
        .expect("recipe instance");
    assert_eq!(recipe.title, "Overnight country loaf");
    assert_eq!(recipe.fields["stage"], "keeper");
    let ty = store
        .entity_types_get("recipe")
        .await
        .unwrap()
        .expect("recipe type");
    let slugs: Vec<&str> = ty.fields.iter().map(|f| f.slug.as_str()).collect();
    assert_eq!(slugs, vec!["notes", "stage"]);

    // Actor tier: people, profile card, artifact, platform identity, source.
    assert_eq!(store.people_list().await.unwrap().len(), 2);
    let profile = store.profile_get("nate").await.unwrap().expect("profile");
    assert_eq!(profile.display_name, "Nate");
    assert_eq!(store.artifacts_list("pia").await.unwrap().len(), 1);
    let ids = store.identities_list().await.unwrap();
    assert_eq!(ids.len(), 1);
    assert_eq!(ids[0].platform_id, "110000000000000001");
    assert_eq!(store.sources_list(None).await.unwrap().len(), 1);

    // Config crossed minus the retired instance's app.version.
    assert_eq!(
        store.config_get("instance.name").await.unwrap().as_deref(),
        Some("Bierly-Smith Hive")
    );
    assert_eq!(store.config_get("app.version").await.unwrap(), None);

    // Inbox: the unread row only.
    let pia_inbox = store.inbox_list("pia", false).await.unwrap();
    assert_eq!(pia_inbox.len(), 1);
    assert_eq!(pia_inbox[0].id, "inb_fixunread01");
    assert!(store.inbox_list("nate", false).await.unwrap().is_empty());

    // Mail account: identity intact, credential GONE, cursor exact.
    let accounts = store.mail_accounts_due().await.unwrap();
    assert_eq!(accounts.len(), 1);
    let acct = &accounts[0];
    assert_eq!(acct.id, "macct_fixnate01");
    assert_eq!(acct.cred_id, None, "credentials must not migrate");
    assert_eq!(acct.jmap_account_id, "acc-jmap-01");
    let (email_state, mailbox_state, backfill_status, backfill_cursor) =
        store.mail_cursor_load("macct_fixnate01").await.unwrap();
    assert_eq!(email_state.as_deref(), Some("es-000042"));
    assert_eq!(mailbox_state.as_deref(), Some("ms-000017"));
    assert_eq!(backfill_status, "complete");
    assert_eq!(
        backfill_cursor.expect("backfill cursor")["upTo"],
        "2025-10-01T00:00:00Z"
    );
    let boxes = store.mail_mailboxes_list("macct_fixnate01").await.unwrap();
    assert_eq!(boxes.len(), 1);
    assert!(boxes[0].ingest, "operator intent (ingest) carries over");

    // Attachment bytes round-trip from the blockstore under the blake3 key;
    // the legacy sha256 key is aliased to it.
    let served = store
        .mail_attachment_serve("matt_fixpdf0001")
        .await
        .unwrap()
        .expect("attachment");
    assert_eq!(served.data.as_deref(), Some(ATTACHMENT_BYTES));
    let blake = blake3::hash(ATTACHMENT_BYTES).to_hex().to_string();
    assert_eq!(served.blob_hash.as_deref(), Some(blake.as_str()));
    let alias = store
        .raw_sql("SELECT namespace, \"from\", \"to\" FROM aliases", vec![])
        .await
        .unwrap();
    assert_eq!(alias.len(), 1);
    assert_eq!(alias[0][0], "blob");
    assert_eq!(alias[0][1], LEGACY_BLOB_HASH);
    assert_eq!(alias[0][2], blake.as_str());

    // FTS: an old journal entry is findable by its words; the clean mail
    // message too; the junk one is not.
    let hits = store.search("levain", 10).await.unwrap();
    assert!(
        hits.iter()
            .any(|h| h.kind == "journal" && h.id == "jrnl_fixentry01"),
        "journal FTS hit: {hits:?}"
    );
    let hits = store.search("millwheel", 10).await.unwrap();
    assert!(
        hits.iter()
            .any(|h| h.kind == "mail" && h.id == "mail_fixmsg0001"),
        "mail FTS hit: {hits:?}"
    );
    let hits = store.search("prize", 10).await.unwrap();
    assert!(
        !hits.iter().any(|h| h.kind == "mail"),
        "junk mail stays unsearchable: {hits:?}"
    );

    // Nothing embedded during import; eligibility is stamped for the
    // app's background backfill (pending) and junk is parked (skip).
    assert_eq!(
        one_cell(&store, "SELECT count(*) FROM embeddings").await,
        serde_json::json!(0)
    );
    let states = store
        .raw_sql(
            "SELECT id, embed_state FROM mail_messages ORDER BY id",
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(states[0][1], "pending");
    assert_eq!(states[1][1], "skip");

    store.shutdown().await.expect("orderly close");
}

/// The onboarding contract, pinned exactly: run() returns with its store
/// fully shut down (flock released), so the SAME process can immediately
/// Store::new the imported dir and read it — no relaunch between the
/// [Import] click and the journal.
#[tokio::test]
async fn the_same_process_reopens_the_store_right_after_import() {
    let url = require_pg!();
    let target = tmp();
    let summary = import(&opts(&url, target.path())).await;
    assert!(summary.records > 0);

    // Would fail with "another hive process has this data dir open" if
    // run() had leaked its flock.
    let store = open_store(target.path());
    assert_eq!(store.device(), IMPORT_DEVICE);
    let entries = store.journal_list(100, 0).await.expect("journal_list");
    assert_eq!(entries.len(), 5, "the imported journal reads back");
    store.shutdown().await.expect("orderly close");
}

/// A reachable Postgres that is NOT a hosted hive fails the source
/// preflight with a plain answer (the onboarding shows it verbatim), not a
/// bare SQL error from the first count.
#[tokio::test]
async fn refuses_a_database_that_is_not_a_hive() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skipping: DATABASE_URL not set (the importer's DB tests need the fixture Postgres)"
        );
        return;
    };
    // A fresh empty schema, pinned via search_path: guaranteed hive-free.
    let schema = format!("imp_{}", nanoid::nanoid!(12, &ALPHA));
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .expect("connect DATABASE_URL");
    sqlx::raw_sql(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .expect("create empty schema");
    admin.close().await;
    let sep = if base.contains('?') { '&' } else { '?' };
    let url = format!("{base}{sep}options=-csearch_path%3D{schema}");

    let target = tmp();
    let err = run(&Opts {
        from: url,
        data_dir: target.path().join("hive"),
        dry_run: true,
        master_key: None,
    })
    .await
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("doesn't look like a hosted hive database"),
        "plain-answer preflight: {err}"
    );
    assert!(
        err.contains("missing table"),
        "names a missing table: {err}"
    );
}

#[tokio::test]
async fn importing_twice_folds_to_identical_state() {
    let url = require_pg!();
    let (a, b) = (tmp(), tmp());
    import(&opts(&url, a.path())).await;
    import(&opts(&url, b.path())).await;

    let store_a = open_store(a.path());
    let dump_a = store_a.canonical_dump().await.expect("dump a");
    store_a.shutdown().await.unwrap();
    let store_b = open_store(b.path());
    let dump_b = store_b.canonical_dump().await.expect("dump b");
    store_b.shutdown().await.unwrap();

    assert!(!dump_a.is_empty());
    assert!(dump_a.contains("journal|"), "dump covers the journal");
    assert_eq!(
        dump_a, dump_b,
        "two imports of one source are byte-identical"
    );
}
