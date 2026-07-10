// Search query side — parity port of store.ts `search` (keyword),
// `semanticSearch` (the standard/precision hybrid cascade), `embeddableItems`,
// and `embeddingStats`, on the SQLite index. Keyword search rides
// SqliteIndex::keyword_search (FTS5 MATCH + bm25 + snippet — the successor of
// tsvector/ts_rank/ts_headline); vector candidates ride the AnnIndex seam for
// the 384-dim model and the brute-force BYTEA-style scan for everything else
// (the 256-dim hash provider in dev/CI), so the whole cascade stays exercised
// without ONNX and hash-provider scores stay bit-identical to the Postgres
// brute-force path (same vectors, same Rust cosine).
//
// Decoupling note: journal/tasks/decisions/events/links data is read via
// private SQL here (not via those store modules) — the orchestrator dedups at
// integration.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use hive_shared::{EmbeddingKindCount, EmbeddingModelCount, EmbeddingStats, EntityKind, SearchHit};

use super::{placeholders_or_never, Core, Store};

/// Options for `semantic_search` — mirrors store.ts `SemanticOptions` (every
/// field optional; defaults applied inside, exactly as the Node code does).
#[derive(Debug, Clone, Default)]
pub struct SemanticOptions {
    pub limit: Option<usize>,
    /// Drop vector matches scoring below this cosine value (default 0).
    pub threshold: Option<f64>,
    /// Blend FTS keyword ranks into the score (default true).
    pub hybrid: Option<bool>,
    /// Cross-encoder rerank (default false on standard; always on precision).
    pub rerank: Option<bool>,
    /// Markov-blanket boost from the links graph (default true).
    pub blanket: Option<bool>,
    /// "standard" (default) | "precision".
    pub mode: Option<String>,
    /// Boost (not filter) hits whose actors include this actor.
    pub identity: Option<String>,
    /// Boost (not filter) hits whose actors include this actor.
    pub peer: Option<String>,
    /// Restrict results to these kinds (a filter, not a boost). None = all.
    /// Applied inside the cascade so excluded kinds never occupy pool slots.
    pub kinds: Option<Vec<EntityKind>>,
}

/// Everything worth embedding (store.ts `embeddableItems`). `text` is the
/// clean body (rerank + display); `embed_text` carries the `[kind] title`
/// context prefix; `hash` stamps re-embeds; `owner` is the namespace user the
/// embedding rows get stamped with (NULL = global) — journal entries carry
/// their own user_scope, task/decision/event inherit their origin entry's.
pub struct EmbeddableItem {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub text: String,
    pub embed_text: String,
    pub hash: String,
    pub owner: Option<String>,
}

