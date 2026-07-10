// ANN query-path tests (plan B5/B8), cutover shape: a fake 384-dim Embedder
// is INJECTED so `dim() == 384` routes semantic_search onto the AnnIndex seam
// (SqliteIndex's in-memory structure) with zero network/model downloads.
//
// Functional coverage: chunk collapse, tombstone hydration drop, and the
// double-probe diversification under a mail flood — the same assertions the
// Postgres HNSW suite pinned. The old #[ignore]d B8 perf gate (200k synthetic
// vectors through Postgres HNSW) died with that extension; the 1.5 ANN is an
// exact
// scan whose perf envelope is documented at core/src/index/ann.rs, and a
// usearch-class HNSW drops in behind the same seam when scale demands it.

mod common;

use std::sync::Arc;

use hive_core::store::semantic::SemanticOptions;
use hive_core::store::Store;

const QUERY: &str = "hive inspection scheduling and honey harvest";

struct FakeBge;

impl hive_embed::Embedder for FakeBge {
    fn model(&self) -> String {
        "fake-bge-384".to_string()
    }
    fn dim(&self) -> usize {
        384
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        fake_vec(text)
    }
    fn embed_query(&self, text: &str) -> Vec<f32> {
        fake_vec(text)
    }
    fn rerank_available(&self) -> bool {
        false
    }
    fn rerank(&self, _query: &str, _docs: &[String]) -> Option<Vec<f64>> {
        None
    }
    fn latched(&self) -> bool {
        false
    }
}

/// Deterministic pseudo-random 384-dim vector seeded by the text (FNV-1a →
/// xorshift). Spread around the sphere so nearest-neighbor structure exists.
fn fake_vec(text: &str) -> Vec<f32> {
    let mut s: u64 = text.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    });
    s = s.max(1);
    (0..384)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            ((s as f64 / u64::MAX as f64) * 2.0 - 1.0) as f32
        })
        .collect()
}

async fn test_store() -> Store {
    common::test_store_with(Arc::new(FakeBge))
}

/// Deterministic vector with cosine ~`sim` to `q` (component along q̂ plus an
/// orthogonalized noise component).
fn vec_with_sim(q: &[f32], sim: f64, seed: u64) -> Vec<f32> {
    let qn = (q.iter().map(|x| (*x as f64).powi(2)).sum::<f64>()).sqrt();
    let qh: Vec<f64> = q.iter().map(|x| *x as f64 / qn).collect();
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

async fn insert_embedding_v(
    store: &Store,
    kind: &str,
    id: &str,
    chunk_idx: i32,
    owner: Option<&str>,
    vec: Vec<f32>,
) {
    store
        .upsert_embedding_raw(kind, id, chunk_idx as i64, "fake-bge-384", owner, vec, "h")
        .await
        .expect("embedding insert");
}

async fn insert_journal(store: &Store, id: &str, author: &str, scope: Option<&str>, body: &str) {
    store
        .raw_sql(
            "INSERT INTO journal (id, author, body, user_scope, created_at) VALUES (?, ?, ?, ?, ?)",
            vec![
                id.into(),
                author.into(),
                body.into(),
                scope.map(str::to_string).into(),
                hive_core::store::now_iso().into(),
            ],
        )
        .await
        .expect("journal insert");
}

async fn insert_mail_account(store: &Store, id: &str, owner: &str) {
    store
        .raw_sql(
            "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)",
            vec![
                id.into(),
                owner.into(),
                format!("{owner}@example.test").into(),
                hive_core::store::now_iso().into(),
                hive_core::store::now_iso().into(),
            ],
        )
        .await
        .expect("mail account insert");
}

async fn insert_mail(store: &Store, id: &str, account: &str, owner: &str, deleted: bool) {
    let now = hive_core::store::now_iso();
    store
        .raw_sql(
            "INSERT INTO mail_messages (id, account_id, jmap_id, jmap_thread_id, received_at, \
               subject, from_addr, snippet, body_text, user_scope, deleted_at, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                id.into(),
                account.into(),
                format!("j-{id}").into(),
                format!("t-{id}").into(),
                now.clone().into(),
                format!("Subject {id}").into(),
                "sender@example.test".into(),
                "snip".into(),
                format!("body of {id}").into(),
                owner.into(),
                deleted.then(|| now.clone()).into(),
                now.clone().into(),
                now.clone().into(),
            ],
        )
        .await
        .expect("mail insert");
}

fn opts(limit: usize) -> SemanticOptions {
    SemanticOptions {
        limit: Some(limit),
        hybrid: Some(false),
        ..Default::default()
    }
}

// ---- small-scale ANN functional coverage ------------------------------------

