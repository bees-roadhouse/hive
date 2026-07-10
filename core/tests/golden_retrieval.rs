// The golden retrieval fixture — the cross-backend parity oracle for the
// PR 1.6 SQLite cutover (docs/PLAN.md PR 1.3/1.6).
//
// A fixed, deterministic corpus is seeded through the NORMAL Store write
// paths (journal_append with bracket tokens + anchors, custom entity types +
// instances, links), embedded with the deterministic hash provider
// (HIVE_EMBED=hash), and queried three ways — keyword `search`,
// `semantic_search` (hybrid ON, reranker OFF — the hash provider has none),
// and `recall`. The expected ordered results live in
// tests/fixtures/golden_retrieval.json, checked in.
//
// ID STABILITY: `new_id()` mints a random nanoid per run, so raw ids can
// never appear in the fixture. Instead, every seeded item is keyed by a
// STABLE LABEL: after seeding, a map from freshly-minted id -> label is
// captured (journal entries by body, tasks/decisions/events by title, custom
// entities by title), and every hit id is translated to its label before
// comparison. Assertions are therefore id-independent: exact LABEL-set
// equality per query, exact order for the top 3, and scores within 1e-6.
//
// ORDER STABILITY: id randomness can also leak through ORDERING — the journal
// embed window sorts `created_at DESC, id DESC`, so two appends landing in
// the same millisecond order by random nanoid, which reorders the embeddings
// scan and flips which zero-similarity ties make a candidate-pool boundary.
// Seeding therefore sleeps 2ms between appends: strictly monotonic
// `created_at` means no ordering anywhere ever consults a random id.
//
// REGENERATING: run with HIVE_GOLDEN_REGEN=1 to rewrite the fixture, then
// diff it consciously:
//
//   HIVE_GOLDEN_REGEN=1 cargo test -p hive-core --test golden_retrieval
//
// The 1.6 cutover MAY relax the score tolerance (FTS5/bm25 replaces
// ts_rank — scores will not be bit-identical) but MUST keep the label-set
// equality and top-3 order assertions: those are the retrieval-behavior
// contract this fixture exists to freeze.
//
// CUTOVER PROVENANCE (PR 1.6): the fixture was regenerated ONCE against the
// SQLite backend after a documented investigation (full Postgres-vs-SQLite
// diff in the PR report). The vector path is bit-identical (hash embeddings +
// the same Rust cosine; every vector-dominated query matched with Δscore 0);
// the conscious differences are keyword-side and cross-backend by nature:
//   1. Stemming: Postgres `english` is Snowball/Porter2, whose exception
//      list holds "canning" invariant while "canned" stems to "can" — so
//      Postgres never matched the "Canned Brandywine Tomatoes" recipe for
//      the query "canning tomatoes" (verified against a live Postgres).
//      FTS5's porter (Porter1, no exception list) stems both to "can", so
//      the recipe now (correctly) hits and leads that query.
//   2. Ranking: bm25 replaces ts_rank; keyword ORDER feeds the hybrid blend
//      as rank positions, which reshuffles blend tails and swapped one
//      recall top-2 pair.
// With the baseline captured under THIS deterministic backend, the strict
// 1e-6 score tolerance is back in force (and the suite runs it twice).

mod common;

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use hive_core::store::recall::RecallOptions;
use hive_core::store::semantic::SemanticOptions;
use hive_core::store::Store;
use serde::{Deserialize, Serialize};
use serde_json::json;

const FIXTURE_PATH: &str = "tests/fixtures/golden_retrieval.json";
const SCORE_TOLERANCE: f64 = 1e-6;

fn hash_setup() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| std::env::set_var("HIVE_EMBED", "hash"));
    assert_eq!(
        hive_embed::embed_dim(),
        256,
        "golden fixture is captured under the deterministic hash provider"
    );
}

// ---- fixture shape ----

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct GoldenHit {
    /// Stable content label (NOT a database id — see the header).
    label: String,
    kind: String,
    score: f64,
}