/// JS `String.prototype.slice(0, n)` — UTF-16 code units, not chars.
fn js_slice(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// Display/rerank material for one ranked hit, fetched keyed (never a corpus
/// scan). `snippet` is only populated for kinds that store one (mail).
struct HydratedHit {
    title: String,
    text: String,
    snippet: String,
}

/// Kinds whose sheer row count could flood an unrestricted candidate pool
/// (mail post-backfill is ~200k chunks vs ~1k journal). The double-probe and
/// the diversified pool fill treat these as "bulk". JSON array or
/// comma-separated; `[]` disables the diversification.
fn bulk_kinds() -> Vec<String> {
    match std::env::var("HIVE_SEARCH_BULK_KINDS") {
        Ok(raw) => parse_bulk_kinds(&raw),
        Err(_) => vec!["mail".to_string()],
    }
}

fn parse_bulk_kinds(raw: &str) -> Vec<String> {
    let raw = raw.trim();
    if let Ok(serde_json::Value::Array(a)) = serde_json::from_str(raw) {
        return a
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .collect();
    }
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Default per-kind rank weights (mail is deliberately demoted so bulk archive
/// material doesn't outrank curated memory at equal similarity). Unlisted
/// kinds weigh 1.0.
fn default_kind_weights() -> HashMap<String, f64> {
    HashMap::from([("mail".to_string(), 0.85)])
}

/// Merge a `{"kind": weight}` JSON object over the defaults. Non-object or
/// non-numeric entries are ignored (never fail a search over bad config).
fn merge_kind_weights(base: &mut HashMap<String, f64>, raw: &str) {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Object(map)) => {
            for (k, v) in map {
                if let Some(f) = v.as_f64() {
                    base.insert(k, f);
                }
            }
        }
        _ => tracing::warn!("unparseable search.kind_weights JSON, using defaults"),
    }
}

/// Collapse chunk-level vector rows to one row per parent `kind:id`, scored by
/// the best chunk (MAX sim). Input rows may repeat keys (chunks, and the
/// double-probe unions overlap); output is sorted best-first.
fn collapse_chunks(mut rows: Vec<(String, f64)>) -> Vec<(String, f64)> {
    rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut seen: HashSet<String> = HashSet::new();
    rows.retain(|(k, _)| seen.insert(k.clone()));
    rows
}

fn ref_key(kind: &str, id: &str) -> String {
    format!("{kind}:{id}")
}

fn split_key(k: &str) -> (&str, &str) {
    match k.find(':') {
        Some(ix) => (&k[..ix], &k[ix + 1..]),
        None => (k, ""),
    }
}

/// Per-candidate blend components (store.ts `Score`).
#[derive(Default, Clone, Copy)]
struct Score {
    vector: f64,
    keyword: f64,
    blanket: f64,
}

/// Insertion-ordered score map (JS `Map` parity — iteration order is
/// load-bearing for stable-sort tie behavior).
#[derive(Default)]
struct ScoreMap {
    order: Vec<String>,
    map: HashMap<String, Score>,
}

impl ScoreMap {
    fn entry(&mut self, key: &str) -> &mut Score {
        if !self.map.contains_key(key) {
            self.order.push(key.to_string());
            self.map.insert(key.to_string(), Score::default());
        }
        self.map.get_mut(key).unwrap()
    }

    fn len(&self) -> usize {
        self.order.len()
    }

    fn keys(&self) -> &[String] {
        &self.order
    }

    fn get(&self, key: &str) -> Option<&Score> {
        self.map.get(key)
    }

    /// Remove keys, preserving the insertion order of the survivors (JS
    /// `Map.delete` parity).
    fn retain_keys(&mut self, keep: &HashSet<String>) {
        self.order.retain(|k| keep.contains(k));
        self.map.retain(|k, _| keep.contains(k));
    }

    fn entries(&self) -> impl Iterator<Item = (&String, &Score)> {
        self.order.iter().map(move |k| (k, &self.map[k]))
    }
}

/// Journal embedding window: only the newest N entries are embeddable
/// (`embeddable_items` below) and the reaper (store/maintenance.rs) deletes
/// vectors for anything beyond it. Both sides MUST share this constant — if
/// they diverge, the backfill re-embeds rows the reaper just deleted (or the
/// reaper leaks rows the backfill stopped refreshing), every cycle, forever.
pub(crate) const JOURNAL_EMBED_WINDOW: i64 = 1000;

/// kind → table for the anchored built-ins. Shared with the reaper so its
/// orphan sweeps cover exactly the kinds embedded here.
pub(crate) const ORIGIN_TABLE: &[(&str, &str)] = &[
    ("task", "tasks"),
    ("decision", "decisions"),
    ("event", "events"),
];

fn origin_table(kind: &str) -> Option<&'static str> {
    ORIGIN_TABLE
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, t)| *t)
}