#[tokio::test]
async fn ann_chunk_collapse_and_tombstone_drop() {
    let store = test_store().await;
    let q = fake_vec(QUERY);

    insert_journal(&store, "jrnl_global", "maggie", None, "global chunked note").await;
    insert_journal(&store, "jrnl_nate", "nate", Some("nate"), "nate private").await;
    insert_mail_account(&store, "acct_n", "nate").await;
    insert_mail_account(&store, "acct_m", "maggie").await;
    insert_mail(&store, "mail_nate", "acct_n", "nate", false).await;
    insert_mail(&store, "mail_maggie", "acct_m", "maggie", false).await;
    insert_mail(&store, "mail_maggie_del", "acct_m", "maggie", true).await;

    // Global journal as three chunks — the parent must surface once at the
    // best chunk's similarity.
    insert_embedding_v(
        &store,
        "journal",
        "jrnl_global",
        0,
        None,
        vec_with_sim(&q, 0.55, 1),
    )
    .await;
    insert_embedding_v(
        &store,
        "journal",
        "jrnl_global",
        1,
        None,
        vec_with_sim(&q, 0.92, 2),
    )
    .await;
    insert_embedding_v(
        &store,
        "journal",
        "jrnl_global",
        2,
        None,
        vec_with_sim(&q, 0.70, 3),
    )
    .await;
    insert_embedding_v(
        &store,
        "journal",
        "jrnl_nate",
        0,
        Some("nate"),
        vec_with_sim(&q, 0.96, 4),
    )
    .await;
    insert_embedding_v(
        &store,
        "mail",
        "mail_nate",
        0,
        Some("nate"),
        vec_with_sim(&q, 0.94, 5),
    )
    .await;
    insert_embedding_v(
        &store,
        "mail",
        "mail_maggie",
        0,
        Some("maggie"),
        vec_with_sim(&q, 0.90, 6),
    )
    .await;
    insert_embedding_v(
        &store,
        "mail",
        "mail_maggie_del",
        0,
        Some("maggie"),
        vec_with_sim(&q, 0.97, 7),
    )
    .await;

    let hits = store
        .semantic_search(QUERY, opts(10))
        .await
        .expect("ANN search");
    let ids: Vec<&str> = hits.iter().map(|h| h.id.as_str()).collect();
    assert!(
        ids.contains(&"jrnl_global"),
        "global journal visible: {ids:?}"
    );
    assert!(
        ids.contains(&"jrnl_nate") && ids.contains(&"mail_nate") && ids.contains(&"mail_maggie"),
        "single-user reads span every stored owner: {ids:?}"
    );
    assert!(
        !ids.contains(&"mail_maggie_del"),
        "tombstoned mail must never hydrate: {ids:?}"
    );

    let chunky: Vec<_> = hits.iter().filter(|h| h.id == "jrnl_global").collect();
    assert_eq!(chunky.len(), 1, "chunks must collapse to one hit: {hits:?}");
    assert!(
        (chunky[0].score - 0.92).abs() < 0.005,
        "parent score must be the MAX chunk sim, got {}",
        chunky[0].score
    );
}

#[tokio::test]
async fn ann_double_probe_rescues_journal_from_mail_flood() {
    let store = test_store().await;
    let q = fake_vec(QUERY);

    // limit 5 → stage1 pool 10 → chunk_k 40. 60 mail chunks all outscoring
    // the journal entry: the unrestricted probe's 40 slots are pure mail, so
    // only the second (bulk-excluding) probe can bring journal back.
    insert_mail_account(&store, "acct_f", "nate").await;
    for i in 0..60 {
        let id = format!("mail_f{i}");
        insert_mail(&store, &id, "acct_f", "nate", false).await;
        let sim = 0.93 + (i as f64) * 0.0004;
        insert_embedding_v(
            &store,
            "mail",
            &id,
            0,
            Some("nate"),
            vec_with_sim(&q, sim, 100 + i),
        )
        .await;
    }
    insert_journal(&store, "jrnl_f", "nate", None, "the one journal entry").await;
    insert_embedding_v(
        &store,
        "journal",
        "jrnl_f",
        0,
        None,
        vec_with_sim(&q, 0.90, 999),
    )
    .await;

    let hits = store
        .semantic_search(QUERY, opts(5))
        .await
        .expect("ANN flood search");
    assert!(
        hits.iter().any(|h| h.id == "jrnl_f"),
        "mail flood evicted journal from the ANN candidates: {hits:?}"
    );
    assert_eq!(
        hits[0].id, "jrnl_f",
        "0.85 mail weight must rank journal first: {hits:?}"
    );
    assert!(
        hits.iter().any(|h| h.kind == "mail"),
        "mail still present: {hits:?}"
    );
}
