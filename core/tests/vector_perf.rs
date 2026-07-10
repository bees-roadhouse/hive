// ANN query-path tests (plan B5/B8). A fake 384-dim ONNX engine stands in for
// BGE so `embed_dim() == 384` routes semantic_search onto the HNSW ANN path
// with zero network/model downloads — every test in this binary shares that
// provider (the choice latches once per process).
//
// Non-ignored tests: small-scale ANN functional coverage (chunk collapse,
// tombstone hydration drop, double-probe diversification — the owner-filter
// halves died with the viewer ACL in PR 1.3). The #[ignore]d test is the B8
// perf gate at 200k vectors:
//
//   cargo test -p hive-core --test vector_perf -- --ignored --nocapture

mod common;

use std::sync::OnceLock;
use std::time::Instant;

use hive_core::store::semantic::SemanticOptions;
use hive_core::store::Store;
use sqlx::PgPool;

const QUERY: &str = "hive inspection scheduling and honey harvest";

struct FakeBge;

impl hive_embed::OnnxProvider for FakeBge {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(fake_vec(text))
    }
    fn rerank(&self, _query: &str, _docs: &[String]) -> anyhow::Result<Vec<f64>> {
        anyhow::bail!("fake engine has no reranker")
    }
    fn supports_rerank(&self) -> bool {
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

/// Force the transformers provider + fake engine before anything embeds.
fn ann_setup() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        std::env::set_var("HIVE_EMBED", "transformers");
        hive_embed::set_onnx_provider(Box::new(FakeBge));
    });
    assert_eq!(
        hive_embed::embed_dim(),
        384,
        "ANN tests need the 384-dim provider (another provider latched first?)"
    );
}

async fn test_store() -> Store {
    ann_setup();
    common::test_store().await
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
    sqlx::query(
        "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec_v, hash, created_at) \
         VALUES ($1, $2, $3, $4, 384, $5, $6, 'h', $7)",
    )
    .bind(kind)
    .bind(id)
    .bind(chunk_idx)
    .bind(hive_embed::embed_model())
    .bind(owner)
    .bind(pgvector::Vector::from(vec))
    .bind(hive_core::store::now_iso())
    .execute(store.db())
    .await
    .expect("vec_v embedding insert");
}