#[derive(Debug, Serialize, Deserialize)]
struct GoldenQuery {
    /// "search" | "semantic" | "recall"
    surface: String,
    query: String,
    hits: Vec<GoldenHit>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GoldenFixture {
    /// Provenance notes for the human diffing a regen.
    captured_under: String,
    queries: Vec<GoldenQuery>,
}

// ---- corpus ----

/// Fixed journal bodies (author, body, optional anchor). Bracket tokens
/// exercise [person:], [topic:], [project:], [task:]; anchors materialise
/// tasks/decisions/events. Texts are frozen — editing one is a conscious
/// fixture regen.
const JOURNAL: &[(&str, &str)] = &[
    ("nate", "Spring inspection of the west apiary went well. [topic: Beekeeping] The queen in hive three is laying strong and the brood pattern is tight."),
    ("nate", "Ordered two packages of Carniolan bees for [project: Apiary Expansion] with [person: Maggie]. Delivery expected mid-April."),
    ("maggie", "The garden fence needs mending before the goats find the gap. [task: Mend the garden fence]"),
    ("nate", "Honey harvest planning: we should pull supers from the strong hives in late July. [topic: Honey Harvest] [project: Apiary Expansion]"),
    ("pia", "Reflected on the pantry inventory: we are low on preserved tomatoes and the [topic: Canning] shelf needs a reorganize."),
    ("nate", "Met with the county inspector about the [topic: Beekeeping] registration renewal. Paperwork is due by the end of the month. [task: File apiary registration]"),
    ("maggie", "Weekend plan: prep the [topic: Canning] jars, then start the tomato batch with [person: Nate]."),
    ("nate", "The smoker fuel experiment worked — dried lavender burns cool and calm. [topic: Beekeeping]"),
    ("pia", "Noted that [person: Maggie] prefers morning meetings; scheduling the seed order call for 9am."),
    ("nate", "Winter prep checklist drafted for [project: Apiary Expansion]: mouse guards, windbreaks, and candy boards. [task: Install mouse guards]"),
    ("maggie", "The orchard drainage ditch flooded again after the storm. We need gravel and a proper grade. [task: Regrade the orchard ditch]"),
    ("nate", "Read about oxalic acid vaporization for varroa control. [topic: Beekeeping] Worth trialing on hive two this fall."),
    ("pia", "Summarized the seed catalog: the heirloom brandywine tomatoes fit the [topic: Canning] plan for August."),
    ("nate", "Farmers market stall confirmed for Saturdays starting June. [project: Honey Sales] Pricing sheet still to do. [task: Draft honey pricing sheet]"),
    ("maggie", "Goat kidding season notes: two does due in March, the barn stall heater needs testing beforehand."),
    ("nate", "Hive two showed early swarm cells during the check. Added a super and will split next week if they persist. [topic: Beekeeping]"),
    ("nate", "Bottling day went long but we filled ninety jars of wildflower honey. [project: Honey Sales] [person: Maggie] labeled every one."),
    ("pia", "The workshop inventory shows three empty deep boxes and one medium — enough for the planned split. [topic: Beekeeping]"),
    ("maggie", "Started the sourdough experiment with the new starter. The kitchen smells wonderful."),
    ("nate", "Rain barrels are full after the storm; the drip lines to the herb bed are next. [task: Connect drip lines]"),
];

/// Anchored spans: (journal body to append, anchor kind, span text, title).
/// Appended as extra entries whose anchors materialise structured entities.
const ANCHORED: &[(&str, &str, &str, &str)] = &[
    (
        "Decision made after the inspector visit: we will register both apiary sites under one county permit to simplify renewals.",
        "decision",
        "we will register both apiary sites under one county permit",
        "Register both apiary sites under one permit",
    ),
    (
        "Event on the calendar: the beekeeping club field day happens at the fairgrounds next month.",
        "event",
        "the beekeeping club field day happens at the fairgrounds",
        "Beekeeping club field day",
    ),
    (
        "We must winterize the pump house before the first hard freeze hits the farm.",
        "task",
        "winterize the pump house before the first hard freeze",
        "Winterize the pump house",
    ),
];

/// Custom entity types + instances (slug, name, instances as (title, field)).
const RECIPES: &[(&str, &str)] = &[
    ("Wildflower Honey Cake", "dessert"),
    ("Canned Brandywine Tomatoes", "preserve"),
];
const TOOLS: &[(&str, &str)] = &[
    ("Oxalic Acid Vaporizer", "apiary"),
    ("Fence Post Driver", "pasture"),
];

/// The representative queries, chosen to exercise: exact keyword hits,
/// stemmed/partial keyword hits, semantic-only similarity, bracket-token
/// entities (topics/projects/people), anchored entities (task/decision/event
/// kinds), custom entity kinds, multi-kind blends, and the recall journal
/// path.
const SEARCH_QUERIES: &[&str] = &[
    "honey harvest supers",
    "varroa oxalic acid",
    "garden fence goats",
    "canning tomatoes",
    "apiary registration paperwork",
];
const SEMANTIC_QUERIES: &[&str] = &[
    "honey harvest planning",
    "controlling mites in the hive",
    "preserving tomatoes for winter",
    "farm infrastructure repairs",
    "swarm prevention",
];
const RECALL_QUERIES: &[(&str, &str)] = &[
    ("nate", "beekeeping inspections"),
    ("maggie", "garden and orchard work"),
];

/// Strictly monotonic timestamps between writes (see ORDER STABILITY above).
async fn tick() {
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
}

async fn seed(store: &Store) -> anyhow::Result<HashMap<String, String>> {
    // Seeding is strictly sequential AND strictly monotonic in created_at.
    for (author, body) in JOURNAL {
        store
            .journal_append(
                serde_json::from_value(json!({"body": body}))?,
                Some(author),
                Some("nate"),
            )
            .await?;
        tick().await;
    }
    for (body, kind, span, title) in ANCHORED {
        let start = body.find(span).expect("span in body") as i64;
        let end = start + span.len() as i64;
        store
            .journal_append(
                serde_json::from_value(json!({
                    "body": body,
                    "anchors": [{
                        "start": start,
                        "end": end,
                        "kind": kind,
                        "fields": {"title": title}
                    }]
                }))?,
                Some("nate"),
                Some("nate"),
            )
            .await?;
        tick().await;
    }

    for (slug, name, instances) in [("recipe", "Recipe", RECIPES), ("tool", "Tool", TOOLS)] {
        store
            .entity_types_create(
                serde_json::from_value(json!({
                    "name": name,
                    "slug": slug,
                    "fields": [{"label": "Category", "slug": "category", "field_type": "text"}],
                }))?,
                "nate",
            )
            .await
            .map_err(|_| anyhow::anyhow!("type create failed"))?;
        for (title, category) in instances {
            store
                .custom_entities_create(
                    serde_json::from_value(json!({
                        "type": slug,
                        "title": title,
                        "fields": {"category": category},
                    }))?,
                    "nate",
                    Some("nate"),
                )
                .await
                .map_err(|_| anyhow::anyhow!("entity create failed"))?;
        }
    }

    // A couple of explicit cross-entity links so the Markov-blanket boost has
    // graph structure to walk. (Reads ride the raw_sql diagnostics seam — the
    // Postgres-client calls died with the cutover; same queries, same rows.)
    let honey_entries: Vec<String> = store
        .raw_sql(
            "SELECT id FROM journal WHERE body LIKE '%Honey harvest planning%' OR body LIKE '%Bottling day%' ORDER BY created_at",
            vec![],
        )
        .await?
        .into_iter()
        .filter_map(|row| row[0].as_str().map(str::to_string))
        .collect();
    if let [a, b] = honey_entries.as_slice() {
        store
            .links_create("journal", a, "journal", b, "relates")
            .await?;
    }

    // Embed the corpus through the normal backfill path.
    store.backfill_embeddings().await?;

    // ---- stable-label map (see header): id -> content label ----
    let mut labels: HashMap<String, String> = HashMap::new();
    let pairs = |rows: Vec<Vec<serde_json::Value>>| -> Vec<(String, String)> {
        rows.into_iter()
            .filter_map(|row| Some((row[0].as_str()?.to_string(), row[1].as_str()?.to_string())))
            .collect()
    };
    for (id, body) in pairs(
        store
            .raw_sql("SELECT id, body FROM journal", vec![])
            .await?,
    ) {
        let label: String = body.chars().take(40).collect();
        labels.insert(id, format!("journal:{label}"));
    }
    for (table, kind) in [
        ("tasks", "task"),
        ("decisions", "decision"),
        ("events", "event"),
    ] {
        for (id, title) in pairs(
            store
                .raw_sql(&format!("SELECT id, title FROM {table}"), vec![])
                .await?,
        ) {
            labels.insert(id, format!("{kind}:{title}"));
        }
    }
    for (id, title) in pairs(
        store
            .raw_sql("SELECT id, title FROM entities", vec![])
            .await?,
    ) {
        labels.insert(id, format!("entity:{title}"));
    }
    Ok(labels)
}

fn label_for<'a>(labels: &'a HashMap<String, String>, id: &str) -> &'a str {
    labels
        .get(id)
        .map(String::as_str)
        .unwrap_or_else(|| panic!("hit id {id} has no stable label — unlabeled kind in results?"))
}

