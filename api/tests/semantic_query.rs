// Query-path functional tests for the B5 rewrite, hash-provider flavor: the
// brute-force BYTEA path must enforce SQL owner filtering, chunk collapse,
// keyed hydration (drop-on-miss), per-kind rank weights, and the diversified
// stage-1 fill — the same cascade the ANN path runs (vector_perf.rs covers
// that side), so CI keeps exercising all of it without ONNX.

use std::sync::OnceLock;

use hive_api::store::semantic::SemanticOptions;
use hive_api::store::Store;

const QUERY: &str = "alpha hive inspection notes";

/// Latch the deterministic hash provider before any embed call (the provider
/// choice is once-per-process).
fn hash_setup() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| std::env::set_var("HIVE_EMBED", "hash"));
    assert_eq!(hive_embed::embed_dim(), 256, "hash provider must be active");
}

async fn test_store() -> Store {
    hash_setup();
    Store::new(hive_api::db::test_pool().await)
}

/// A deterministic vector whose cosine similarity to `q` is ~`sim`:
/// `sim·q̂ + sqrt(1-sim²)·û` with û a unit vector orthogonal to q.
fn vec_with_sim(q: &[f32], sim: f64, seed: u64) -> Vec<f32> {
    let qn = (q.iter().map(|x| (*x as f64).powi(2)).sum::<f64>()).sqrt();
    let qh: Vec<f64> = q.iter().map(|x| *x as f64 / qn).collect();
    // xorshift noise, then Gram-Schmidt against q̂.
    let mut s = seed.wrapping_mul(0x9e3779b97f4a7c15).max(1);
    let mut r: Vec<f64> = (0..q.len())
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            (s as f64 / u64::MAX as f64) * 2.0 - 1.0
        })
        .collect();
    let dot: f64 = r.iter().zip(&qh).map(|(a, b)| a * b).sum();
    for (ri, qi) in r.iter_mut().zip(&qh) {
        *ri -= dot * qi;
    }
    let rn = (r.iter().map(|x| x * x).sum::<f64>()).sqrt().max(1e-12);
    let ortho = (1.0 - sim * sim).max(0.0).sqrt();
    qh.iter()
        .zip(&r)
        .map(|(qi, ri)| (sim * qi + ortho * ri / rn) as f32)
        .collect()
}

async fn insert_embedding(
    store: &Store,
    kind: &str,
    id: &str,
    chunk_idx: i32,
    owner: Option<&str>,
    vec: &[f32],
) {
    hive_api::pgq::query(
        "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec, hash, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'h', ?)",
    )
    .bind(kind)
    .bind(id)
    .bind(chunk_idx)
    .bind(hive_embed::embed_model())
    .bind(vec.len() as i64)
    .bind(owner)
    .bind(hive_embed::to_blob(vec))
    .bind(hive_api::store::now_iso())
    .execute(store.db())
    .await
    .expect("embedding insert");
}

