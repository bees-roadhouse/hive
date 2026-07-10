// SqliteIndex integration tests (PR 1.5): FTS5 behavior against real data
// (hits, snippets, bm25 ordering, kind filters, hostile queries) and the
// embeddings + ANN plumbing (self-retrieval, persistence across reopen,
// removal). Hermetic: tempdir + MemoryKeySource; no Postgres.
//
// The pinned fts5_query adversarial matrix lives with the builder in
// core/src/index/fts.rs; this file proves the composed path — raw user
// input through keyword_search — never reaches FTS5 as syntax.

use ciborium::Value as Cb;
use hive_core::index::SqliteIndex;
use hive_core::keys::MemoryKeySource;
use hive_core::oplog::{kind, Record};

fn keysource() -> MemoryKeySource {
    MemoryKeySource([7u8; 32])
}

fn t(s: &str) -> Cb {
    Cb::Text(s.to_string())
}

fn map(entries: Vec<(&str, Cb)>) -> Cb {
    Cb::Map(entries.into_iter().map(|(k, v)| (t(k), v)).collect())
}

fn arr(vals: Vec<Cb>) -> Cb {
    Cb::Array(vals)
}

fn ts(i: usize) -> String {
    format!("2026-07-10T13:{:02}:{:02}.000Z", i / 60, i % 60)
}

fn journal_rec(seq: u64, id: &str, body: &str) -> Record {
    Record::new(
        "dev-1",
        seq,
        seq,
        &ts(seq as usize),
        "nate",
        kind::JOURNAL_APPEND,
        map(vec![
            ("id", t(id)),
            ("author", t("nate")),
            ("body", t(body)),
            ("tags", arr(vec![])),
            ("mentions", arr(vec![])),
            ("created_at", t(&ts(seq as usize))),
        ]),
    )
}

fn task_rec(seq: u64, id: &str, title: &str, body: &str) -> Record {
    Record::new(
        "dev-1",
        seq,
        seq,
        &ts(seq as usize),
        "nate",
        kind::ENTITY_CREATE,
        map(vec![
            ("kind", t("task")),
            ("id", t(id)),
            (
                "fields",
                map(vec![
                    ("title", t(title)),
                    ("body", t(body)),
                    ("created_at", t(&ts(seq as usize))),
                    ("updated_at", t(&ts(seq as usize))),
                ]),
            ),
        ]),
    )
}

/// An index with a small folded corpus: three journal entries + one task.
fn corpus() -> (tempfile::TempDir, SqliteIndex) {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = SqliteIndex::open(dir.path(), &keysource()).unwrap();
    idx.fold(&[
        journal_rec(1, "j1", "the rust rewrite ships this week"),
        journal_rec(2, "j2", "groceries: honey, oats, tea"),
        journal_rec(
            3,
            "j3",
            "rust segments rotate at eight mebibytes; the rust rewrite is close",
        ),
        task_rec(4, "t1", "Finish the rust fold", "rewrite the projector"),
    ])
    .unwrap();
    (dir, idx)
}

// ── 4. FTS5: hits, snippets, bm25 ordering, kind filters, hostility ─────────

#[test]
fn keyword_search_hits_and_bm25_ordering() {
    let (_dir, idx) = corpus();
    let hits = idx.keyword_search("rust rewrite", None, 10).unwrap();
    assert!(hits.len() >= 3, "hits: {hits:?}");
    // j3 mentions rust twice + rewrite once; it must outrank j2-free rows…
    // and every hit must actually contain both terms (implicit AND).
    assert!(hits.iter().all(|h| h.id != "j2"));
    // Scores are the normalized DESC shape: monotone with rank order.
    for pair in hits.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "scores must be descending: {hits:?}"
        );
    }
    // Bounded 0..1 like the Postgres clamp; a common term in a tiny corpus
    // can legitimately round to 0.000 (FTS5 floors the IDF near zero).
    assert!(hits.iter().all(|h| (0.0..=1.0).contains(&h.score)));
}

#[test]
fn snippet_wraps_matches_in_brackets() {
    let (_dir, idx) = corpus();
    let hits = idx.keyword_search("mebibytes", None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "j3");
    assert!(
        hits[0].snippet.contains("[mebibytes]"),
        "snippet: {:?}",
        hits[0].snippet
    );
}