async fn insert_journal(store: &Store, id: &str, author: &str, scope: Option<&str>, body: &str) {
    hive_core::pgq::query(
        "INSERT INTO journal (id, author, body, user_scope, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(author)
    .bind(body)
    .bind(scope)
    .bind(hive_core::store::now_iso())
    .execute(store.db())
    .await
    .expect("journal insert");
}

async fn insert_mail_account(store: &Store, id: &str, owner: &str) {
    hive_core::pgq::query(
        "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(owner)
    .bind(format!("{owner}@example.test"))
    .bind(hive_core::store::now_iso())
    .bind(hive_core::store::now_iso())
    .execute(store.db())
    .await
    .expect("mail account insert");
}

async fn insert_mail(store: &Store, id: &str, account: &str, owner: &str, deleted: bool) {
    let now = hive_core::store::now_iso();
    hive_core::pgq::query(
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
    .bind("snip")
    .bind(format!("body of {id}"))
    .bind(owner)
    .bind(deleted.then(|| now.clone()))
    .bind(&now)
    .bind(&now)
    .execute(store.db())
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

// ---- non-ignored: small-scale ANN functional coverage ------------------------

#[tokio::test]
async fn ann_chunk_collapse_and_tombstone_drop() {
    let store = test_store().await;
    let q = hive_embed::embed_query(QUERY);

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
    let q = hive_embed::embed_query(QUERY);

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

// ---- ignored: the B8 perf gate at 200k --------------------------------------

async fn drop_hnsw(pool: &PgPool) {
    sqlx::query("DROP INDEX IF EXISTS embeddings_vec_hnsw")
        .execute(pool)
        .await
        .expect("drop hnsw");
}

async fn create_hnsw(pool: &PgPool) {
    let mut conn = pool.acquire().await.expect("conn");
    // Serial build: parallel workers allocate the build memory as POSIX shm,
    // which blows the 64MB /dev/shm default of docker/podman containers.
    // Serial keeps the graph in backend-local memory — slower but runs
    // anywhere. 1GB comfortably fits 211k × 384-dim.
    sqlx::query("SET maintenance_work_mem = '1GB'")
        .execute(&mut *conn)
        .await
        .expect("work mem");
    sqlx::query("SET max_parallel_maintenance_workers = 0")
        .execute(&mut *conn)
        .await
        .expect("serial build");
    sqlx::query(
        "CREATE INDEX embeddings_vec_hnsw ON embeddings \
         USING hnsw (vec_v public.vector_cosine_ops) WITH (m = 16, ef_construction = 64)",
    )
    .execute(&mut *conn)
    .await
    .expect("create hnsw");
    sqlx::query("ANALYZE embeddings")
        .execute(&mut *conn)
        .await
        .expect("analyze");
}

/// Cluster centroids for the synthetic corpus. Uniform random 384-dim vectors
/// are a known HNSW pathology — every neighbor is a near-tie and the graph is
/// unnavigable, so recall collapses in a way that says nothing about real
/// embeddings. Real BGE vectors live on a clustered manifold; 1000 centroids
/// + per-row noise reproduces that structure.
async fn create_centroids(pool: &PgPool) {
    sqlx::query(
        "CREATE TABLE perf_centroids (id INT PRIMARY KEY, arr DOUBLE PRECISION[] NOT NULL)",
    )
    .execute(pool)
    .await
    .expect("centroid table");
    // `(cid - cid)` correlates the inner series so it re-evaluates per row.
    sqlx::query(
        "INSERT INTO perf_centroids \
         SELECT cid, (SELECT array_agg(random() - 0.5) FROM generate_series(1, 384 + (cid - cid))) \
         FROM generate_series(0, 999) AS cid",
    )
    .execute(pool)
    .await
    .expect("centroids");
}

/// One synthetic vector per row: its `g % 1000` centroid plus elementwise
/// noise (ORDER BY ordinality keeps components aligned with the centroid).
/// Noise amplitude matches the centroid amplitude (soft, overlapping
/// clusters — within-cluster cosine ≈ 0.5, cross-cluster ≈ 0): tighter
/// clusters fragment the HNSW graph into unreachable islands and recall for
/// unlucky probes collapses to 0, which real embedding manifolds don't do.
const CLUSTERED_VEC: &str = "(SELECT array_agg(x + (random() - 0.5) ORDER BY ord) \
     FROM unnest(c.arr) WITH ORDINALITY AS u(x, ord))::public.vector";

async fn bulk_mail(pool: &PgPool, owner: &str, account: &str, n: i64) {
    let now = hive_core::store::now_iso();
    sqlx::query(
        "INSERT INTO mail_messages (id, account_id, jmap_id, jmap_thread_id, received_at, \
           subject, from_addr, snippet, body_text, user_scope, created_at, updated_at) \
         SELECT 'perf_' || $1::text || '_' || g, $2, 'j' || g, 't' || g, $3, \
                'perf subject ' || g, 'perf@example.test', 'snip', 'perf body ' || g, $1, $3, $3 \
         FROM generate_series(1, $4) AS g",
    )
    .bind(owner)
    .bind(account)
    .bind(&now)
    .bind(n)
    .execute(pool)
    .await
    .expect("bulk mail rows");
    sqlx::query(&format!(
        "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec_v, hash, created_at) \
         SELECT 'mail', 'perf_' || $1::text || '_' || g, 0, $2, 384, $1, {CLUSTERED_VEC}, 'h', $3 \
         FROM generate_series(1, $4) AS g \
         JOIN perf_centroids c ON c.id = (g % 1000)",
    ))
    .bind(owner)
    .bind(hive_embed::embed_model())
    .bind(&now)
    .bind(n)
    .execute(pool)
    .await
    .expect("bulk mail vectors");
}

async fn bulk_journal(pool: &PgPool, author: &str, prefix: &str, n: i64) {
    let now = hive_core::store::now_iso();
    sqlx::query(
        "INSERT INTO journal (id, author, body, created_at) \
         SELECT $1::text || g, $2, 'perf journal body ' || g, $3 FROM generate_series(1, $4) AS g",
    )
    .bind(prefix)
    .bind(author)
    .bind(&now)
    .bind(n)
    .execute(pool)
    .await
    .expect("bulk journal rows");
    sqlx::query(&format!(
        "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec_v, hash, created_at) \
         SELECT 'journal', $1::text || g, 0, $2, 384, NULL, {CLUSTERED_VEC}, 'h', $3 \
         FROM generate_series(1, $4) AS g \
         JOIN perf_centroids c ON c.id = (g % 1000)",
    ))
    .bind(prefix)
    .bind(hive_embed::embed_model())
    .bind(&now)
    .bind(n)
    .execute(pool)
    .await
    .expect("bulk journal vectors");
}

fn p95(samples: &mut [f64]) -> f64 {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((samples.len() as f64) * 0.95).ceil() as usize - 1;
    samples[idx.min(samples.len() - 1)]
}

/// 50 warm hybrid searches (rerank off); returns per-search ms and the last
/// result set.
async fn timed_searches(store: &Store, n: usize) -> (Vec<f64>, Vec<hive_shared::SearchHit>) {
    let mut times = Vec::with_capacity(n);
    let mut last = Vec::new();
    for i in 0..n {
        let query = format!("perf query {i} hive honey harvest planning");
        let t = Instant::now();
        last = store
            .semantic_search(
                &query,
                SemanticOptions {
                    limit: Some(10),
                    rerank: Some(false),
                    ..Default::default()
                },
            )
            .await
            .expect("perf search");
        times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    (times, last)
}

async fn top10(pool: &PgPool, qv: &pgvector::Vector, exact: bool) -> Vec<String> {
    let mut tx = pool.begin().await.expect("tx");
    if exact {
        // pgvector's documented exact-search mode: block the index scan so
        // ORDER BY runs a full heap sort.
        sqlx::query("SET LOCAL enable_indexscan = off")
            .execute(&mut *tx)
            .await
            .expect("exact mode");
    } else {
        sqlx::query("SET LOCAL hnsw.ef_search = 80")
            .execute(&mut *tx)
            .await
            .expect("ef");
    }
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT ref_kind || ':' || ref_id FROM embeddings \
         WHERE vec_v IS NOT NULL ORDER BY vec_v <=> $1 LIMIT 10",
    )
    .bind(qv)
    .fetch_all(&mut *tx)
    .await
    .expect("top10");
    rows.into_iter().map(|(k,)| k).collect()
}

#[tokio::test]
#[ignore = "B8 perf gate: 200k synthetic vectors, minutes of setup — run explicitly"]
async fn ann_perf_200k_hybrid_p95_and_recall() {
    ann_setup();

    // Full corpus: 200k nate mail + 10k maggie mail + 1k journal.
    let store = common::test_store().await;
    let pool = store.db().clone();
    println!("[perf] building full schema (200k + 10k mail + 1k journal)…");
    insert_mail_account(&store, "acct_nate", "nate").await;
    insert_mail_account(&store, "acct_maggie", "maggie").await;
    drop_hnsw(&pool).await;
    create_centroids(&pool).await;
    let t = Instant::now();
    bulk_mail(&pool, "nate", "acct_nate", 200_000).await;
    bulk_mail(&pool, "maggie", "acct_maggie", 10_000).await;
    bulk_journal(&pool, "nate", "perfj_n_", 500).await;
    bulk_journal(&pool, "maggie", "perfj_m_", 500).await;
    println!("[perf] bulk insert: {:.1}s", t.elapsed().as_secs_f64());
    let t = Instant::now();
    create_hnsw(&pool).await;
    println!(
        "[perf] full HNSW build (211k vectors): {:.1}s",
        t.elapsed().as_secs_f64()
    );

    // 1. The candidate probe must use the HNSW index.
    let qv = pgvector::Vector::from(fake_vec("explain probe"));
    let mut tx = pool.begin().await.expect("tx");
    sqlx::query("SET LOCAL hnsw.ef_search = 80")
        .execute(&mut *tx)
        .await
        .expect("ef");
    let plan: serde_json::Value = sqlx::query_scalar(
        "EXPLAIN (FORMAT JSON) \
         SELECT ref_kind, ref_id, 1 - (vec_v <=> $1) AS sim FROM embeddings \
         WHERE model = $2 AND vec_v IS NOT NULL \
         ORDER BY vec_v <=> $1 LIMIT 80",
    )
    .bind(&qv)
    .bind(hive_embed::embed_model())
    .fetch_one(&mut *tx)
    .await
    .expect("explain");
    drop(tx);
    assert!(
        plan.to_string().contains("embeddings_vec_hnsw"),
        "ANN probe must use the HNSW index, plan: {plan}"
    );
    println!("[perf] EXPLAIN uses embeddings_vec_hnsw: ok");

    // 2. p95 < 100ms over 50 warm hybrid searches (rerank off).
    let (_, _) = timed_searches(&store, 10).await; // warm
    let (mut times, hits) = timed_searches(&store, 50).await;
    let p95_all = p95(&mut times);
    println!(
        "[perf] p95 over 50 warm hybrid searches: {p95_all:.1}ms (min {:.1} max {:.1})",
        times.first().unwrap(),
        times.last().unwrap()
    );
    assert!(!hits.is_empty(), "searches must return hits");
    assert!(p95_all < 100.0, "p95 {p95_all:.1}ms ≥ 100ms budget");

    // 3. ANN recall: top-10 overlap vs exact brute force ≥ 9/10 at ef=80.
    // Probes are stored vectors — on-manifold queries, like a real BGE query
    // vector landing near the corpus it was trained with.
    let probe_ids = [
        "perf_nate_11",
        "perf_nate_50001",
        "perf_maggie_7",
        "perf_nate_123456",
        "perf_maggie_9999",
    ];
    let mut overlaps = Vec::new();
    for id in probe_ids {
        let qv: pgvector::Vector = sqlx::query_scalar(
            "SELECT vec_v FROM embeddings WHERE ref_kind = 'mail' AND ref_id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .expect("probe vector");
        let ann = top10(&pool, &qv, false).await;
        let exact = top10(&pool, &qv, true).await;
        let overlap = ann.iter().filter(|k| exact.contains(k)).count();
        overlaps.push(overlap);
    }
    println!("[perf] ANN vs brute top-10 overlap per query: {overlaps:?}");
    for (i, o) in overlaps.iter().enumerate() {
        assert!(*o >= 9, "query {i}: ANN top-10 overlap {o}/10 < 9/10");
    }
}