impl Store {
    /// Keyword search (store.ts `search`), unscoped. FTS5 bm25 ranking,
    /// snippet() excerpts — SqliteIndex::keyword_search.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let query = query.to_string();
        self.run(move |core| {
            let mut hits = core.index.keyword_search(&query, None, limit)?;
            hits.truncate(limit);
            Ok(hits)
        })
        .await
    }

    /// Every item worth embedding (store.ts `embeddableItems`). Public: the
    /// backfill iterates this exactly like Node's worker did.
    pub async fn embeddable_items(&self) -> Result<Vec<EmbeddableItem>> {
        self.run(|core| embeddable_items_core(core)).await
    }

    /// Admin view of the embedding corpus (store.ts `embeddingStats`).
    pub async fn embedding_stats(&self) -> Result<EmbeddingStats> {
        let model = self.embedder().model();
        self.run(move |core| {
            let items = embeddable_items_core(core)?;
            let mut stored: HashMap<String, String> = HashMap::new();
            {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT ref_kind, ref_id, hash FROM embeddings")?;
                let rows = stmt.query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                for row in rows {
                    let (kind, id, hash) = row?;
                    stored.insert(ref_key(&kind, &id), hash);
                }
            }
            let pending = items
                .iter()
                .filter(|it| stored.get(&ref_key(&it.kind, &it.id)) != Some(&it.hash))
                .count();

            let total: i64 = core
                .conn()
                .query_row("SELECT count(*) FROM embeddings", [], |r| r.get(0))?;
            let by_kind: Vec<EmbeddingKindCount> = {
                let mut stmt = core.conn().prepare(
                    "SELECT ref_kind AS kind, count(*) AS count FROM embeddings GROUP BY ref_kind ORDER BY count DESC",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(EmbeddingKindCount {
                        kind: r.get(0)?,
                        count: r.get(1)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let by_model: Vec<EmbeddingModelCount> = {
                let mut stmt = core.conn().prepare(
                    "SELECT model, dim, count(*) AS count FROM embeddings GROUP BY model, dim ORDER BY count DESC",
                )?;
                let rows = stmt.query_map([], |r| {
                    Ok(EmbeddingModelCount {
                        model: r.get(0)?,
                        dim: r.get(1)?,
                        count: r.get(2)?,
                    })
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };

            Ok(EmbeddingStats {
                total,
                model,
                embeddable: items.len() as i64,
                pending: pending as i64,
                by_kind,
                by_model,
            })
        })
        .await
    }

    /// Per-kind rank weights: defaults ← `search.kind_weights` config JSON ←
    /// `HIVE_SEARCH_KIND_WEIGHTS` env (strongest). Merged per key, so an
    /// override only has to name the kinds it changes.
    async fn kind_weights(&self) -> HashMap<String, f64> {
        let mut w = default_kind_weights();
        if let Ok(Some(raw)) = self.config_get("search.kind_weights").await {
            merge_kind_weights(&mut w, &raw);
        }
        if let Ok(raw) = std::env::var("HIVE_SEARCH_KIND_WEIGHTS") {
            if !raw.trim().is_empty() {
                merge_kind_weights(&mut w, &raw);
            }
        }
        w
    }

    /// Semantic search — store.ts `semanticSearch`, the full standard|precision
    /// hybrid pipeline (vector pass → FTS blend → Markov-blanket boost →
    /// identity/peer soft boosts → optional cross-encoder rerank). The `kinds`
    /// filter applies BEFORE every pool cut, not as a post-filter, so excluded
    /// kinds can't starve the result pool.
    pub async fn semantic_search(
        &self,
        query: &str,
        opts: SemanticOptions,
    ) -> Result<Vec<SearchHit>> {
        let limit = opts.limit.unwrap_or(10);
        let threshold = opts.threshold.unwrap_or(0.0);
        let precision = opts.mode.as_deref() == Some("precision");
        let hybrid = opts.hybrid.unwrap_or(true);
        let use_blanket = opts.blanket.unwrap_or(true);
        // Precision forces the cross-encoder on; standard honors the flag.
        // Either way it only engages when a reranker is actually available, so
        // precision degrades to the standard blend under hash/CI (#47).
        let embedder = self.embedder().clone();
        let rerank_avail = tokio::task::spawn_blocking(move || embedder.rerank_available()).await?;
        let use_rerank = (precision || opts.rerank.unwrap_or(false)) && rerank_avail;
        if query.trim().is_empty() {
            return Ok(vec![]);
        }

        // Cascade over-fetch: precision widens each stage's pool.
        let stage1_pool = if precision {
            (limit * 4).max(limit)
        } else {
            (limit * 2).max(limit)
        };
        let stage2_pool = if precision {
            (limit * 3).max(limit)
        } else {
            limit * 2
        };

        // 1. Vector candidate pass. Provider dispatch: the 384-dim model uses
        // the ANN seam; anything else (the 256-dim hash provider in dev/CI)
        // keeps the brute-force scan so the whole cascade below stays
        // exercised without ONNX. The q.len() guard covers the transformers
        // latch: a mid-flight model failure degrades embed_query to 256-dim
        // hash vectors, which must not probe a 384-dim structure.
        let embedder = self.embedder().clone();
        let owned_query = query.to_string();
        let q = tokio::task::spawn_blocking(move || embedder.embed_query(&owned_query)).await?;
        let chunk_k = (stage1_pool * 4).max(limit) as i64;
        let bulk = bulk_kinds();
        let model = self.embedder().model();
        let use_ann = self.embedder().dim() == 384 && q.len() == 384;
        let kinds_owned = opts.kinds.clone();
        let bulk_for_probe = bulk.clone();
        let raw_rows: Vec<(String, f64)> = self
            .run(move |core| {
                if use_ann {
                    ann_candidate_rows(
                        core,
                        &model,
                        &q,
                        kinds_owned.as_deref(),
                        chunk_k as usize,
                        &bulk_for_probe,
                    )
                } else {
                    brute_candidate_rows(core, &model, &q, kinds_owned.as_deref())
                }
            })
            .await?;
        // Chunked rows share one `kind:id` key — collapse to the item's best
        // chunk (MAX sim) so pool slots, the fallback, and the final hit list
        // stay one-entry-per-item. Everything downstream works on parent keys.
        let mut scored_all = collapse_chunks(raw_rows);

        // Over-fetch keyword rows when a kinds filter will thin the pool.
        // Blend/scoring order below is unchanged.
        let kw_fetch = if opts.kinds.is_some() {
            stage2_pool * 5
        } else {
            stage2_pool
        };
        let kw_all: Vec<SearchHit> = if hybrid {
            self.search(query, kw_fetch).await?
        } else {
            Vec::new()
        };

        // `admit` gates every pool cut: a candidate enters only if its kind
        // parses when a kinds filter is set (a newer binary's rows must not
        // hold slots this build drops at hydration). The typed filter can
        // only name built-ins, so when it's set it excludes everything else
        // (custom slugs included).
        let admit = |kind_s: &str| -> bool {
            match opts.kinds.as_ref() {
                Some(ks) => matches!(EntityKind::parse(kind_s), Some(kind) if ks.contains(&kind)),
                None => true,
            }
        };

        // Admission applies before ANY cut — the threshold filter, the stage
        // pools, and the fallback refill all see an already-admitted list.
        scored_all.retain(|(k, _)| {
            let (kind, _) = split_key(k);
            admit(kind)
        });
        let passing: Vec<&(String, f64)> =
            scored_all.iter().filter(|(_, s)| *s >= threshold).collect();
        let raw_hit_keys: HashSet<String> = passing.iter().map(|(k, _)| k.clone()).collect();

        // Diversified stage-1 fill: the best candidates overall, PLUS (when
        // no kinds filter narrows the query) the best non-bulk candidates —
        // so a 200k-chunk mailbox can never evict journal/tasks from the
        // pool. ScoreMap dedups; insertion order stays deterministic.
        let mut scores = ScoreMap::default();
        for (k, s) in passing.iter().take(stage1_pool) {
            scores.entry(k).vector = *s;
        }
        if opts.kinds.is_none() && !bulk.is_empty() {
            for (k, s) in passing
                .iter()
                .filter(|(k, _)| {
                    let (kind, _) = split_key(k);
                    !bulk.iter().any(|b| b == kind)
                })
                .take(stage1_pool)
            {
                scores.entry(k).vector = *s;
            }
        }

        // 2. Keyword pass (FTS) — rank-based score decaying from the top.
        if hybrid {
            // Over-fetched above when a kinds filter will thin the pool;
            // admit BEFORE the stage cut, so excluded-kind rows can't hold
            // keyword slots.
            let mut kw = kw_all;
            kw.retain(|r| admit(r.kind.as_str()));
            kw.truncate(stage2_pool);
            let total = kw.len().max(1) as f64;
            for (i, r) in kw.iter().enumerate() {
                let kk = ref_key(r.kind.as_str(), &r.id);
                scores.entry(&kk).keyword = 1.0 - i as f64 / total;
            }
            if precision && scores.len() > stage2_pool {
                let mut blend: Vec<(String, f64)> = scores
                    .entries()
                    .map(|(k, s)| (k.clone(), s.vector * 0.6 + s.keyword * 0.4))
                    .collect();
                blend.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                let kept: HashSet<String> = blend
                    .into_iter()
                    .take(stage2_pool)
                    .map(|(k, _)| k)
                    .collect();
                scores.retain_keys(&kept);
            }
        }

        // 3. Markov-blanket boost: neighbor in the final set (+0.05, cap 0.15),
        // neighbor with a vector hit that missed the cut (+0.02, cap 0.06).
        if use_blanket {
            let scored_keys: HashSet<String> = scores.keys().iter().cloned().collect();
            let keys = scores.keys().to_vec();
            let neighbor_map = self
                .run(move |core| {
                    let mut out: HashMap<String, Vec<String>> = HashMap::new();
                    for kk in &keys {
                        let (k, id) = split_key(kk);
                        out.insert(kk.clone(), blanket_neighbors_core(core, k, id)?);
                    }
                    Ok(out)
                })
                .await?;
            for kk in scores.keys().to_vec() {
                let mut strong = 0usize;
                let mut weak = 0usize;
                for nk in neighbor_map.get(&kk).map(Vec::as_slice).unwrap_or(&[]) {
                    if scored_keys.contains(nk) {
                        strong += 1;
                    } else if raw_hit_keys.contains(nk) {
                        weak += 1;
                    }
                }
                if strong > 0 || weak > 0 {
                    scores.entry(&kk).blanket =
                        (strong as f64 * 0.05).min(0.15) + (weak as f64 * 0.02).min(0.06);
                }
            }
        }

        // Drop keyword-only noise — a keyword hit with zero semantic relevance.
        if hybrid {
            let keep: HashSet<String> = scores
                .entries()
                .filter(|(_, s)| !(s.vector == 0.0 && s.keyword > 0.0))
                .map(|(k, _)| k.clone())
                .collect();
            scores.retain_keys(&keep);
        }

        // 4. Blended sort, with the per-kind rank weight (multiplicative,
        // applied exactly once) and then the identity/peer soft boost (+0.1 —
        // a nudge, not a filter).
        let weights = self.kind_weights().await;
        let focus: HashSet<String> = [opts.identity.clone(), opts.peer.clone()]
            .into_iter()
            .flatten()
            .collect();
        let actor_map: HashMap<String, Vec<String>> = if focus.is_empty() {
            HashMap::new()
        } else {
            let keys = scores.keys().to_vec();
            self.run(move |core| {
                let mut out = HashMap::new();
                for kk in &keys {
                    let (kind, id) = split_key(kk);
                    out.insert(kk.clone(), hit_actors_core(core, kind, id)?);
                }
                Ok(out)
            })
            .await?
        };
        let (w_vec, w_kw) = if precision { (0.55, 0.25) } else { (0.7, 0.2) };
        let pre_rerank_n = if precision && use_rerank {
            (limit * 2).max(limit)
        } else {
            limit
        };
        let mut ranked: Vec<(String, f64)> = Vec::with_capacity(scores.len());
        for kk in scores.keys().to_vec() {
            let s = *scores.get(&kk).unwrap();
            let (kind, _id) = split_key(&kk);
            let scoped = if focus.is_empty() {
                0.0
            } else if actor_map
                .get(&kk)
                .map(Vec::as_slice)
                .unwrap_or(&[])
                .iter()
                .any(|a| focus.contains(a.as_str()))
            {
                0.1
            } else {
                0.0
            };
            let base = if s.keyword > 0.0 && s.vector > 0.0 {
                s.vector * w_vec + s.keyword * w_kw + s.blanket
            } else {
                s.vector + s.blanket
            };
            let kind_w = weights.get(kind).copied().unwrap_or(1.0);
            ranked.push((kk, base * kind_w + scoped));
        }
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        // 5. Fallback — never return empty when admitted vectors exist
        // (scored_all was admission-filtered above, so this can't smuggle
        // unknown-kind rows back in). Runs before hydration so fallback keys
        // hydrate too.
        if ranked.is_empty() && !scored_all.is_empty() {
            ranked = scored_all.iter().take(limit).cloned().collect();
        }

        // 6. Keyed hydration over the ranked pool (bounded — never the old
        // corpus scan). A key that doesn't hydrate is DROPPED before the
        // pre-rerank cut, so an orphaned embedding can't hold a result slot
        // OR starve the survivors out of theirs.
        let keys: Vec<String> = ranked.iter().map(|(k, _)| k.clone()).collect();
        let hydrated = self.run(move |core| hydrate_hits_core(core, &keys)).await?;
        ranked.retain(|(k, _)| hydrated.contains_key(k));
        ranked.truncate(pre_rerank_n);

        // 7. Cross-encoder rerank (precision stage 4, or the standard flag).
        if use_rerank && !ranked.is_empty() {
            let docs: Vec<String> = ranked
                .iter()
                .map(|(k, _)| hydrated.get(k).map(|h| h.text.clone()).unwrap_or_default())
                .collect();
            let embedder = self.embedder().clone();
            let owned_query = query.to_string();
            let rr =
                tokio::task::spawn_blocking(move || embedder.rerank(&owned_query, &docs)).await?;
            if let Some(rr) = rr {
                let mut reranked: Vec<(String, f64)> = ranked
                    .into_iter()
                    .zip(rr)
                    .map(|((k, _), score)| (k, score))
                    .collect();
                reranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                ranked = reranked;
            }
        }
        ranked.truncate(limit);

        let hits: Vec<SearchHit> = ranked
            .into_iter()
            .filter_map(|(k, score)| {
                let (kind, id) = split_key(&k);
                let h = hydrated.get(&k)?;
                Some(SearchHit {
                    kind: kind.to_string(),
                    id: id.to_string(),
                    title: h.title.clone(),
                    snippet: h.snippet.clone(),
                    score: (score * 1000.0).round() / 1000.0,
                })
            })
            .collect();

        Ok(hits)
    }
}

// ── core-level pieces (run on the writer thread) ─────────────────────────────

pub(crate) fn embeddable_items_core(core: &Core) -> Result<Vec<EmbeddableItem>> {
    let conn = core.conn();
    let mut out: Vec<EmbeddableItem> = Vec::new();
    let mut push = |kind: &str, id: String, title: String, text: String, owner: Option<String>| {
        let embed_text = format!("[{kind}] {title}\n\n{text}");
        let hash = hive_embed::content_hash(&embed_text);
        out.push(EmbeddableItem {
            kind: kind.to_string(),
            id,
            title,
            text,
            embed_text,
            hash,
            owner,
        });
    };

    // Window + `id` tiebreak match the reaper's journal sweep exactly
    // (store/maintenance.rs) — a nondeterministic boundary row would
    // churn embed/reap forever.
    {
        let mut stmt = conn.prepare(
            "SELECT id, author, body, user_scope FROM journal ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![JOURNAL_EMBED_WINDOW], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, author, body, owner) = row?;
            push(
                "journal",
                id,
                format!("{author}: {}", js_slice(&body, 40)),
                body,
                owner,
            );
        }
    }

    // Anchored entities inherit their origin entry's scope — resolved in
    // the same query (one LEFT JOIN per table, no per-item lookups).
    // Rows without an origin entry (or with a global one) stay global.
    {
        let mut stmt = conn.prepare(
            "SELECT t.id, t.title, t.body, j.user_scope AS owner FROM tasks t \
             LEFT JOIN journal j ON j.id = t.origin_entry_id \
             ORDER BY CASE t.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, t.created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, title, body, owner) = row?;
            let text = format!("{title} {body}");
            push("task", id, title, text, owner);
        }
    }

    {
        let mut stmt = conn.prepare(
            "SELECT d.id, d.title, d.context, d.decision, d.consequences, j.user_scope AS owner \
             FROM decisions d LEFT JOIN journal j ON j.id = d.origin_entry_id \
             ORDER BY d.created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        })?;
        for row in rows {
            let (id, title, context, decision, consequences, owner) = row?;
            let text = format!("{title} {context} {decision} {consequences}");
            push("decision", id, title, text, owner);
        }
    }

    {
        let mut stmt = conn.prepare(
            "SELECT e.id, e.title, e.body, j.user_scope AS owner FROM events e \
             LEFT JOIN journal j ON j.id = e.origin_entry_id \
             ORDER BY COALESCE(e.at, e.created_at) DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, title, body, owner) = row?;
            let text = format!("{title} {body}");
            push("event", id, title, text, owner);
        }
    }

    Ok(out)
}

/// The actors associated with a hit (store.ts `hitActors`): journal →
/// author + mentions; task/decision/event → assignees.
fn hit_actors_core(core: &Core, kind: &str, ref_id: &str) -> Result<Vec<String>> {
    use rusqlite::OptionalExtension;
    let conn = core.conn();
    if kind == "journal" {
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT author, mentions FROM journal WHERE id = ?1",
                rusqlite::params![ref_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((author, mentions)) = row else {
            return Ok(vec![]);
        };
        let mut actors = vec![author];
        actors.extend(super::json_vec(&mentions));
        return Ok(actors);
    }
    let Some(table) = origin_table(kind) else {
        return Ok(vec![]);
    };
    let assignees: Option<String> = conn
        .query_row(
            &format!("SELECT assignees FROM {table} WHERE id = ?1"),
            rusqlite::params![ref_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(assignees.map(|a| super::json_vec(&a)).unwrap_or_default())
}

/// Neighbors of an entity in the links graph, either direction (store.ts
/// `blanketNeighbors` — the Markov blanket).
fn blanket_neighbors_core(core: &Core, kind: &str, id: &str) -> Result<Vec<String>> {
    let mut stmt = core.conn().prepare(
        "SELECT target_kind AS k, target_id AS i FROM links WHERE source_kind = ?1 AND source_id = ?2 \
         UNION \
         SELECT source_kind AS k, source_id AS i FROM links WHERE target_kind = ?1 AND target_id = ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![kind, id], |r| {
        Ok(ref_key(
            r.get::<_, String>(0)?.as_str(),
            r.get::<_, String>(1)?.as_str(),
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// ANN candidate stage over the AnnIndex seam. Returns chunk-level
/// `(kind:id, sim)` rows, NOT yet collapsed.
///
/// When `kinds` is None a second probe excludes the bulk kinds so mail
/// volume can never evict journal/tasks from the candidate set — union'd
/// and deduped by the caller's collapse. The seam has no kind filter, so
/// filtered probes fetch the whole candidate list and post-filter — exact
/// (the 1.5 ANN is a brute-force scan) and personal-scale cheap.
fn ann_candidate_rows(
    core: &Core,
    model: &str,
    q: &[f32],
    kinds: Option<&[EntityKind]>,
    chunk_k: usize,
    bulk: &[String],
) -> Result<Vec<(String, f64)>> {
    let all = core.index.ann_len(model);
    let to_rows = |cands: Vec<crate::index::AnnCandidate>| -> Vec<(String, f64)> {
        cands
            .into_iter()
            .map(|c| (ref_key(&c.ref_kind, &c.ref_id), c.score as f64))
            .collect()
    };
    match kinds {
        Some(ks) => {
            let want: HashSet<&str> = ks.iter().map(|k| k.as_str()).collect();
            let cands = core.index.ann_candidates(model, q, all)?;
            let mut rows = to_rows(cands);
            rows.retain(|(k, _)| {
                let (kind, _) = split_key(k);
                want.contains(kind)
            });
            rows.truncate(chunk_k);
            Ok(rows)
        }
        None => {
            let mut rows = to_rows(core.index.ann_candidates(model, q, chunk_k)?);
            if !bulk.is_empty() {
                let cands = core.index.ann_candidates(model, q, all)?;
                let mut non_bulk = to_rows(cands);
                non_bulk.retain(|(k, _)| {
                    let (kind, _) = split_key(k);
                    !bulk.iter().any(|b| b == kind)
                });
                non_bulk.truncate(chunk_k);
                rows.extend(non_bulk);
            }
            Ok(rows)
        }
    }
}

/// Brute-force candidate stage — full scan of model-matched rows, cosine in
/// Rust (bit-identical to the Postgres BYTEA path for the hash provider).
fn brute_candidate_rows(
    core: &Core,
    model: &str,
    q: &[f32],
    kinds: Option<&[EntityKind]>,
) -> Result<Vec<(String, f64)>> {
    let kind_clause = match kinds {
        Some(ks) => format!(" AND ref_kind IN ({})", placeholders_or_never(ks.len())),
        None => String::new(),
    };
    let sql = format!(
        "SELECT ref_kind, ref_id, vec FROM embeddings \
         WHERE model = ? AND dim = ? AND vec IS NOT NULL{kind_clause}"
    );
    let mut binds: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(model.to_string()), Box::new(q.len() as i64)];
    for k in kinds.unwrap_or(&[]) {
        binds.push(Box::new(k.as_str().to_string()));
    }
    let mut stmt = core.conn().prepare(&sql)?;
    let rows = stmt.query_map(
        rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref())),
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (kind, id, blob) = row?;
        let vec = hive_embed::from_blob(&blob);
        out.push((ref_key(&kind, &id), hive_embed::cosine(q, &vec)));
    }
    Ok(out)
}

/// Keyed hydration for ranked hits — one IN-query per kind present,
/// replacing the old query-time `embeddable_items()` corpus scan. A key
/// that doesn't resolve (orphaned embedding, deleted mail, unknown kind)
/// is simply absent from the map: callers DROP those hits rather than
/// surfacing raw-id titles.
fn hydrate_hits_core(core: &Core, keys: &[String]) -> Result<HashMap<String, HydratedHit>> {
    let conn = core.conn();
    let mut by_kind: HashMap<&str, Vec<&str>> = HashMap::new();
    for k in keys {
        let (kind, id) = split_key(k);
        if !id.is_empty() {
            by_kind.entry(kind).or_default().push(id);
        }
    }
    let mut out: HashMap<String, HydratedHit> = HashMap::new();
    let mut custom_ids: Vec<&str> = Vec::new();
    for (kind, ids) in by_kind {
        let sql_in = placeholders_or_never(ids.len());
        let binds = rusqlite::params_from_iter(ids.iter());
        match kind {
            "journal" => {
                let sql = format!("SELECT id, author, body FROM journal WHERE id IN ({sql_in})");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(binds, |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, author, body) = row?;
                    out.insert(
                        ref_key("journal", &id),
                        HydratedHit {
                            title: format!("{author}: {}", js_slice(&body, 40)),
                            text: body,
                            snippet: String::new(),
                        },
                    );
                }
            }
            "task" | "event" => {
                let table = if kind == "task" { "tasks" } else { "events" };
                let sql = format!("SELECT id, title, body FROM {table} WHERE id IN ({sql_in})");
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(binds, |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })?;
                for row in rows {
                    let (id, title, body) = row?;
                    out.insert(
                        ref_key(kind, &id),
                        HydratedHit {
                            text: format!("{title} {body}"),
                            title,
                            snippet: String::new(),
                        },
                    );
                }
            }
            "decision" => {
                let sql = format!(
                    "SELECT id, title, context, decision, consequences \
                     FROM decisions WHERE id IN ({sql_in})"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(binds, |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                    ))
                })?;
                for row in rows {
                    let (id, title, context, decision, consequences) = row?;
                    out.insert(
                        ref_key("decision", &id),
                        HydratedHit {
                            text: format!("{title} {context} {decision} {consequences}"),
                            title,
                            snippet: String::new(),
                        },
                    );
                }
            }
            "mail" => {
                // Tombstoned mail hydrates as a miss on purpose — the
                // embeddings row may outlive the message until the reaper
                // sweeps it.
                let sql = format!(
                    "SELECT id, subject, from_name, from_addr, snippet, body_text \
                     FROM mail_messages WHERE id IN ({sql_in}) AND deleted_at IS NULL"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(binds, |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                    ))
                })?;
                for row in rows {
                    let (id, subject, from_name, from_addr, snippet, body) = row?;
                    let from = from_name
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or(from_addr);
                    let title = if subject.trim().is_empty() {
                        from.clone()
                    } else {
                        subject.clone()
                    };
                    out.insert(
                        ref_key("mail", &id),
                        HydratedHit {
                            title,
                            text: format!("From: {from}\nSubject: {subject}\n\n{body}"),
                            snippet,
                        },
                    );
                }
            }
            _ => custom_ids.extend(ids),
        }
    }
    // Everything else may be a custom entity type slug — one batched
    // query keyed by the row's actual slug, so a kind/slug mismatch stays
    // a miss (fail closed, same as the old admit() contract).
    if !custom_ids.is_empty() {
        let sql = format!(
            "SELECT e.id, t.slug, e.title FROM entities e \
             JOIN entity_types t ON t.id = e.type_id WHERE e.id IN ({})",
            placeholders_or_never(custom_ids.len())
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(custom_ids.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (id, slug, title) = row?;
            out.insert(
                ref_key(&slug, &id),
                HydratedHit {
                    text: title.clone(),
                    title,
                    snippet: String::new(),
                },
            );
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_slice_counts_utf16_units() {
        assert_eq!(js_slice("hello", 40), "hello");
        assert_eq!(js_slice("abcdef", 3), "abc");
        // '𝄞' is one surrogate pair = 2 UTF-16 units.
        assert_eq!(js_slice("𝄞x", 3), "𝄞x");
    }

    #[test]
    fn split_key_on_first_colon() {
        assert_eq!(
            split_key("journal:jrnl_abc:def"),
            ("journal", "jrnl_abc:def")
        );
    }

    #[test]
    fn score_map_preserves_insertion_order() {
        let mut m = ScoreMap::default();
        m.entry("b:1").vector = 0.5;
        m.entry("a:2").vector = 0.5;
        m.entry("b:1").keyword = 0.9;
        assert_eq!(m.keys(), ["b:1".to_string(), "a:2".to_string()]);
        let keep: HashSet<String> = ["a:2".to_string()].into();
        m.retain_keys(&keep);
        assert_eq!(m.keys(), ["a:2".to_string()]);
    }

    #[test]
    fn collapse_chunks_takes_max_per_parent_and_sorts() {
        let rows = vec![
            ("journal:a".to_string(), 0.4),
            ("mail:m".to_string(), 0.7),
            ("journal:a".to_string(), 0.9),
            ("mail:m".to_string(), 0.7), // double-probe overlap duplicate
            ("journal:a".to_string(), 0.1),
        ];
        let out = collapse_chunks(rows);
        assert_eq!(
            out,
            vec![("journal:a".to_string(), 0.9), ("mail:m".to_string(), 0.7)]
        );
    }

    #[test]
    fn bulk_kinds_parse_json_and_csv() {
        assert_eq!(parse_bulk_kinds(r#"["mail","doc"]"#), ["mail", "doc"]);
        assert_eq!(parse_bulk_kinds("mail, doc"), ["mail", "doc"]);
        assert!(parse_bulk_kinds("[]").is_empty());
        assert!(parse_bulk_kinds("  ").is_empty());
    }

    #[test]
    fn kind_weights_merge_over_defaults() {
        let mut w = default_kind_weights();
        assert_eq!(w.get("mail"), Some(&0.85));
        merge_kind_weights(&mut w, r#"{"journal": 1.2, "mail": "bogus"}"#);
        assert_eq!(w.get("journal"), Some(&1.2));
        assert_eq!(w.get("mail"), Some(&0.85), "non-numeric entries ignored");
        merge_kind_weights(&mut w, "not json"); // must not panic or clear
        assert_eq!(w.get("journal"), Some(&1.2));
        assert_eq!(w.get("task"), None, "unlisted kinds default to 1.0 at use");
    }
}