#[test]
fn prefix_matching_per_token() {
    let (_dir, idx) = corpus();
    // Every token is a prefix query ("rot"* matches rotate).
    let hits = idx.keyword_search("seg rot", None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "j3");
}

#[test]
fn kinds_filter_restricts_without_boosting() {
    let (_dir, idx) = corpus();
    let all = idx.keyword_search("rust", None, 10).unwrap();
    assert!(all.iter().any(|h| h.kind == "task"));
    assert!(all.iter().any(|h| h.kind == "journal"));
    let only_tasks = idx.keyword_search("rust", Some(&["task"]), 10).unwrap();
    assert_eq!(only_tasks.len(), 1);
    assert_eq!(only_tasks[0].id, "t1");
    // Multi-kind filters bind after the MATCH parameter and before LIMIT.
    let both = idx
        .keyword_search("rust", Some(&["task", "journal"]), 10)
        .unwrap();
    assert_eq!(both.len(), all.len());
    let none = idx.keyword_search("rust", Some(&[]), 10).unwrap();
    assert!(none.is_empty());
}

#[test]
fn hostile_inputs_never_reach_fts5_as_syntax() {
    let (_dir, idx) = corpus();
    // Every one of these would be an FTS5 syntax error or an operator if
    // passed raw; through fts5_query they are plain (possibly empty) queries.
    for hostile in [
        "\"unbalanced",
        "rust AND",
        "AND rust",
        "NOT rust",
        "(rust OR",
        "rust NEAR/3 rewrite",
        "-rust",
        "rust*ships",
        "col:value",
        "^rust",
        "*",
        "\"\"",
        "()",
        "!!!",
        "",
        "   ",
        "🐝 蜂蜜",
    ] {
        let res = idx.keyword_search(hostile, None, 10);
        assert!(res.is_ok(), "query {hostile:?} errored: {res:?}");
    }
    // Neutralized operators still search as words: "rust AND" finds rust
    // rows because "and"* is just a term that happens to miss.
    let hits = idx.keyword_search("rust -week", None, 10).unwrap();
    assert!(hits.iter().any(|h| h.id == "j1"), "hits: {hits:?}");
}

#[test]
fn title_matches_rank_too() {
    let (_dir, idx) = corpus();
    // "Finish" appears only in the task's title column.
    let hits = idx.keyword_search("finish", None, 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind, "task");
    assert_eq!(hits[0].title, "Finish the rust fold");
}

// ── 5. ANN + embeddings ─────────────────────────────────────────────────────

