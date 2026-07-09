// Search query side — parity port of store.ts `search` (FTS5 keyword + viewer
// ACL), `semanticSearch` (the standard/precision hybrid cascade),
// `embeddableItems`, `embeddingStats`, and `visibleEntryIds`/`scopeHits`.
//
// Decoupling note: journal/tasks/decisions/events/shares/links data is read via
// private SQL here (not via those store modules) — the orchestrator dedups at
// integration.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use hive_shared::{EmbeddingKindCount, EmbeddingModelCount, EmbeddingStats, EntityKind, SearchHit};
use sqlx::Row;

use super::{placeholders_or_never, Store};

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
    /// Scope results to entries this viewer may see.
    pub viewer: Option<String>,
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
/// context prefix; `hash` stamps re-embeds.
pub struct EmbeddableItem {
    pub kind: String,
    pub id: String,
    pub title: String,
    pub text: String,
    pub embed_text: String,
    pub hash: String,
}

/// JS `String.prototype.slice(0, n)` — UTF-16 code units, not chars.
fn js_slice(s: &str, n: usize) -> String {
    let units: Vec<u16> = s.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
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

/// store.ts `toMatchQuery`, Postgres tsquery form: per-term strip
/// non-alphanumerics + lowercase, append `:*` (prefix match), drop empty/1-char
/// stems, join with ` & ` (AND). Feeds `to_tsquery('english', …)`.
fn to_match_query(q: &str) -> String {
    q.split_whitespace()
        .map(|term| {
            let stem: String = term
                .chars()
                .filter(|c| c.is_alphanumeric())
                .flat_map(|c| c.to_lowercase())
                .collect();
            format!("{stem}:*")
        })
        .filter(|t| t.encode_utf16().count() > 2) // drop empty stems (":*"), keep 1-char+
        .collect::<Vec<_>>()
        .join(" & ")
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

const ORIGIN_TABLE: &[(&str, &str)] = &[
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

/// A pure `kind:id → visible?` predicate for one viewer, resolved up front in
/// four queries (the ACL set plus one origin map per anchored table) so the
/// cascade can scope candidates without per-hit SQL.
struct VisibilityIndex {
    visible_entries: HashSet<String>,
    /// `kind:id` → origin_entry_id for task/decision/event rows.
    origin_of: HashMap<String, Option<String>>,
    /// `slug:id` keys of custom entity rows this viewer may see (scope lives
    /// on the row itself, not an origin entry).
    custom_visible: HashSet<String>,
    /// Mail ids owned by this viewer. Mail scopes by `user_scope` alone —
    /// owner-only, no share or mention piercing (DIRECTION.md D9).
    mail_visible: HashSet<String>,
}

impl VisibilityIndex {
    /// journal ids check the ACL set directly; task/decision/event go through
    /// their origin entry; mail through its owner set; anything else is
    /// invisible (fail closed).
    fn allows(&self, kind: &str, id: &str) -> bool {
        if kind == "journal" {
            return self.visible_entries.contains(id);
        }
        if kind == "mail" {
            return self.mail_visible.contains(id);
        }
        match self.origin_of.get(&ref_key(kind, id)) {
            Some(Some(origin)) => self.visible_entries.contains(origin),
            Some(None) => false,
            // Not an anchored built-in: visible only if it's a custom entity
            // row this viewer may see — unknown kinds stay invisible.
            None => self.custom_visible.contains(&ref_key(kind, id)),
        }
    }
}

impl Store {
    /// Resolved once per scoped semantic query. The cascade needs a pure
    /// predicate over EVERY vector candidate, so origin maps load their whole
    /// (small) tables up front — but only for kinds the query can return:
    /// `kinds` skips excluded tables (recall's journal-only path pays nothing
    /// here).
    async fn visibility_index(
        &self,
        viewer: &str,
        kinds: Option<&[EntityKind]>,
    ) -> Result<VisibilityIndex> {
        // `viewer` is always a concrete namespace user here (admins search
        // unscoped — the route passes viewer=None). The visible set is already
        // namespace-gated inside visible_entry_ids.
        let visible_entries = self
            .visible_entry_ids(&crate::middleware::Visibility::Namespace(
                viewer.to_string(),
            ))
            .await?
            .unwrap_or_default();
        let mut origin_of: HashMap<String, Option<String>> = HashMap::new();
        for (kind, table) in ORIGIN_TABLE {
            if kinds.is_some_and(|ks| !ks.iter().any(|k| k.as_str() == *kind)) {
                continue;
            }
            let rows = crate::pgq::query(&format!("SELECT id, origin_entry_id FROM {table}"))
                .fetch_all(self.db())
                .await?;
            for r in &rows {
                origin_of.insert(
                    ref_key(kind, r.try_get::<String, _>("id")?.as_str()),
                    r.try_get("origin_entry_id")?,
                );
            }
        }
        // Custom entity rows carry their scope directly. The typed `kinds`
        // filter can never name a custom slug, so any restriction excludes
        // them; only the unrestricted cascade loads this map (dormant until
        // custom kinds get embeddings).
        let mut custom_visible: HashSet<String> = HashSet::new();
        if kinds.is_none() {
            let rows = crate::pgq::query(
                "SELECT e.id, t.slug FROM entities e JOIN entity_types t ON t.id = e.type_id \
                 WHERE e.user_scope IS NULL OR e.user_scope = ?",
            )
            .bind(viewer)
            .fetch_all(self.db())
            .await?;
            for r in &rows {
                custom_visible.insert(ref_key(
                    r.try_get::<String, _>("slug")?.as_str(),
                    r.try_get::<String, _>("id")?.as_str(),
                ));
            }
        }
        // Mail scoping is a plain owner check. This full set stays small
        // until mail vectors exist (the D8 embed gate); the 1.5 retrieval
        // work moves the filter into the candidate SQL (embeddings.owner)
        // and this load becomes a safety net rather than the boundary.
        let mut mail_visible: HashSet<String> = HashSet::new();
        if kinds.is_none_or(|ks| ks.contains(&EntityKind::Mail)) {
            let rows = crate::pgq::query(
                "SELECT id FROM mail_messages WHERE user_scope = ? AND deleted_at IS NULL",
            )
            .bind(viewer)
            .fetch_all(self.db())
            .await?;
            for r in &rows {
                mail_visible.insert(r.try_get::<String, _>("id")?);
            }
        }
        Ok(VisibilityIndex {
            visible_entries,
            origin_of,
            custom_visible,
            mail_visible,
        })
    }

    /// Drop search hits a viewer can't see (store.ts `scopeHits`). Hits are
    /// already a bounded candidate list, so origins resolve in one batched
    /// IN query per kind present instead of full-table scans.
    async fn scope_hits(&self, hits: Vec<SearchHit>, viewer: &str) -> Result<Vec<SearchHit>> {
        let visible_entries = self
            .visible_entry_ids(&crate::middleware::Visibility::Namespace(
                viewer.to_string(),
            ))
            .await?
            .unwrap_or_default();
        let mut origin_of: HashMap<String, Option<String>> = HashMap::new();
        for (kind, table) in ORIGIN_TABLE {
            let ids: Vec<&str> = hits
                .iter()
                .filter(|h| h.kind.as_str() == *kind)
                .map(|h| h.id.as_str())
                .collect();
            if ids.is_empty() {
                continue;
            }
            let sql = format!(
                "SELECT id, origin_entry_id FROM {table} WHERE id IN ({})",
                placeholders_or_never(ids.len())
            );
            let mut q = crate::pgq::query(&sql);
            for id in &ids {
                q = q.bind(*id);
            }
            for r in &q.fetch_all(self.db()).await? {
                origin_of.insert(
                    ref_key(kind, r.try_get::<String, _>("id")?.as_str()),
                    r.try_get("origin_entry_id")?,
                );
            }
        }
        // Mail hits scope by owner, batched over the bounded candidate list —
        // never a full mail_messages load (200k rows post-backfill).
        let mut mail_visible: HashSet<String> = HashSet::new();
        let mail_ids: Vec<&str> = hits
            .iter()
            .filter(|h| h.kind == "mail")
            .map(|h| h.id.as_str())
            .collect();
        if !mail_ids.is_empty() {
            let sql = format!(
                "SELECT id FROM mail_messages WHERE id IN ({}) \
                 AND user_scope = ? AND deleted_at IS NULL",
                placeholders_or_never(mail_ids.len())
            );
            let mut q = crate::pgq::query(&sql);
            for id in &mail_ids {
                q = q.bind(*id);
            }
            q = q.bind(viewer);
            for r in &q.fetch_all(self.db()).await? {
                mail_visible.insert(r.try_get::<String, _>("id")?);
            }
        }
        // Hits whose kind is neither journal, mail, nor an anchored built-in
        // may be custom entities — resolve their visibility in one batched
        // query.
        let mut custom_visible: HashSet<String> = HashSet::new();
        let custom_ids: Vec<&str> = hits
            .iter()
            .filter(|h| h.kind != "journal" && h.kind != "mail" && origin_table(&h.kind).is_none())
            .map(|h| h.id.as_str())
            .collect();
        if !custom_ids.is_empty() {
            let sql = format!(
                "SELECT e.id, t.slug FROM entities e JOIN entity_types t ON t.id = e.type_id \
                 WHERE e.id IN ({}) AND (e.user_scope IS NULL OR e.user_scope = ?)",
                placeholders_or_never(custom_ids.len())
            );
            let mut q = crate::pgq::query(&sql);
            for id in &custom_ids {
                q = q.bind(*id);
            }
            q = q.bind(viewer);
            for r in &q.fetch_all(self.db()).await? {
                custom_visible.insert(ref_key(
                    r.try_get::<String, _>("slug")?.as_str(),
                    r.try_get::<String, _>("id")?.as_str(),
                ));
            }
        }
        let index = VisibilityIndex {
            visible_entries,
            origin_of,
            custom_visible,
            mail_visible,
        };
        Ok(hits
            .into_iter()
            .filter(|h| index.allows(h.kind.as_str(), &h.id))
            .collect())
    }

    /// FTS5 keyword search with optional viewer scoping (store.ts `search`).
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        viewer: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        let match_q = to_match_query(query);
        if match_q.is_empty() {
            return Ok(vec![]);
        }
        // Over-fetch when scoping so permission filtering doesn't starve the result.
        let fetch = if viewer.is_some() { limit * 5 } else { limit };
        // Postgres FTS: tsvector @@ tsquery, ts_rank for ranking (higher = better,
        // so DESC), ts_headline for the snippet. Replaces FTS5 MATCH/bm25/snippet.
        let rows = crate::pgq::query(
            "SELECT kind, ref_id, title, \
                    ts_headline('english', body, to_tsquery('english', ?), \
                      'StartSel=[, StopSel=], MaxFragments=2, MaxWords=14, MinWords=4, ShortWord=0') AS snip, \
                    ts_rank(tsv, to_tsquery('english', ?)) AS rank \
             FROM search WHERE tsv @@ to_tsquery('english', ?) ORDER BY rank DESC LIMIT ?",
        )
        .bind(&match_q)
        .bind(&match_q)
        .bind(&match_q)
        .bind(fetch as i64)
        .fetch_all(self.db())
        .await?;
        let hits: Vec<SearchHit> = rows
            .iter()
            .map(|r| -> Result<SearchHit> {
                // ts_rank is f32 and higher = better; clamp to a 0..1 score.
                let rank: f32 = r.try_get("rank")?;
                Ok(SearchHit {
                    kind: r.try_get("kind")?,
                    id: r.try_get("ref_id")?,
                    title: r.try_get("title")?,
                    snippet: r.try_get("snip")?,
                    score: ((rank.clamp(0.0, 1.0) as f64) * 1000.0).round() / 1000.0,
                })
            })
            .collect::<Result<_>>()?;
        let mut hits = match viewer {
            Some(v) => self.scope_hits(hits, v).await?,
            None => hits,
        };
        hits.truncate(limit);
        Ok(hits)
    }

    /// Every item worth embedding (store.ts `embeddableItems`). Public: the
    /// worker's backfill iterates this exactly like Node's worker does.
    pub async fn embeddable_items(&self) -> Result<Vec<EmbeddableItem>> {
        let mut out: Vec<EmbeddableItem> = Vec::new();
        let mut push = |kind: &str, id: String, title: String, text: String| {
            let embed_text = format!("[{kind}] {title}\n\n{text}");
            let hash = hive_embed::content_hash(&embed_text);
            out.push(EmbeddableItem {
                kind: kind.to_string(),
                id,
                title,
                text,
                embed_text,
                hash,
            });
        };

        let journal = crate::pgq::query(
            "SELECT id, author, body FROM journal ORDER BY created_at DESC LIMIT 1000",
        )
        .fetch_all(self.db())
        .await?;
        for r in &journal {
            let id: String = r.try_get("id")?;
            let author: String = r.try_get("author")?;
            let body: String = r.try_get("body")?;
            push(
                "journal",
                id,
                format!("{author}: {}", js_slice(&body, 40)),
                body,
            );
        }

        let tasks = crate::pgq::query(
            "SELECT id, title, body FROM tasks ORDER BY CASE priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, created_at DESC",
        )
        .fetch_all(self.db())
        .await?;
        for r in &tasks {
            let id: String = r.try_get("id")?;
            let title: String = r.try_get("title")?;
            let body: String = r.try_get("body")?;
            let text = format!("{title} {body}");
            push("task", id, title, text);
        }

        let decisions = crate::pgq::query(
            "SELECT id, title, context, decision, consequences FROM decisions ORDER BY created_at DESC",
        )
        .fetch_all(self.db())
        .await?;
        for r in &decisions {
            let id: String = r.try_get("id")?;
            let title: String = r.try_get("title")?;
            let context: String = r.try_get("context")?;
            let decision: String = r.try_get("decision")?;
            let consequences: String = r.try_get("consequences")?;
            let text = format!("{title} {context} {decision} {consequences}");
            push("decision", id, title, text);
        }

        let events = crate::pgq::query(
            "SELECT id, title, body FROM events ORDER BY COALESCE(at, created_at) DESC",
        )
        .fetch_all(self.db())
        .await?;
        for r in &events {
            let id: String = r.try_get("id")?;
            let title: String = r.try_get("title")?;
            let body: String = r.try_get("body")?;
            let text = format!("{title} {body}");
            push("event", id, title, text);
        }

        Ok(out)
    }

    /// Admin view of the embedding corpus (store.ts `embeddingStats`).
    pub async fn embedding_stats(&self) -> Result<EmbeddingStats> {
        let items = self.embeddable_items().await?;
        let stored_rows = crate::pgq::query("SELECT ref_kind, ref_id, hash FROM embeddings")
            .fetch_all(self.db())
            .await?;
        let mut stored: HashMap<String, String> = HashMap::new();
        for r in &stored_rows {
            stored.insert(
                ref_key(
                    r.try_get::<String, _>("ref_kind")?.as_str(),
                    r.try_get::<String, _>("ref_id")?.as_str(),
                ),
                r.try_get("hash")?,
            );
        }
        let pending = items
            .iter()
            .filter(|it| stored.get(&ref_key(&it.kind, &it.id)) != Some(&it.hash))
            .count();

        let total: i64 = crate::pgq::query_scalar("SELECT count(*) FROM embeddings")
            .fetch_one(self.db())
            .await?;
        let by_kind = crate::pgq::query(
            "SELECT ref_kind AS kind, count(*) AS count FROM embeddings GROUP BY ref_kind ORDER BY count DESC",
        )
        .fetch_all(self.db())
        .await?
        .iter()
        .map(|r| -> Result<EmbeddingKindCount> {
            Ok(EmbeddingKindCount {
                kind: r.try_get("kind")?,
                count: r.try_get("count")?,
            })
        })
        .collect::<Result<_>>()?;
        let by_model = crate::pgq::query(
            "SELECT model, dim, count(*) AS count FROM embeddings GROUP BY model, dim ORDER BY count DESC",
        )
        .fetch_all(self.db())
        .await?
        .iter()
        .map(|r| -> Result<EmbeddingModelCount> {
            Ok(EmbeddingModelCount {
                model: r.try_get("model")?,
                dim: r.try_get("dim")?,
                count: r.try_get("count")?,
            })
        })
        .collect::<Result<_>>()?;

        Ok(EmbeddingStats {
            total,
            model: hive_embed::embed_model().to_string(),
            embeddable: items.len() as i64,
            pending: pending as i64,
            by_kind,
            by_model,
        })
    }

    /// The actors associated with a hit (store.ts `hitActors`): journal →
    /// author + mentions; task/decision/event → assignees.
    async fn hit_actors(&self, kind: &str, ref_id: &str) -> Result<Vec<String>> {
        if kind == "journal" {
            let row = crate::pgq::query("SELECT author, mentions FROM journal WHERE id = ?")
                .bind(ref_id)
                .fetch_optional(self.db())
                .await?;
            let Some(r) = row else { return Ok(vec![]) };
            let mut actors = vec![r.try_get::<String, _>("author")?];
            actors.extend(super::json_vec(&r.try_get::<String, _>("mentions")?));
            return Ok(actors);
        }
        let Some(table) = origin_table(kind) else {
            return Ok(vec![]);
        };
        let assignees: Option<String> =
            crate::pgq::query_scalar(&format!("SELECT assignees FROM {table} WHERE id = ?"))
                .bind(ref_id)
                .fetch_optional(self.db())
                .await?;
        Ok(assignees.map(|a| super::json_vec(&a)).unwrap_or_default())
    }

    /// Neighbors of an entity in the links graph, either direction (store.ts
    /// `blanketNeighbors` — the Markov blanket).
    async fn blanket_neighbors(&self, kind: &str, id: &str) -> Result<Vec<String>> {
        let rows = crate::pgq::query(
            "SELECT target_kind AS k, target_id AS i FROM links WHERE source_kind = ? AND source_id = ? \
             UNION \
             SELECT source_kind AS k, source_id AS i FROM links WHERE target_kind = ? AND target_id = ?",
        )
        .bind(kind)
        .bind(id)
        .bind(kind)
        .bind(id)
        .fetch_all(self.db())
        .await?;
        rows.iter()
            .map(|r| -> Result<String> {
                Ok(ref_key(
                    r.try_get::<String, _>("k")?.as_str(),
                    r.try_get::<String, _>("i")?.as_str(),
                ))
            })
            .collect()
    }

    /// Semantic search — store.ts `semanticSearch`, the full standard|precision
    /// hybrid pipeline (vector pass → FTS blend → Markov-blanket boost →
    /// identity/peer soft boosts → optional cross-encoder rerank). Viewer ACL
    /// scoping applies BEFORE every pool cut, not as a post-filter, so hidden
    /// rows can't starve a viewer's results (DIRECTION.md Phase 0 item 2).
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
        let rerank_avail = tokio::task::spawn_blocking(hive_embed::rerank_available).await?;
        let use_rerank = (precision || opts.rerank.unwrap_or(false)) && rerank_avail;
        if query.trim().is_empty() {
            return Ok(vec![]);
        }

        // Resolve the viewer ACL once, up front. `admit` is pure after that:
        // a candidate enters any pool only if its kind parses (a newer
        // binary's rows must not hold slots this build drops at hydration),
        // passes the kinds filter, and is visible to the viewer. Applied
        // before every cut, including the fallback.
        let visible = match opts.viewer.as_deref() {
            Some(v) => Some(self.visibility_index(v, opts.kinds.as_deref()).await?),
            None => None,
        };
        let admit = |kind_s: &str, id: &str| -> bool {
            // The typed filter can only name built-ins, so when it's set it
            // excludes everything else (custom slugs included). Unrestricted
            // queries admit whatever the visibility index allows — which
            // handles custom entity rows by their own scope.
            if let Some(ks) = opts.kinds.as_ref() {
                match EntityKind::parse(kind_s) {
                    Some(kind) if ks.contains(&kind) => {}
                    _ => return false,
                }
            }
            match &visible {
                Some(ix) => ix.allows(kind_s, id),
                None => true,
            }
        };

        let items = self.embeddable_items().await?;
        let mut title_of: HashMap<String, String> = HashMap::new();
        let mut text_of: HashMap<String, String> = HashMap::new();
        for it in &items {
            let key = ref_key(&it.kind, &it.id);
            title_of.insert(key.clone(), it.title.clone());
            text_of.insert(key, it.text.clone());
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

        // 1. Vector pass — full cosine over model-matched blobs (model+dim
        // filter so a partial backfill never compares across dimensions). The
        // kind filter also lives in SQL so excluded kinds' vectors never load;
        // admit() stays the guarantee if this query ever changes.
        let owned_query = query.to_string();
        let q = tokio::task::spawn_blocking(move || hive_embed::embed_query(&owned_query)).await?;
        let kind_clause = match &opts.kinds {
            Some(ks) => format!(" AND ref_kind IN ({})", placeholders_or_never(ks.len())),
            None => String::new(),
        };
        let sql = format!(
            "SELECT ref_kind, ref_id, vec FROM embeddings WHERE model = ? AND dim = ?{kind_clause}"
        );
        let mut q_db = crate::pgq::query(&sql)
            .bind(hive_embed::embed_model())
            .bind(q.len() as i64);
        for k in opts.kinds.as_deref().unwrap_or(&[]) {
            q_db = q_db.bind(k.as_str());
        }
        let rows = q_db.fetch_all(self.db()).await?;
        let mut scored_all: Vec<(String, f64)> = rows
            .iter()
            .map(|r| -> Result<(String, f64)> {
                let key = ref_key(
                    r.try_get::<String, _>("ref_kind")?.as_str(),
                    r.try_get::<String, _>("ref_id")?.as_str(),
                );
                let vec = hive_embed::from_blob(&r.try_get::<Vec<u8>, _>("vec")?);
                Ok((key, hive_embed::cosine(&q, &vec)))
            })
            .collect::<Result<_>>()?;
        scored_all.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        // Admission applies before ANY cut — the threshold filter, the stage
        // pools, and the fallback refill all see an already-admitted list.
        scored_all.retain(|(k, _)| {
            let (kind, id) = split_key(k);
            admit(kind, id)
        });
        let passing: Vec<&(String, f64)> =
            scored_all.iter().filter(|(_, s)| *s >= threshold).collect();
        let raw_hit_keys: HashSet<String> = passing.iter().map(|(k, _)| k.clone()).collect();

        let mut scores = ScoreMap::default();
        for (k, s) in passing.iter().take(stage1_pool) {
            scores.entry(k).vector = *s;
        }

        // 2. Keyword pass (FTS) — rank-based score decaying from the top.
        if hybrid {
            // Over-fetch when a filter will thin the pool (mirrors search()'s
            // own viewer over-fetch), then admit BEFORE the stage cut, so
            // hidden or excluded-kind rows can't hold keyword slots.
            let fetch = if visible.is_some() || opts.kinds.is_some() {
                stage2_pool * 5
            } else {
                stage2_pool
            };
            let mut kw = self.search(query, fetch, None).await?;
            kw.retain(|r| admit(r.kind.as_str(), &r.id));
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
            for kk in scores.keys().to_vec() {
                let (k, id) = split_key(&kk);
                let mut strong = 0usize;
                let mut weak = 0usize;
                for nk in self.blanket_neighbors(k, id).await? {
                    if scored_keys.contains(&nk) {
                        strong += 1;
                    } else if raw_hit_keys.contains(&nk) {
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

        // 4. Blended sort, with the identity/peer soft boost (+0.1 — a nudge,
        // not a filter).
        let focus: HashSet<&str> = [opts.identity.as_deref(), opts.peer.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        let (w_vec, w_kw) = if precision { (0.55, 0.25) } else { (0.7, 0.2) };
        let pre_rerank_n = if precision && use_rerank {
            (limit * 2).max(limit)
        } else {
            limit
        };
        let mut ranked: Vec<(String, f64)> = Vec::with_capacity(scores.len());
        for kk in scores.keys().to_vec() {
            let s = *scores.get(&kk).unwrap();
            let scoped = if focus.is_empty() {
                0.0
            } else {
                let (k, id) = split_key(&kk);
                if self
                    .hit_actors(k, id)
                    .await?
                    .iter()
                    .any(|a| focus.contains(a.as_str()))
                {
                    0.1
                } else {
                    0.0
                }
            };
            let base = if s.keyword > 0.0 && s.vector > 0.0 {
                s.vector * w_vec + s.keyword * w_kw + s.blanket
            } else {
                s.vector + s.blanket
            };
            ranked.push((kk, base + scoped));
        }
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(pre_rerank_n);

        // 5. Cross-encoder rerank (precision stage 4, or the standard flag).
        if use_rerank && !ranked.is_empty() {
            let docs: Vec<String> = ranked
                .iter()
                .map(|(k, _)| text_of.get(k).cloned().unwrap_or_default())
                .collect();
            let owned_query = query.to_string();
            let rr = tokio::task::spawn_blocking(move || hive_embed::rerank(&owned_query, &docs))
                .await?;
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

        // 6. Fallback — never return empty when admitted vectors exist
        // (scored_all was admission-filtered above, so this can't smuggle
        // hidden or unknown-kind rows back in).
        if ranked.is_empty() && !scored_all.is_empty() {
            ranked = scored_all.iter().take(limit).cloned().collect();
        }

        let hits: Vec<SearchHit> = ranked
            .into_iter()
            .map(|(k, score)| {
                let (kind, id) = split_key(&k);
                SearchHit {
                    kind: kind.to_string(),
                    id: id.to_string(),
                    title: title_of.get(&k).cloned().unwrap_or_else(|| id.to_string()),
                    snippet: String::new(),
                    score: (score * 1000.0).round() / 1000.0,
                }
            })
            .collect();

        // Already viewer-scoped above (before every cut) — no post-filter.
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_query_strips_and_stars() {
        assert_eq!(to_match_query("hello world"), "hello:* & world:*");
        assert_eq!(to_match_query("c++ rocks!"), "c:* & rocks:*");
        assert_eq!(to_match_query("!!! ..."), "");
        // Single-char stems survive as `a:*` (prefix match, AND-joined).
        assert_eq!(to_match_query("a bee"), "a:* & bee:*");
    }

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
}
