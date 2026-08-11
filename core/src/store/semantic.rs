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

/// `hnsw.ef_search` for the ANN candidate probes. Default 80 — above
/// pgvector's 40 because the WHERE clauses post-filter the index stream and a
/// LIMIT can starve below k otherwise. Clamped to pgvector's accepted range.
fn vec_ef_search() -> u32 {
    std::env::var("HIVE_VEC_EF_SEARCH")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v >= 1)
        .unwrap_or(80)
        .min(1000)
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
        mail_ids: &HashSet<String>,
    ) -> Result<VisibilityIndex> {
        // `viewer` is always a concrete namespace user here (admins search
        // unscoped — the route passes viewer=None). The visible set is already
        // namespace-gated inside visible_entry_ids.
        let visible_entries = self
            .visible_entry_ids(&crate::Visibility::Namespace(viewer.to_string()))
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
        // Mail scoping is a plain owner check, resolved over the BOUNDED
        // candidate id set the caller collected (vector candidates + keyword
        // hits) — never a full mail_messages load (200k rows post-backfill).
        // The candidate SQL already pre-filters by embeddings.owner; this is
        // the authority check (user_scope + tombstones) on what survived.
        let mut mail_visible: HashSet<String> = HashSet::new();
        if kinds.is_none_or(|ks| ks.contains(&EntityKind::Mail)) && !mail_ids.is_empty() {
            let ids: Vec<&str> = mail_ids.iter().map(String::as_str).collect();
            let sql = format!(
                "SELECT id FROM mail_messages WHERE id IN ({}) \
                 AND user_scope = ? AND deleted_at IS NULL",
                placeholders_or_never(ids.len())
            );
            let mut q = crate::pgq::query(&sql);
            for id in &ids {
                q = q.bind(*id);
            }
            q = q.bind(viewer);
            for r in &q.fetch_all(self.db()).await? {
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
            .visible_entry_ids(&crate::Visibility::Namespace(viewer.to_string()))
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
        let mut push =
            |kind: &str, id: String, title: String, text: String, owner: Option<String>| {
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
        let journal = crate::pgq::query(
            "SELECT id, author, body, user_scope FROM journal ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(JOURNAL_EMBED_WINDOW)
        .fetch_all(self.db())
        .await?;
        for r in &journal {
            let id: String = r.try_get("id")?;
            let author: String = r.try_get("author")?;
            let body: String = r.try_get("body")?;
            let owner: Option<String> = r.try_get("user_scope")?;
            push(
                "journal",
                id,
                format!("{author}: {}", js_slice(&body, 40)),
                body,
                owner,
            );
        }

        // Anchored entities inherit their origin entry's scope — resolved in
        // the same query (one LEFT JOIN per table, no per-item lookups).
        // Rows without an origin entry (or with a global one) stay global.
        let tasks = crate::pgq::query(
            "SELECT t.id, t.title, t.body, j.user_scope AS owner FROM tasks t \
             LEFT JOIN journal j ON j.id = t.origin_entry_id \
             ORDER BY CASE t.priority WHEN 'urgent' THEN 0 WHEN 'high' THEN 1 WHEN 'normal' THEN 2 ELSE 3 END, t.created_at DESC",
        )
        .fetch_all(self.db())
        .await?;
        for r in &tasks {
            let id: String = r.try_get("id")?;
            let title: String = r.try_get("title")?;
            let body: String = r.try_get("body")?;
            let owner: Option<String> = r.try_get("owner")?;
            let text = format!("{title} {body}");
            push("task", id, title, text, owner);
        }

        let decisions = crate::pgq::query(
            "SELECT d.id, d.title, d.context, d.decision, d.consequences, j.user_scope AS owner \
             FROM decisions d LEFT JOIN journal j ON j.id = d.origin_entry_id \
             ORDER BY d.created_at DESC",
        )
        .fetch_all(self.db())
        .await?;
        for r in &decisions {
            let id: String = r.try_get("id")?;
            let title: String = r.try_get("title")?;
            let context: String = r.try_get("context")?;
            let decision: String = r.try_get("decision")?;
            let consequences: String = r.try_get("consequences")?;
            let owner: Option<String> = r.try_get("owner")?;
            let text = format!("{title} {context} {decision} {consequences}");
            push("decision", id, title, text, owner);
        }

        let events = crate::pgq::query(
            "SELECT e.id, e.title, e.body, j.user_scope AS owner FROM events e \
             LEFT JOIN journal j ON j.id = e.origin_entry_id \
             ORDER BY COALESCE(e.at, e.created_at) DESC",
        )
        .fetch_all(self.db())
        .await?;
        for r in &events {
            let id: String = r.try_get("id")?;
            let title: String = r.try_get("title")?;
            let body: String = r.try_get("body")?;
            let owner: Option<String> = r.try_get("owner")?;
            let text = format!("{title} {body}");
            push("event", id, title, text, owner);
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

    /// ANN candidate stage (plan B5): HNSW index probes over the native
    /// `vec_v` column instead of loading the whole BYTEA table. Returns
    /// chunk-level `(kind:id, sim)` rows, NOT yet collapsed.
    ///
    /// Owner filtering runs in SQL (`embeddings.owner`) so a 200k-row foreign
    /// mailbox never occupies candidate slots; `admit()` stays the authority
    /// on top. When `kinds` is None a second probe excludes the bulk kinds so
    /// mail volume can never evict journal/tasks from the candidate set —
    /// union'd and deduped by the caller's collapse.
    ///
    /// Raw sqlx (not pgq): the query vector binds via the pgvector crate and
    /// `$1` is reused in SELECT + ORDER BY, which the `?` rewriter can't
    /// express.
    async fn ann_candidates(
        &self,
        q: &[f32],
        viewer: Option<&str>,
        kinds: Option<&[EntityKind]>,
        chunk_k: i64,
        bulk: &[String],
    ) -> Result<Vec<(String, f64)>> {
        let qv = pgvector::Vector::from(q.to_vec());
        let model = hive_embed::embed_model();
        // ef_search is SET LOCAL (transaction-scoped) — never leaks into the
        // pooled connection. Formatted, not bound: SET takes no parameters;
        // the value is a parsed+clamped integer.
        let ef = vec_ef_search();
        let mut tx = self.db().begin().await?;
        sqlx::query(&format!("SET LOCAL hnsw.ef_search = {ef}"))
            .execute(&mut *tx)
            .await?;

        const BASE: &str = "SELECT ref_kind, ref_id, chunk_idx, 1 - (vec_v <=> $1) AS sim \
             FROM embeddings \
             WHERE model = $2 AND vec_v IS NOT NULL \
               AND ($3::text IS NULL OR owner IS NULL OR owner = $3)";
        let mut rows: Vec<(String, f64)> = Vec::new();
        let mut push_rows = |fetched: Vec<sqlx::postgres::PgRow>| -> Result<()> {
            for r in &fetched {
                let key = ref_key(
                    r.try_get::<String, _>("ref_kind")?.as_str(),
                    r.try_get::<String, _>("ref_id")?.as_str(),
                );
                rows.push((key, r.try_get::<f64, _>("sim")?));
            }
            Ok(())
        };

        match kinds {
            Some(ks) => {
                let arr: Vec<String> = ks.iter().map(|k| k.as_str().to_string()).collect();
                let sql = format!("{BASE} AND ref_kind = ANY($4) ORDER BY vec_v <=> $1 LIMIT $5");
                let fetched = sqlx::query(&sql)
                    .bind(&qv)
                    .bind(model)
                    .bind(viewer)
                    .bind(&arr)
                    .bind(chunk_k)
                    .fetch_all(&mut *tx)
                    .await?;
                push_rows(fetched)?;
            }
            None => {
                let sql = format!("{BASE} ORDER BY vec_v <=> $1 LIMIT $4");
                let fetched = sqlx::query(&sql)
                    .bind(&qv)
                    .bind(model)
                    .bind(viewer)
                    .bind(chunk_k)
                    .fetch_all(&mut *tx)
                    .await?;
                push_rows(fetched)?;
                if !bulk.is_empty() {
                    let sql =
                        format!("{BASE} AND ref_kind <> ALL($4) ORDER BY vec_v <=> $1 LIMIT $5");
                    let fetched = sqlx::query(&sql)
                        .bind(&qv)
                        .bind(model)
                        .bind(viewer)
                        .bind(bulk)
                        .bind(chunk_k)
                        .fetch_all(&mut *tx)
                        .await?;
                    push_rows(fetched)?;
                }
            }
        }
        tx.commit().await?;
        Ok(rows)
    }

    /// Brute-force BYTEA candidate stage — the pre-pgvector path, kept for
    /// non-384-dim providers (the 256-dim hash embedder in dev/CI) so the
    /// whole cascade stays exercised without ONNX. Owner-filtered in SQL like
    /// the ANN path; full scan of model-matched rows, cosine in Rust. Returns
    /// chunk-level rows for the same collapse.
    async fn brute_candidates(
        &self,
        q: &[f32],
        viewer: Option<&str>,
        kinds: Option<&[EntityKind]>,
    ) -> Result<Vec<(String, f64)>> {
        let kind_clause = match kinds {
            Some(ks) => format!(" AND ref_kind IN ({})", placeholders_or_never(ks.len())),
            None => String::new(),
        };
        let owner_clause = if viewer.is_some() {
            " AND (owner IS NULL OR owner = ?)"
        } else {
            ""
        };
        let sql = format!(
            "SELECT ref_kind, ref_id, vec FROM embeddings \
             WHERE model = ? AND dim = ? AND vec IS NOT NULL{kind_clause}{owner_clause}"
        );
        let mut q_db = crate::pgq::query(&sql)
            .bind(hive_embed::embed_model())
            .bind(q.len() as i64);
        for k in kinds.unwrap_or(&[]) {
            q_db = q_db.bind(k.as_str());
        }
        if let Some(v) = viewer {
            q_db = q_db.bind(v);
        }
        let rows = q_db.fetch_all(self.db()).await?;
        rows.iter()
            .map(|r| -> Result<(String, f64)> {
                let key = ref_key(
                    r.try_get::<String, _>("ref_kind")?.as_str(),
                    r.try_get::<String, _>("ref_id")?.as_str(),
                );
                let vec = hive_embed::from_blob(&r.try_get::<Vec<u8>, _>("vec")?);
                Ok((key, hive_embed::cosine(q, &vec)))
            })
            .collect()
    }

    /// Keyed hydration for ranked hits — one IN-query per kind present,
    /// replacing the old query-time `embeddable_items()` corpus scan. A key
    /// that doesn't resolve (orphaned embedding, deleted mail, unknown kind)
    /// is simply absent from the map: callers DROP those hits rather than
    /// surfacing raw-id titles.
    async fn hydrate_hits(&self, keys: &[String]) -> Result<HashMap<String, HydratedHit>> {
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
            match kind {
                "journal" => {
                    let sql =
                        format!("SELECT id, author, body FROM journal WHERE id IN ({sql_in})");
                    let mut q = crate::pgq::query(&sql);
                    for id in &ids {
                        q = q.bind(*id);
                    }
                    for r in &q.fetch_all(self.db()).await? {
                        let id: String = r.try_get("id")?;
                        let author: String = r.try_get("author")?;
                        let body: String = r.try_get("body")?;
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
                    let mut q = crate::pgq::query(&sql);
                    for id in &ids {
                        q = q.bind(*id);
                    }
                    for r in &q.fetch_all(self.db()).await? {
                        let id: String = r.try_get("id")?;
                        let title: String = r.try_get("title")?;
                        let body: String = r.try_get("body")?;
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
                    let mut q = crate::pgq::query(&sql);
                    for id in &ids {
                        q = q.bind(*id);
                    }
                    for r in &q.fetch_all(self.db()).await? {
                        let id: String = r.try_get("id")?;
                        let title: String = r.try_get("title")?;
                        let context: String = r.try_get("context")?;
                        let decision: String = r.try_get("decision")?;
                        let consequences: String = r.try_get("consequences")?;
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
                    let mut q = crate::pgq::query(&sql);
                    for id in &ids {
                        q = q.bind(*id);
                    }
                    for r in &q.fetch_all(self.db()).await? {
                        let id: String = r.try_get("id")?;
                        let subject: String = r.try_get("subject")?;
                        let from_name: Option<String> = r.try_get("from_name")?;
                        let from_addr: String = r.try_get("from_addr")?;
                        let snippet: String = r.try_get("snippet")?;
                        let body: String = r.try_get("body_text")?;
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
            let mut q = crate::pgq::query(&sql);
            for id in &custom_ids {
                q = q.bind(*id);
            }
            for r in &q.fetch_all(self.db()).await? {
                let id: String = r.try_get("id")?;
                let slug: String = r.try_get("slug")?;
                let title: String = r.try_get("title")?;
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
        // the HNSW ANN probes over vec_v; anything else (the 256-dim hash
        // provider in dev/CI) keeps the brute-force BYTEA scan so the whole
        // cascade below stays exercised without ONNX. The q.len() guard
        // covers the transformers latch: a mid-flight model failure degrades
        // embed_query to 256-dim hash vectors, which must not bind against
        // vector(384). Both paths owner-filter in SQL; admit() stays the
        // authority on top.
        let owned_query = query.to_string();
        let q = tokio::task::spawn_blocking(move || hive_embed::embed_query(&owned_query)).await?;
        let chunk_k = (stage1_pool * 4).max(limit) as i64;
        let bulk = bulk_kinds();
        let raw_rows = if hive_embed::embed_dim() == 384 && q.len() == 384 {
            self.ann_candidates(
                &q,
                opts.viewer.as_deref(),
                opts.kinds.as_deref(),
                chunk_k,
                &bulk,
            )
            .await?
        } else {
            self.brute_candidates(&q, opts.viewer.as_deref(), opts.kinds.as_deref())
                .await?
        };
        // Chunked rows share one `kind:id` key — collapse to the item's best
        // chunk (MAX sim) so pool slots, the fallback, and the final hit list
        // stay one-entry-per-item. Everything downstream works on parent keys.
        let mut scored_all = collapse_chunks(raw_rows);

        // Keyword rows are fetched BEFORE the visibility index so its mail
        // arm can batch-resolve every candidate mail id in one bounded query.
        // Blend/scoring order below is unchanged.
        let kw_fetch = if opts.viewer.is_some() || opts.kinds.is_some() {
            stage2_pool * 5
        } else {
            stage2_pool
        };
        let kw_all: Vec<SearchHit> = if hybrid {
            self.search(query, kw_fetch, None).await?
        } else {
            Vec::new()
        };

        // Resolve the viewer ACL once, up front. `admit` is pure after that:
        // a candidate enters any pool only if its kind parses (a newer
        // binary's rows must not hold slots this build drops at hydration),
        // passes the kinds filter, and is visible to the viewer. Applied
        // before every cut, including the fallback.
        let visible = match opts.viewer.as_deref() {
            Some(v) => {
                let mut mail_ids: HashSet<String> = HashSet::new();
                for (k, _) in &scored_all {
                    let (kind, id) = split_key(k);
                    if kind == "mail" {
                        mail_ids.insert(id.to_string());
                    }
                }
                for h in &kw_all {
                    if h.kind == "mail" {
                        mail_ids.insert(h.id.clone());
                    }
                }
                Some(
                    self.visibility_index(v, opts.kinds.as_deref(), &mail_ids)
                        .await?,
                )
            }
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

        // Admission applies before ANY cut — the threshold filter, the stage
        // pools, and the fallback refill all see an already-admitted list.
        scored_all.retain(|(k, _)| {
            let (kind, id) = split_key(k);
            admit(kind, id)
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
            // Over-fetched above when a filter will thin the pool (mirrors
            // search()'s own viewer over-fetch); admit BEFORE the stage cut,
            // so hidden or excluded-kind rows can't hold keyword slots.
            let mut kw = kw_all;
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

        // 4. Blended sort, with the per-kind rank weight (multiplicative,
        // applied exactly once) and then the identity/peer soft boost (+0.1 —
        // a nudge, not a filter).
        let weights = self.kind_weights().await;
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
            let (kind, id) = split_key(&kk);
            let scoped = if focus.is_empty() {
                0.0
            } else if self
                .hit_actors(kind, id)
                .await?
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
        // hidden or unknown-kind rows back in). Runs before hydration so
        // fallback keys hydrate too.
        if ranked.is_empty() && !scored_all.is_empty() {
            ranked = scored_all.iter().take(limit).cloned().collect();
        }

        // 6. Keyed hydration over the ranked pool (bounded — never the old
        // corpus scan). A key that doesn't hydrate is DROPPED before the
        // pre-rerank cut, so an orphaned embedding can't hold a result slot
        // OR starve the survivors out of theirs.
        let keys: Vec<String> = ranked.iter().map(|(k, _)| k.clone()).collect();
        let hydrated = self.hydrate_hits(&keys).await?;
        ranked.retain(|(k, _)| hydrated.contains_key(k));
        ranked.truncate(pre_rerank_n);

        // 7. Cross-encoder rerank (precision stage 4, or the standard flag).
        if use_rerank && !ranked.is_empty() {
            let docs: Vec<String> = ranked
                .iter()
                .map(|(k, _)| hydrated.get(k).map(|h| h.text.clone()).unwrap_or_default())
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