async fn capture(store: &Store, labels: &HashMap<String, String>) -> anyhow::Result<GoldenFixture> {
    let mut queries = Vec::new();

    for q in SEARCH_QUERIES {
        let hits = store.search(q, 10).await?;
        queries.push(GoldenQuery {
            surface: "search".into(),
            query: (*q).into(),
            hits: hits
                .iter()
                .map(|h| GoldenHit {
                    label: label_for(labels, &h.id).to_string(),
                    kind: h.kind.clone(),
                    score: h.score,
                })
                .collect(),
        });
    }

    for q in SEMANTIC_QUERIES {
        // Reranker OFF (the hash provider has none — precision would silently
        // degrade anyway; standard keeps the surface explicit), hybrid ON.
        let hits = store
            .semantic_search(
                q,
                SemanticOptions {
                    limit: Some(10),
                    hybrid: Some(true),
                    rerank: Some(false),
                    ..Default::default()
                },
            )
            .await?;
        queries.push(GoldenQuery {
            surface: "semantic".into(),
            query: (*q).into(),
            hits: hits
                .iter()
                .map(|h| GoldenHit {
                    label: label_for(labels, &h.id).to_string(),
                    kind: h.kind.clone(),
                    score: h.score,
                })
                .collect(),
        });
    }

    for (identity, q) in RECALL_QUERIES {
        let recall = store
            .recall(
                identity,
                RecallOptions {
                    query: Some((*q).into()),
                    ..Default::default()
                },
            )
            .await?;
        queries.push(GoldenQuery {
            surface: "recall".into(),
            query: format!("{identity}: {q}"),
            hits: recall
                .data
                .journal
                .iter()
                .map(|h| GoldenHit {
                    label: label_for(labels, &h.hit.id).to_string(),
                    kind: h.hit.kind.clone(),
                    score: h.hit.score,
                })
                .collect(),
        });
    }

    Ok(GoldenFixture {
        captured_under: format!(
            "sqlite FTS5 porter/bm25 + brute-force cosine, hash embedder (dim {}), reranker off, hybrid on",
            hive_embed::embed_dim()
        ),
        queries,
    })
}