async fn insert_journal(store: &Store, id: &str, author: &str, scope: Option<&str>, body: &str) {
    hive_api::pgq::query(
        "INSERT INTO journal (id, author, body, user_scope, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(author)
    .bind(body)
    .bind(scope)
    .bind(hive_api::store::now_iso())
    .execute(store.db())
    .await
    .expect("journal insert");
}

async fn insert_mail_account(store: &Store, id: &str, owner: &str) {
    hive_api::pgq::query(
        "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(owner)
    .bind(format!("{owner}@example.test"))
    .bind(hive_api::store::now_iso())
    .bind(hive_api::store::now_iso())
    .execute(store.db())
    .await
    .expect("mail account insert");
}

async fn insert_mail(store: &Store, id: &str, account: &str, owner: &str, deleted: bool) {
    let now = hive_api::store::now_iso();
    hive_api::pgq::query(
        "INSERT INTO mail_messages (id, account_id, jmap_id, jmap_thread_id, received_at, \
           subject, from_addr, snippet, body_text, user_scope, deleted_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(account)
    .bind(format!("j-{id}"))
    .bind(format!("t-{id}"))
    .bind(&now)
    .bind(format!("Subject {id}"))
    .bind("sender@example.test")
    .bind("snippet text")
    .bind(format!("body of {id}"))
    .bind(owner)
    .bind(deleted.then(|| now.clone()))
    .bind(&now)
    .bind(&now)
    .execute(store.db())
    .await
    .expect("mail insert");
}

fn opts(viewer: Option<&str>, limit: usize) -> SemanticOptions {
    SemanticOptions {
        limit: Some(limit),
        // Pure vector ranking: no FTS blend so crafted similarities decide
        // order deterministically (links are empty, so blanket is inert).
        hybrid: Some(false),
        viewer: viewer.map(String::from),
        ..Default::default()
    }
}

#[tokio::test]
async fn brute_owner_filter_and_batched_mail_visibility() {
    let store = test_store().await;
    let q = hive_embed::embed_query(QUERY);

    insert_journal(&store, "jrnl_global", "maggie", None, "global note").await;
    insert_journal(&store, "jrnl_nate", "nate", Some("nate"), "nate private").await;
    insert_mail_account(&store, "acct_nate", "nate").await;
    insert_mail_account(&store, "acct_maggie", "maggie").await;
    insert_mail(&store, "mail_nate", "acct_nate", "nate", false).await;
    insert_mail(&store, "mail_maggie", "acct_maggie", "maggie", false).await;
    insert_mail(&store, "mail_maggie_del", "acct_maggie", "maggie", true).await;

    insert_embedding(
        &store,
        "journal",
        "jrnl_global",
        0,
        None,
        &vec_with_sim(&q, 0.95, 1),
    )
    .await;
    insert_embedding(
        &store,
        "journal",
        "jrnl_nate",
        0,
        Some("nate"),
        &vec_with_sim(&q, 0.96, 2),
    )
    .await;
    insert_embedding(
        &store,
        "mail",
        "mail_nate",
        0,
        Some("nate"),
        &vec_with_sim(&q, 0.94, 3),
    )
    .await;
    insert_embedding(
        &store,
        "mail",
        "mail_maggie",
        0,
        Some("maggie"),
        &vec_with_sim(&q, 0.93, 4),
    )
    .await;
    // Tombstoned mail whose embeddings row lingers (reaper hasn't swept): the
    // SQL owner filter passes it, the batched mail-visibility arm must not.
    insert_embedding(
        &store,
        "mail",
        "mail_maggie_del",
        0,
        Some("maggie"),
        &vec_with_sim(&q, 0.97, 5),
    )
    .await;

    let hits = store
        .semantic_search(QUERY, opts(Some("maggie"), 10))
        .await
        .expect("scoped search");
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert!(
        ids.contains(&"jrnl_global"),
        "global journal visible: {ids:?}"
    );
    assert!(ids.contains(&"mail_maggie"), "own mail visible: {ids:?}");
    assert!(
        !ids.contains(&"jrnl_nate"),
        "foreign journal leaked: {ids:?}"
    );
    assert!(!ids.contains(&"mail_nate"), "foreign mail leaked: {ids:?}");
    assert!(
        !ids.contains(&"mail_maggie_del"),
        "tombstoned mail leaked past the visibility arm: {ids:?}"
    );

    // Unscoped (admin) search skips the owner filter but hydration still
    // refuses deleted mail.
    let hits = store
        .semantic_search(QUERY, opts(None, 10))
        .await
        .expect("admin search");
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert!(ids.contains(&"mail_nate"), "admin sees all owners: {ids:?}");
    assert!(
        ids.contains(&"mail_maggie"),
        "admin sees all owners: {ids:?}"
    );
    assert!(
        !ids.contains(&"mail_maggie_del"),
        "deleted mail must never hydrate: {ids:?}"
    );
    // Mail hydration carries the stored snippet through to the hit.
    let mail_hit = hits.iter().find(|h| h.id == "mail_maggie").unwrap();
    assert_eq!(mail_hit.title, "Subject mail_maggie");
    assert_eq!(mail_hit.snippet, "snippet text");
}

#[tokio::test]
async fn chunk_collapse_scores_parent_by_best_chunk() {
    let store = test_store().await;
    let q = hive_embed::embed_query(QUERY);

    insert_journal(&store, "jrnl_chunky", "nate", None, "long chunked entry").await;
    insert_embedding(
        &store,
        "journal",
        "jrnl_chunky",
        0,
        None,
        &vec_with_sim(&q, 0.60, 10),
    )
    .await;
    insert_embedding(
        &store,
        "journal",
        "jrnl_chunky",
        1,
        None,
        &vec_with_sim(&q, 0.95, 11),
    )
    .await;
    insert_embedding(
        &store,
        "journal",
        "jrnl_chunky",
        2,
        None,
        &vec_with_sim(&q, 0.80, 12),
    )
    .await;

    let hits = store
        .semantic_search(QUERY, opts(None, 10))
        .await
        .expect("search");
    let chunky: Vec<_> = hits.iter().filter(|h| h.id == "jrnl_chunky").collect();
    assert_eq!(chunky.len(), 1, "chunks must collapse to one hit: {hits:?}");
    assert!(
        (chunky[0].score - 0.95).abs() < 0.005,
        "score must be the MAX chunk sim, got {}",
        chunky[0].score
    );
}

#[tokio::test]
async fn hydration_misses_drop_without_starving_results() {
    let store = test_store().await;
    let q = hive_embed::embed_query(QUERY);

    insert_journal(&store, "jrnl_real", "nate", None, "real entry body").await;
    insert_embedding(
        &store,
        "journal",
        "jrnl_real",
        0,
        None,
        &vec_with_sim(&q, 0.90, 20),
    )
    .await;
    // Top-scoring orphan: embeddings row without a journal row (the old code
    // surfaced these with a raw-id title).
    insert_embedding(
        &store,
        "journal",
        "jrnl_orphan",
        0,
        None,
        &vec_with_sim(&q, 0.99, 21),
    )
    .await;

    let hits = store
        .semantic_search(QUERY, opts(None, 1))
        .await
        .expect("search");
    assert_eq!(
        hits.len(),
        1,
        "orphan drop must not starve the slot: {hits:?}"
    );
    assert_eq!(hits[0].id, "jrnl_real");
    assert_eq!(hits[0].title, "nate: real entry body");
    assert!(
        !hits
            .iter()
            .any(|h| h.id == "jrnl_orphan" || h.title == "jrnl_orphan"),
        "orphan surfaced: {hits:?}"
    );
}

#[tokio::test]
async fn kind_weights_demote_mail_and_config_overrides() {
    let store = test_store().await;
    let q = hive_embed::embed_query(QUERY);

    insert_journal(&store, "jrnl_w", "nate", None, "journal about inspections").await;
    insert_mail_account(&store, "acct_w", "nate").await;
    insert_mail(&store, "mail_w", "acct_w", "nate", false).await;
    // Mail similarity strictly above journal's: raw order is mail-first.
    insert_embedding(
        &store,
        "mail",
        "mail_w",
        0,
        Some("nate"),
        &vec_with_sim(&q, 0.95, 30),
    )
    .await;
    insert_embedding(
        &store,
        "journal",
        "jrnl_w",
        0,
        None,
        &vec_with_sim(&q, 0.90, 31),
    )
    .await;

    let hits = store
        .semantic_search(QUERY, opts(None, 2))
        .await
        .expect("search");
    assert_eq!(
        hits[0].id, "jrnl_w",
        "default mail weight 0.85 must demote mail below journal: {hits:?}"
    );
    assert_eq!(hits[1].id, "mail_w");

    // Config override restores neutral weighting → raw similarity order.
    store
        .config_set("search.kind_weights", r#"{"mail": 1.0}"#)
        .await
        .expect("config set");
    let hits = store
        .semantic_search(QUERY, opts(None, 2))
        .await
        .expect("search");
    assert_eq!(
        hits[0].id, "mail_w",
        "weight 1.0 restores sim order: {hits:?}"
    );
    assert!(
        (hits[0].score - 0.95).abs() < 0.005,
        "weight applied once: {hits:?}"
    );
}

#[tokio::test]
async fn diversified_pool_guarantees_journal_under_mail_flood() {
    let store = test_store().await;
    let q = hive_embed::embed_query(QUERY);

    insert_mail_account(&store, "acct_f", "nate").await;
    // 30 mail messages all outscoring the single journal entry — enough to
    // fill stage-1 (limit 5 → pool 10) many times over.
    for i in 0..30 {
        let id = format!("mail_f{i}");
        insert_mail(&store, &id, "acct_f", "nate", false).await;
        let sim = 0.94 + (i as f64) * 0.0005;
        insert_embedding(
            &store,
            "mail",
            &id,
            0,
            Some("nate"),
            &vec_with_sim(&q, sim, 40 + i),
        )
        .await;
    }
    insert_journal(&store, "jrnl_f", "nate", None, "the one journal entry").await;
    insert_embedding(
        &store,
        "journal",
        "jrnl_f",
        0,
        None,
        &vec_with_sim(&q, 0.90, 99),
    )
    .await;

    let hits = store
        .semantic_search(QUERY, opts(None, 5))
        .await
        .expect("search");
    assert!(
        hits.iter().any(|h| h.id == "jrnl_f"),
        "mail flood evicted journal from the pool: {hits:?}"
    );
    // With the default 0.85 mail demotion the journal hit outranks the flood
    // (0.90 vs ≤0.955×0.85).
    assert_eq!(hits[0].id, "jrnl_f", "journal must rank first: {hits:?}");
    assert!(
        hits.iter().any(|h| h.kind == "mail"),
        "mail still present: {hits:?}"
    );
}