/// Deterministic pseudo-vectors (LCG — hermetic tests, no RNG crates).
fn vec_for(seed: u64, dim: usize) -> Vec<f32> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..dim)
        .map(|_| {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((x >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0
        })
        .collect()
}

const MODEL: &str = "test-model";
const DIM: usize = 64;

fn seeded_index(dir: &std::path::Path, n: u64) -> SqliteIndex {
    let mut idx = SqliteIndex::open(dir, &keysource()).unwrap();
    for i in 1..=n {
        idx.upsert_embedding(
            "journal",
            &format!("j{i}"),
            0,
            MODEL,
            Some("nate"),
            &vec_for(i, DIM),
            &format!("hash-{i}"),
            "2026-07-10T13:00:00.000Z",
        )
        .unwrap();
    }
    idx
}

#[test]
fn ann_self_retrieval() {
    let dir = tempfile::tempdir().unwrap();
    let idx = seeded_index(dir.path(), 40);
    assert_eq!(idx.ann_len(MODEL), 40);
    for probe in [1u64, 17, 40] {
        let hits = idx.ann_candidates(MODEL, &vec_for(probe, DIM), 3).unwrap();
        assert_eq!(hits[0].ref_id, format!("j{probe}"), "hits: {hits:?}");
        assert_eq!(hits[0].ref_kind, "journal");
        assert_eq!(hits[0].chunk_idx, 0);
        assert!(hits[0].score > 0.999);
    }
    // Unknown model probes come back empty, not wrong.
    assert!(idx
        .ann_candidates("other-model", &vec_for(1, DIM), 3)
        .unwrap()
        .is_empty());
}

#[test]
fn embeddings_persist_and_ann_rebuilds_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let before = {
        let idx = seeded_index(dir.path(), 25);
        idx.ann_candidates(MODEL, &vec_for(9, DIM), 5).unwrap()
    };
    // Reopen: the ANN structure rebuilds from the embeddings table.
    let idx = SqliteIndex::open(dir.path(), &keysource()).unwrap();
    assert_eq!(idx.ann_len(MODEL), 25);
    let after = idx.ann_candidates(MODEL, &vec_for(9, DIM), 5).unwrap();
    assert_eq!(before, after, "reopen changed candidate results");
    // The persisted row keeps the Postgres-aligned columns.
    let (chunk_idx, dim, owner, hash): (i64, i64, String, String) = idx
        .conn()
        .query_row(
            "SELECT chunk_idx, dim, owner, hash FROM embeddings WHERE ref_kind = 'journal' AND ref_id = 'j9'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(chunk_idx, 0);
    assert_eq!(dim, DIM as i64);
    assert_eq!(owner, "nate");
    assert_eq!(hash, "hash-9");
}

#[test]
fn remove_embeddings_forgets_all_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = seeded_index(dir.path(), 10);
    // Add a second chunk for j3, then remove the whole item.
    idx.upsert_embedding(
        "journal",
        "j3",
        1,
        MODEL,
        Some("nate"),
        &vec_for(103, DIM),
        "hash-3b",
        "2026-07-10T13:00:00.000Z",
    )
    .unwrap();
    assert_eq!(idx.ann_len(MODEL), 11);
    idx.remove_embeddings("journal", "j3").unwrap();
    assert_eq!(idx.ann_len(MODEL), 9);
    let hits = idx.ann_candidates(MODEL, &vec_for(3, DIM), 10).unwrap();
    assert!(hits.iter().all(|h| h.ref_id != "j3"), "hits: {hits:?}");
    let n: i64 = idx
        .conn()
        .query_row(
            "SELECT count(*) FROM embeddings WHERE ref_id = 'j3'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
}

#[test]
fn upsert_replaces_vector_and_multi_chunk_hydrates() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = seeded_index(dir.path(), 5);
    // Replace j2's vector with j100's: probing j100 must now find j2.
    idx.upsert_embedding(
        "journal",
        "j2",
        0,
        MODEL,
        None,
        &vec_for(100, DIM),
        "hash-2b",
        "2026-07-10T13:00:01.000Z",
    )
    .unwrap();
    assert_eq!(idx.ann_len(MODEL), 5, "upsert must not grow the index");
    let hits = idx.ann_candidates(MODEL, &vec_for(100, DIM), 1).unwrap();
    assert_eq!(hits[0].ref_id, "j2");
    // Chunked rows hydrate with their chunk_idx (collapse happens above).
    idx.upsert_embedding(
        "journal",
        "j2",
        1,
        MODEL,
        None,
        &vec_for(101, DIM),
        "hash-2b",
        "2026-07-10T13:00:01.000Z",
    )
    .unwrap();
    let hits = idx.ann_candidates(MODEL, &vec_for(101, DIM), 1).unwrap();
    assert_eq!((hits[0].ref_id.as_str(), hits[0].chunk_idx), ("j2", 1));
}

#[test]
fn models_partition_the_ann() {
    let dir = tempfile::tempdir().unwrap();
    let mut idx = SqliteIndex::open(dir.path(), &keysource()).unwrap();
    idx.upsert_embedding(
        "journal",
        "a",
        0,
        "model-x",
        None,
        &vec_for(1, 16),
        "h1",
        &ts(0),
    )
    .unwrap();
    idx.upsert_embedding(
        "journal",
        "b",
        0,
        "model-y",
        None,
        &vec_for(2, 32),
        "h2",
        &ts(0),
    )
    .unwrap();
    assert_eq!(idx.ann_len("model-x"), 1);
    assert_eq!(idx.ann_len("model-y"), 1);
    let hits = idx.ann_candidates("model-x", &vec_for(1, 16), 5).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].ref_id, "a");
}