fn fixture_file() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_PATH)
}

#[tokio::test]
async fn golden_retrieval_matches_the_checked_in_fixture() {
    hash_setup();
    let store = common::test_store().await;
    let labels = seed(&store).await.expect("seed corpus");
    let got = capture(&store, &labels).await.expect("capture");

    if std::env::var("HIVE_GOLDEN_REGEN").as_deref() == Ok("1") {
        let out = serde_json::to_string_pretty(&got).expect("serialize fixture");
        std::fs::write(fixture_file(), out + "\n").expect("write fixture");
        eprintln!("golden fixture regenerated at {FIXTURE_PATH} — diff it consciously");
        return;
    }

    let raw = std::fs::read_to_string(fixture_file())
        .expect("fixture missing — run once with HIVE_GOLDEN_REGEN=1");
    let want: GoldenFixture = serde_json::from_str(&raw).expect("parse fixture");

    assert_eq!(
        got.queries.len(),
        want.queries.len(),
        "query count changed — regenerate consciously"
    );
    for (g, w) in got.queries.iter().zip(&want.queries) {
        assert_eq!(g.surface, w.surface);
        assert_eq!(g.query, w.query);
        let key = format!("[{} · {}]", g.surface, g.query);

        // 1. Exact label-SET equality (order-independent membership).
        let got_set: BTreeMap<&str, &str> = g
            .hits
            .iter()
            .map(|h| (h.label.as_str(), h.kind.as_str()))
            .collect();
        let want_set: BTreeMap<&str, &str> = w
            .hits
            .iter()
            .map(|h| (h.label.as_str(), h.kind.as_str()))
            .collect();
        assert_eq!(got_set, want_set, "{key} result set drifted");

        // 2. Exact order for the top 3.
        let got_top: Vec<&str> = g.hits.iter().take(3).map(|h| h.label.as_str()).collect();
        let want_top: Vec<&str> = w.hits.iter().take(3).map(|h| h.label.as_str()).collect();
        assert_eq!(got_top, want_top, "{key} top-3 order drifted");

        // 3. Scores within tolerance, position by position.
        assert_eq!(g.hits.len(), w.hits.len(), "{key} hit count drifted");
        for (i, (gh, wh)) in g.hits.iter().zip(&w.hits).enumerate() {
            assert!(
                (gh.score - wh.score).abs() <= SCORE_TOLERANCE,
                "{key} hit {i} ({}) score {} vs {} exceeds {SCORE_TOLERANCE}",
                gh.label,
                gh.score,
                wh.score,
            );
        }
    }
}
