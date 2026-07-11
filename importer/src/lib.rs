// hive-import — the one-shot hosted-Postgres → hive-data-dir migration
// (PLAN.md PR 1.7). This crate is the ONE remaining Postgres reader in the
// workspace; sqlx is declared here and nowhere else (the grep gate in
// tests/no_postgres_gate.rs keeps it that way).
//
// Shape of the run:
//   1. preflight — refuse a data dir that already holds a store (one-shot),
//      then refuse a database that doesn't look like a hosted hive;
//   2. connect + count every source table into the Plan (--dry-run stops
//      here, having written nothing);
//   3. open the store on a pinned synthetic device id, then replay the old
//      instance as op-log records IN DEPENDENCY ORDER: actors, entity types,
//      entities, journal+anchors, links, profile/config-tier rows, unread
//      inbox, mail (accounts → cursors → mailboxes → messages → attachments);
//   4. attachment bytes stream through the blockstore via the runtime path
//      mail.rs uses (blob_refs + a module.doc), round-trip-verified;
//   5. mail FTS membership is stamped the way the ingest path stamps it
//      (command-layer policy — the fold owns every other kind's FTS).
//
// Contract with the records (PLAN.md): original nanoid ids ARE the new ids,
// original timestamps ride verbatim (payload fields untouched; the record
// envelope ts is shape-normalized only), every payload carries
// `origin: {source: "hosted-v0.6", table}` (the fold v3 amendment), and
// `alias` records exist ONLY for re-keyed blob hashes. Embeddings are never
// computed here — imported rows enter the store with no vectors and the
// app's background backfill (Store::backfill_embeddings) fills them in.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use hive_core::keys::MemoryKeySource;
use hive_core::store::import::ImportRecord;
use hive_core::store::mail::{fts_clip, FTS_CLIP_BYTES};
use hive_core::store::Store;
use hive_embed::HashEmbedder;
use serde::Serialize;
use serde_json::{json, Value as Json};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

/// Provenance stamped into every record payload (the fold v3 `origin` key).
pub const ORIGIN_SOURCE: &str = "hosted-v0.6";

/// The pinned synthetic device id every import writes under. Pinning it (via
/// the data dir's `device` file, laid down before the store opens) makes two
/// imports of the same source fold to byte-identical derived state — the
/// determinism oracle — and keeps imported history recognizable once
/// multi-device sync exists.
pub const IMPORT_DEVICE: &str = "import-hosted-v0.6";

/// Record author when the source row names none.
const IMPORTER_ACTOR: &str = "importer";

/// Records per import_batch call (each batch = one fold transaction; chunking
/// bounds memory on large mail archives without changing record order).
const BATCH: usize = 500;

pub struct Opts {
    /// Postgres URL of the old hosted instance (--from).
    pub from: String,
    /// Target data dir (--data-dir, or the app/bridge default resolution).
    pub data_dir: PathBuf,
    /// Count and plan only; write nothing.
    pub dry_run: bool,
    /// Master key bytes. None is only legal for --dry-run: real runs resolve
    /// the OS keychain (or HIVE_IMPORT_KEY_HEX) in main BEFORE the tokio
    /// runtime exists — keyring's sync Secret Service backend panics on a
    /// tokio thread (same ordering the app and bridge use).
    pub master_key: Option<[u8; 32]>,
}

/// The dry-run product: per-table SOURCE row counts, in import dependency
/// order. Serializable so non-CLI callers (the app's onboarding today) can
/// carry and render it; `grouped()` is the coarse human rollup they show.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub tables: Vec<(&'static str, i64)>,
}

impl Plan {
    /// Rows counted for one source table (0 when the plan doesn't cover it).
    pub fn rows(&self, table: &str) -> i64 {
        self.tables
            .iter()
            .find(|(t, _)| *t == table)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    /// Every source row the import would read.
    pub fn total_rows(&self) -> i64 {
        self.tables.iter().map(|(_, n)| n).sum()
    }

    /// The coarse rollup a plan card renders: human labels over table
    /// groups. Covers every PLAN_TABLES member exactly once, so the group
    /// sum equals total_rows() — the grouped_covers_every_plan_table unit
    /// test holds this mapping honest when tables are added.
    pub fn grouped(&self) -> Vec<(&'static str, i64)> {
        const GROUPS: &[(&str, &[&str])] = &[
            ("journal entries", &["journal"]),
            ("anchors", &["anchors"]),
            (
                "entities",
                &[
                    "people",
                    "entity_types",
                    "entity_fields",
                    "projects",
                    "topics",
                    "phases",
                    "tasks",
                    "decisions",
                    "events",
                    "entities",
                    "profile",
                    "identity_artifacts",
                    "identities",
                    "sources",
                ],
            ),
            ("links", &["links"]),
            ("inbox items", &["inbox"]),
            ("config entries", &["config"]),
            ("mail accounts", &["mail_accounts"]),
            ("mail folders", &["mail_mailboxes"]),
            ("mail messages", &["mail_messages"]),
            ("attachments", &["mail_attachments"]),
        ];
        GROUPS
            .iter()
            .map(|(label, tables)| (*label, tables.iter().map(|t| self.rows(t)).sum()))
            .collect()
    }
}

/// What one completed import wrote. `plan` carries the SOURCE row counts
/// (the same counts a dry run reports); the rest count what landed.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub plan: Plan,
    /// Op-log records committed.
    pub records: u64,
    /// Attachment blobs stored in the blockstore (round-trip verified).
    pub blobs_stored: u64,
    /// Blobs whose legacy hash was not the blake3 of the bytes → alias record.
    pub blobs_rekeyed: u64,
    /// Attachment metadata imported without local bytes (Phase 3 refetches).
    pub attachments_pending: u64,
    /// Soft-deleted mail messages left behind (deleted mail stays deleted).
    pub mail_deleted_skipped: u64,
    /// Already-read inbox rows left behind (only unread state migrates).
    pub inbox_read_skipped: u64,
    /// Mail messages given an FTS row (ingest-mailbox ∩ not-junk parity).
    pub mail_fts_rows: u64,
}

/// What `run` produced — data only; the CLI formats it for stdout and the
/// app's onboarding renders it as cards.
#[derive(Debug, Clone, Serialize)]
pub enum RunOutcome {
    /// Dry run: the counted plan. Nothing was written; no key was needed.
    Plan(Plan),
    /// A real import ran to completion, and its store is fully shut down
    /// (data-dir flock released) — the same process may Store::new the data
    /// dir immediately, which is exactly what the app's onboarding does.
    Imported(Summary),
}

impl RunOutcome {
    /// The source counts, whichever arm this is.
    pub fn plan(&self) -> &Plan {
        match self {
            RunOutcome::Plan(plan) => plan,
            RunOutcome::Imported(summary) => &summary.plan,
        }
    }
}

/// Run the import per `opts`. On a partial failure after the store opened,
/// the error tells the user to remove the data dir and re-run — one-shot
/// means no resume. Errors are anyhow chains meant for eyes: connection
/// refusals, the not-a-hive-database preflight, and stage failures all
/// surface verbatim in the CLI and the onboarding UI.
pub async fn run(opts: &Opts) -> Result<RunOutcome> {
    preflight_data_dir(&opts.data_dir)?;

    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&opts.from)
        .await
        .with_context(|| format!("connecting to {}", redact_url(&opts.from)))?;
    preflight_source(&pool).await?;

    let plan = Plan {
        tables: count_plan(&pool).await?,
    };
    tracing::info!(
        source = %redact_url(&opts.from),
        rows = plan.total_rows(),
        "counted the source plan"
    );
    if opts.dry_run {
        return Ok(RunOutcome::Plan(plan));
    }

    let master = opts
        .master_key
        .ok_or_else(|| anyhow!("no master key resolved (importer bug: main resolves it)"))?;

    // Lay the pinned device id down before the store opens, so the op log is
    // written under IMPORT_DEVICE instead of a freshly minted id. The file
    // name is the store's frozen data-dir layout (core/src/store/core.rs);
    // preflight just proved it absent.
    std::fs::create_dir_all(&opts.data_dir)
        .with_context(|| format!("creating data dir {}", opts.data_dir.display()))?;
    std::fs::write(opts.data_dir.join("device"), format!("{IMPORT_DEVICE}\n"))
        .context("writing the import device id")?;

    let store = Store::new(
        &opts.data_dir,
        Arc::new(MemoryKeySource(master)),
        Arc::new(HashEmbedder),
    )
    .with_context(|| format!("opening hive store at {}", opts.data_dir.display()))?;

    let mut summary = Summary {
        plan,
        ..Summary::default()
    };
    // Shut the writer thread down — releasing the data-dir flock — whether
    // the stages succeeded or not, THEN surface the stage error. The caller
    // stays in-process either way: on success it reopens this dir at once,
    // on failure it may clean the dir up and retry.
    let outcome = import_all(&pool, &store, &mut summary).await;
    store.shutdown().await?;
    outcome.with_context(|| {
        format!(
            "import failed part-way; the data dir at {} is incomplete — remove it and re-run",
            opts.data_dir.display()
        )
    })?;

    tracing::info!(records = summary.records, "import complete");
    Ok(RunOutcome::Imported(summary))
}

/// True when `dir` already holds a hive store: a `device` file, or op-log
/// segments under `log/`. This is THE fresh-dir rule — the importer's
/// one-shot refusal and the app's first-launch probe (fresh dir → onboarding,
/// no store opened) must agree, so they share it.
pub fn data_dir_holds_store(dir: &Path) -> bool {
    let log = dir.join("log");
    dir.join("device").exists()
        || (log.is_dir()
            && std::fs::read_dir(&log)
                .map(|mut d| d.next().is_some())
                .unwrap_or(false))
}

/// One-shot target check: a `device` file or op-log segments mean a store
/// already lives here. Runs for --dry-run too — it is the truthful preflight
/// for the real run.
pub fn preflight_data_dir(dir: &Path) -> Result<()> {
    if data_dir_holds_store(dir) {
        bail!(
            "data dir {} already holds a hive store ({}). hive-import is one-shot: \
             move or remove the existing store, or pass --data-dir to import elsewhere",
            dir.display(),
            if dir.join("device").exists() {
                "found a device file"
            } else {
                "found op-log segments under log/"
            }
        );
    }
    Ok(())
}

/// "Is this actually a hosted hive database?" — probe for every source table
/// before the first count, so a mistyped URL that reaches SOME Postgres
/// fails with a plain answer instead of a bare SQL error. to_regclass
/// resolves through search_path, matching how every later read finds tables.
async fn preflight_source(pool: &PgPool) -> Result<()> {
    let tables: Vec<String> = PLAN_TABLES.iter().map(|t| t.to_string()).collect();
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT t FROM unnest($1::text[]) AS t WHERE to_regclass(t) IS NULL ORDER BY t",
    )
    .bind(&tables)
    .fetch_all(pool)
    .await
    .context("probing for the hosted hive tables")?;
    if let Some(first) = missing.first() {
        bail!(
            "this doesn't look like a hosted hive database (missing table {first}{}) — \
             the URL must point at the legacy instance's own Postgres",
            match missing.len() {
                1 => String::new(),
                n => format!(" and {} more", n - 1),
            }
        );
    }
    Ok(())
}

/// Postgres URLs carry credentials; log/print only scheme + host + db.
pub fn redact_url(url: &str) -> String {
    match url.split_once('@') {
        Some((head, tail)) => {
            let scheme = head.split("://").next().unwrap_or("postgres");
            format!("{scheme}://…@{tail}")
        }
        None => url.to_string(),
    }
}

// ── plan ─────────────────────────────────────────────────────────────────────

/// Source tables the import reads, in dependency order. NOT read, per
/// PLAN.md: search/embeddings (derived), wire/outbox (transient queues),
/// users/sessions/tokens/oauth/shares/cc_credentials (hosted-era auth + the
/// old credential vault — mail credentials are re-entered against the OS
/// keychain in Phase 3), and conversation transcripts.
const PLAN_TABLES: &[&str] = &[
    "people",
    "entity_types",
    "entity_fields",
    "projects",
    "topics",
    "phases",
    "tasks",
    "decisions",
    "events",
    "entities",
    "journal",
    "anchors",
    "links",
    "profile",
    "identity_artifacts",
    "identities",
    "sources",
    "config",
    "inbox",
    "mail_accounts",
    "mail_mailboxes",
    "mail_messages",
    "mail_attachments",
];

async fn count_plan(pool: &PgPool) -> Result<Vec<(&'static str, i64)>> {
    let mut out = Vec::with_capacity(PLAN_TABLES.len());
    for table in PLAN_TABLES {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(pool)
            .await
            .with_context(|| format!("counting {table} (is this a hosted hive database?)"))?;
        out.push((*table, n));
    }
    Ok(out)
}

// ── record assembly ──────────────────────────────────────────────────────────

/// Buffered writer: keeps record order while committing in bounded batches.
struct Batcher<'a> {
    store: &'a Store,
    buf: Vec<ImportRecord>,
    written: u64,
}

impl<'a> Batcher<'a> {
    fn new(store: &'a Store) -> Self {
        Batcher {
            store,
            buf: Vec::with_capacity(BATCH),
            written: 0,
        }
    }

    async fn push(&mut self, kind: &str, actor: &str, ts: String, payload: Json) -> Result<()> {
        self.buf.push(ImportRecord {
            kind: kind.to_string(),
            actor: actor.to_string(),
            ts,
            payload,
        });
        if self.buf.len() >= BATCH {
            self.flush().await?;
        }
        Ok(())
    }

    async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let batch = std::mem::replace(&mut self.buf, Vec::with_capacity(BATCH));
        self.written += batch.len() as u64;
        self.store.import_batch(batch).await
    }
}

fn origin(table: &str) -> Json {
    json!({"source": ORIGIN_SOURCE, "table": table})
}

/// Normalize a source timestamp into the frozen 24-char envelope shape.
/// Hosted-era rows were written by JS toISOString / the store's now_iso and
/// already match; anything else parses as RFC 3339 and reformats. Payload
/// fields are NEVER normalized — this is for the record envelope only.
fn norm_ts(raw: &str, what: &str) -> Result<String> {
    if hive_core::oplog::ts_shape_ok(raw) {
        return Ok(raw.to_string());
    }
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Ok(parsed
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string());
    }
    // Pre-Postgres rows: the app's SQLite era wrote datetime('now') literals
    // ("2026-05-11 13:47:34", naive, UTC by SQLite semantics) that the
    // original Postgres migration carried over verbatim. Read them as UTC.
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, fmt) {
            return Ok(naive.and_utc().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string());
        }
    }
    anyhow::bail!(
        "{what}: timestamp {raw:?} is neither the frozen ISO shape, RFC 3339, \
         nor a naive SQLite-era datetime"
    )
}

/// TEXT column holding a JSON string array → real array for the
/// journal.append payload (the fold requires arrays there). Tolerates legacy
/// garbage the way the store's own json_vec does.
fn json_arr(raw: &str) -> Vec<String> {
    serde_json::from_str(raw).unwrap_or_default()
}

fn opt(v: Option<String>) -> Json {
    v.map(Json::String).unwrap_or(Json::Null)
}

// ── the stages ───────────────────────────────────────────────────────────────

async fn import_all(pool: &PgPool, store: &Store, summary: &mut Summary) -> Result<()> {
    let mut b = Batcher::new(store);
    stage_people(pool, &mut b).await?;
    stage_entity_types(pool, &mut b).await?;
    stage_builtin_entities(pool, &mut b).await?;
    stage_custom_entities(pool, &mut b).await?;
    stage_journal(pool, &mut b).await?;
    stage_links(pool, &mut b).await?;
    stage_profile_tier(pool, &mut b).await?;
    stage_inbox(pool, &mut b, summary).await?;
    let mail = stage_mail(pool, &mut b, summary).await?;
    b.flush().await?;
    summary.records += b.written;

    // Attachment bytes ride the runtime path (blockstore + blob_refs + one
    // module.doc record each), never import_batch; alias records for
    // re-keyed hashes land as one final record batch.
    let aliases = store_attachment_blobs(store, mail.blobs, summary).await?;
    summary.records += summary.blobs_stored;
    if !aliases.is_empty() {
        summary.blobs_rekeyed = aliases.len() as u64;
        let mut ab = Batcher::new(store);
        for a in aliases {
            ab.push(
                "alias",
                IMPORTER_ACTOR,
                a.ts,
                json!({
                    "namespace": "blob", "from": a.from, "to": a.to,
                    "created_at": a.created_at,
                    "origin": origin("blobs"),
                }),
            )
            .await?;
        }
        ab.flush().await?;
        summary.records += ab.written;
    }

    // Mail FTS membership: ingest-mailbox ∩ not-junk, exactly the ingest
    // path's post-commit stamp (fold-blind by design — see store/search.rs).
    for (id, title, body) in &mail.fts {
        store
            .index_entity("mail", id, title, fts_clip(body, FTS_CLIP_BYTES), &[])
            .await?;
    }
    summary.mail_fts_rows = mail.fts.len() as u64;
    Ok(())
}

async fn stage_people(pool: &PgPool, b: &mut Batcher<'_>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, slug, name, kind, owner, bio, role, created_at FROM people ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "people.created_at")?,
            json!({
                "kind": "person", "id": r.get::<String, _>("id"),
                "fields": {
                    "slug": r.get::<String, _>("slug"),
                    "name": r.get::<String, _>("name"),
                    "kind": r.get::<String, _>("kind"),
                    "owner": opt(r.get("owner")),
                    "bio": opt(r.get("bio")),
                    "role": opt(r.get("role")),
                    "created_at": created,
                },
                "origin": origin("people"),
            }),
        )
        .await?;
    }
    Ok(())
}

async fn stage_entity_types(pool: &PgPool, b: &mut Batcher<'_>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, slug, name, name_plural, description, icon, color, board_field, archived, \
         created_by, created_at, updated_at FROM entity_types ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        let actor: String = r.get("created_by");
        b.push(
            "entity.create",
            &actor,
            norm_ts(&created, "entity_types.created_at")?,
            json!({
                "kind": "entity_type", "id": r.get::<String, _>("id"),
                "fields": {
                    "slug": r.get::<String, _>("slug"),
                    "name": r.get::<String, _>("name"),
                    "name_plural": r.get::<String, _>("name_plural"),
                    "description": r.get::<String, _>("description"),
                    "icon": r.get::<String, _>("icon"),
                    "color": r.get::<String, _>("color"),
                    "board_field": opt(r.get("board_field")),
                    "archived": r.get::<bool, _>("archived"),
                    "created_by": actor,
                    "created_at": created,
                    "updated_at": r.get::<String, _>("updated_at"),
                },
                "origin": origin("entity_types"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT id, type_id, slug, label, field_type, required, position, options, ref_kind, \
         archived, created_at, updated_at FROM entity_fields ORDER BY type_id, position, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "entity_fields.created_at")?,
            json!({
                "kind": "entity_field", "id": r.get::<String, _>("id"),
                "fields": {
                    "type_id": r.get::<String, _>("type_id"),
                    "slug": r.get::<String, _>("slug"),
                    "label": r.get::<String, _>("label"),
                    "field_type": r.get::<String, _>("field_type"),
                    "required": r.get::<bool, _>("required"),
                    "position": r.get::<i64, _>("position"),
                    "options": r.get::<String, _>("options"),
                    "ref_kind": opt(r.get("ref_kind")),
                    "archived": r.get::<bool, _>("archived"),
                    "created_at": created,
                    "updated_at": r.get::<String, _>("updated_at"),
                },
                "origin": origin("entity_fields"),
            }),
        )
        .await?;
    }
    Ok(())
}

/// Built-in entity tables. One entity.create per row CARRYING THE FINAL LIVE
/// STATE (created_at AND updated_at verbatim): replay determinism is the
/// goal, not history reconstruction — the old instance kept no per-field
/// history to reconstruct anyway, so create-then-update records would
/// invent intermediate states that never existed.
async fn stage_builtin_entities(pool: &PgPool, b: &mut Batcher<'_>) -> Result<()> {
    let rows =
        sqlx::query("SELECT id, name, slug, created_at FROM projects ORDER BY created_at, id")
            .fetch_all(pool)
            .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "projects.created_at")?,
            json!({
                "kind": "project", "id": r.get::<String, _>("id"),
                "fields": {
                    "name": r.get::<String, _>("name"),
                    "slug": r.get::<String, _>("slug"),
                    "created_at": created,
                },
                "origin": origin("projects"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query("SELECT id, name, slug, created_at FROM topics ORDER BY created_at, id")
        .fetch_all(pool)
        .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "topics.created_at")?,
            json!({
                "kind": "topic", "id": r.get::<String, _>("id"),
                "fields": {
                    "name": r.get::<String, _>("name"),
                    "slug": r.get::<String, _>("slug"),
                    "created_at": created,
                },
                "origin": origin("topics"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT id, project, name, position, created_at FROM phases ORDER BY project, position, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "phases.created_at")?,
            json!({
                "kind": "phase", "id": r.get::<String, _>("id"),
                "fields": {
                    "project": r.get::<String, _>("project"),
                    "name": r.get::<String, _>("name"),
                    "position": r.get::<i64, _>("position"),
                    "created_at": created,
                },
                "origin": origin("phases"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT id, project, phase, due, title, body, status, priority, tags, assignees, \
         origin_entry_id, anchor_text, created_at, updated_at FROM tasks ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "tasks.created_at")?,
            json!({
                "kind": "task", "id": r.get::<String, _>("id"),
                "fields": {
                    "project": opt(r.get("project")),
                    "phase": opt(r.get("phase")),
                    "due": opt(r.get("due")),
                    "title": r.get::<String, _>("title"),
                    "body": r.get::<String, _>("body"),
                    "status": r.get::<String, _>("status"),
                    "priority": r.get::<String, _>("priority"),
                    "tags": r.get::<String, _>("tags"),
                    "assignees": r.get::<String, _>("assignees"),
                    "origin_entry_id": opt(r.get("origin_entry_id")),
                    "anchor_text": opt(r.get("anchor_text")),
                    "created_at": created,
                    "updated_at": r.get::<String, _>("updated_at"),
                },
                "origin": origin("tasks"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT id, title, context, decision, consequences, status, tags, assignees, project, \
         supersedes, origin_entry_id, anchor_text, created_at, updated_at FROM decisions \
         ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "decisions.created_at")?,
            json!({
                "kind": "decision", "id": r.get::<String, _>("id"),
                "fields": {
                    "title": r.get::<String, _>("title"),
                    "context": r.get::<String, _>("context"),
                    "decision": r.get::<String, _>("decision"),
                    "consequences": r.get::<String, _>("consequences"),
                    "status": r.get::<String, _>("status"),
                    "tags": r.get::<String, _>("tags"),
                    "assignees": r.get::<String, _>("assignees"),
                    "project": opt(r.get("project")),
                    "supersedes": opt(r.get("supersedes")),
                    "origin_entry_id": opt(r.get("origin_entry_id")),
                    "anchor_text": opt(r.get("anchor_text")),
                    "created_at": created,
                    "updated_at": r.get::<String, _>("updated_at"),
                },
                "origin": origin("decisions"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT id, title, body, at, tags, assignees, origin_entry_id, anchor_text, created_at \
         FROM events ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "events.created_at")?,
            json!({
                "kind": "event", "id": r.get::<String, _>("id"),
                "fields": {
                    "title": r.get::<String, _>("title"),
                    "body": r.get::<String, _>("body"),
                    "at": opt(r.get("at")),
                    "tags": r.get::<String, _>("tags"),
                    "assignees": r.get::<String, _>("assignees"),
                    "origin_entry_id": opt(r.get("origin_entry_id")),
                    "anchor_text": opt(r.get("anchor_text")),
                    "created_at": created,
                },
                "origin": origin("events"),
            }),
        )
        .await?;
    }
    Ok(())
}

/// Custom-entity instances: kind is the TYPE SLUG (the fold routes it to the
/// entities table); the inner JSONB rides as text (the fold's JsonText
/// binding passes pre-serialized strings through).
async fn stage_custom_entities(pool: &PgPool, b: &mut Batcher<'_>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT e.id, e.type_id, t.slug AS type_slug, e.title, e.fields::text AS fields, \
         e.user_scope, e.origin_entry_id, e.created_by, e.created_at, e.updated_at \
         FROM entities e JOIN entity_types t ON t.id = e.type_id ORDER BY e.created_at, e.id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        let actor: String = r.get("created_by");
        b.push(
            "entity.create",
            &actor,
            norm_ts(&created, "entities.created_at")?,
            json!({
                "kind": r.get::<String, _>("type_slug"), "id": r.get::<String, _>("id"),
                "fields": {
                    "type_id": r.get::<String, _>("type_id"),
                    "title": r.get::<String, _>("title"),
                    "fields": r.get::<String, _>("fields"),
                    "user_scope": opt(r.get("user_scope")),
                    "origin_entry_id": opt(r.get("origin_entry_id")),
                    "created_by": actor,
                    "created_at": created,
                    "updated_at": r.get::<String, _>("updated_at"),
                },
                "origin": origin("entities"),
            }),
        )
        .await?;
    }
    Ok(())
}

/// Journal entries with their anchors inline. `emerged`/`inbox` ride EMPTY
/// (omitted): every entity the entries once emerged already imported above
/// under its original id, and link records carry the graph — re-running
/// emergence here would mint duplicates.
async fn stage_journal(pool: &PgPool, b: &mut Batcher<'_>) -> Result<()> {
    let anchor_rows = sqlx::query(
        r#"SELECT id, entry_id, start, "end", text, kind, ref_id, created_at FROM anchors
           ORDER BY entry_id, start, id"#,
    )
    .fetch_all(pool)
    .await?;
    let mut anchors: HashMap<String, Vec<Json>> = HashMap::new();
    for r in &anchor_rows {
        anchors
            .entry(r.get::<String, _>("entry_id"))
            .or_default()
            .push(json!({
                "id": r.get::<String, _>("id"),
                "start": r.get::<i64, _>("start"),
                "end": r.get::<i64, _>("end"),
                "text": r.get::<String, _>("text"),
                "kind": r.get::<String, _>("kind"),
                "ref_id": r.get::<String, _>("ref_id"),
                "created_at": r.get::<String, _>("created_at"),
            }));
    }

    let rows = sqlx::query(
        "SELECT id, author, body, tags, mentions, user_scope, created_at FROM journal \
         ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let id: String = r.get("id");
        let author: String = r.get("author");
        let created: String = r.get("created_at");
        let mut payload = json!({
            "id": id,
            "author": author,
            "body": r.get::<String, _>("body"),
            "tags": json_arr(&r.get::<String, _>("tags")),
            "mentions": json_arr(&r.get::<String, _>("mentions")),
            "user_scope": opt(r.get("user_scope")),
            "created_at": created,
            "origin": origin("journal"),
        });
        if let Some(list) = anchors.remove(&id) {
            payload["anchors"] = Json::Array(list);
        }
        b.push(
            "journal.append",
            &author,
            norm_ts(&created, "journal.created_at")?,
            payload,
        )
        .await?;
    }
    if !anchors.is_empty() {
        let orphans: usize = anchors.values().map(Vec::len).sum();
        tracing::warn!("{orphans} anchor row(s) reference missing journal entries — skipped");
    }
    Ok(())
}

async fn stage_links(pool: &PgPool, b: &mut Batcher<'_>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, source_kind, source_id, target_kind, target_id, rel, created_at FROM links \
         ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "link.add",
            IMPORTER_ACTOR,
            norm_ts(&created, "links.created_at")?,
            json!({
                "id": r.get::<String, _>("id"),
                "source_kind": r.get::<String, _>("source_kind"),
                "source_id": r.get::<String, _>("source_id"),
                "target_kind": r.get::<String, _>("target_kind"),
                "target_id": r.get::<String, _>("target_id"),
                "rel": r.get::<String, _>("rel"),
                "created_at": created,
                "origin": origin("links"),
            }),
        )
        .await?;
    }
    Ok(())
}

/// Profile cards, Claude Code artifacts, platform identities, feed sources,
/// and the config kv — the fold's built-in kinds + config.set. `app.version`
/// stays behind (a fact about the retired instance, not this store).
async fn stage_profile_tier(pool: &PgPool, b: &mut Batcher<'_>) -> Result<()> {
    let rows = sqlx::query(
        "SELECT actor, kind, display_name, body, source, derived_at, updated_at FROM profile \
         ORDER BY actor",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let actor: String = r.get("actor");
        let updated: String = r.get("updated_at");
        b.push(
            "entity.create",
            &actor,
            norm_ts(&updated, "profile.updated_at")?,
            json!({
                "kind": "profile", "id": actor,
                "fields": {
                    "kind": r.get::<String, _>("kind"),
                    "display_name": r.get::<String, _>("display_name"),
                    "body": r.get::<String, _>("body"),
                    "source": r.get::<String, _>("source"),
                    "derived_at": opt(r.get("derived_at")),
                    "updated_at": updated,
                },
                "origin": origin("profile"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT id, actor, kind, name, content, description, enabled, created_at, updated_at \
         FROM identity_artifacts ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let actor: String = r.get("actor");
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            &actor,
            norm_ts(&created, "identity_artifacts.created_at")?,
            json!({
                "kind": "identity_artifact", "id": r.get::<String, _>("id"),
                "fields": {
                    "actor": actor,
                    "kind": r.get::<String, _>("kind"),
                    "name": r.get::<String, _>("name"),
                    "content": r.get::<String, _>("content"),
                    "description": r.get::<String, _>("description"),
                    "enabled": r.get::<bool, _>("enabled"),
                    "created_at": created,
                    "updated_at": r.get::<String, _>("updated_at"),
                },
                "origin": origin("identity_artifacts"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT id, platform, platform_id, actor, created_at FROM identities ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "identities.created_at")?,
            json!({
                "kind": "identity", "id": r.get::<String, _>("id"),
                "fields": {
                    "platform": r.get::<String, _>("platform"),
                    "platform_id": r.get::<String, _>("platform_id"),
                    "actor": r.get::<String, _>("actor"),
                    "created_at": created,
                },
                "origin": origin("identities"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT id, name, url, kind, category, severity, interval_secs, notify, enabled, owner, \
         last_polled_at, last_status, created_at FROM sources ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            IMPORTER_ACTOR,
            norm_ts(&created, "sources.created_at")?,
            json!({
                "kind": "source", "id": r.get::<String, _>("id"),
                "fields": {
                    "name": r.get::<String, _>("name"),
                    "url": r.get::<String, _>("url"),
                    "kind": r.get::<String, _>("kind"),
                    "category": opt(r.get("category")),
                    "severity": r.get::<String, _>("severity"),
                    "interval_secs": r.get::<i64, _>("interval_secs"),
                    "notify": opt(r.get("notify")),
                    "enabled": r.get::<bool, _>("enabled"),
                    "owner": opt(r.get("owner")),
                    "last_polled_at": opt(r.get("last_polled_at")),
                    "last_status": opt(r.get("last_status")),
                    "created_at": created,
                },
                "origin": origin("sources"),
            }),
        )
        .await?;
    }

    let rows = sqlx::query(
        "SELECT key, value, updated_at FROM config WHERE key <> 'app.version' ORDER BY key",
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let updated: String = r.get("updated_at");
        b.push(
            "config.set",
            IMPORTER_ACTOR,
            norm_ts(&updated, "config.updated_at")?,
            json!({
                "key": r.get::<String, _>("key"),
                "value": r.get::<String, _>("value"),
                "origin": origin("config"),
            }),
        )
        .await?;
    }
    Ok(())
}

/// Unread notifications only: read ones are spent state, not knowledge.
async fn stage_inbox(pool: &PgPool, b: &mut Batcher<'_>, summary: &mut Summary) -> Result<()> {
    summary.inbox_read_skipped =
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM inbox WHERE read_at IS NOT NULL")
            .fetch_one(pool)
            .await? as u64;
    let rows = sqlx::query(
        r#"SELECT id, recipient, "from", reason, ref_kind, ref_id, entry_id, snippet, created_at
           FROM inbox WHERE read_at IS NULL ORDER BY created_at, id"#,
    )
    .fetch_all(pool)
    .await?;
    for r in rows {
        let from: String = r.get("from");
        let created: String = r.get("created_at");
        b.push(
            "entity.create",
            &from,
            norm_ts(&created, "inbox.created_at")?,
            json!({
                "kind": "inbox", "id": r.get::<String, _>("id"),
                "fields": {
                    "recipient": r.get::<String, _>("recipient"),
                    "from": from,
                    "reason": r.get::<String, _>("reason"),
                    "ref_kind": r.get::<String, _>("ref_kind"),
                    "ref_id": r.get::<String, _>("ref_id"),
                    "entry_id": opt(r.get("entry_id")),
                    "snippet": r.get::<String, _>("snippet"),
                    "created_at": created,
                },
                "origin": origin("inbox"),
            }),
        )
        .await?;
    }
    Ok(())
}

// ── mail ─────────────────────────────────────────────────────────────────────

/// Bytes waiting for the blockstore pass, plus the FTS rows to stamp.
struct MailCarry {
    blobs: Vec<PendingBlob>,
    fts: Vec<(String, String, String)>, // (mail id, title, body)
}

struct PendingBlob {
    att_id: String,
    legacy_hash: Option<String>,
    mime: String,
    bytes: Vec<u8>,
    created_at: String,
    ts: String,
}

struct RekeyedAlias {
    from: String,
    to: String,
    created_at: String,
    ts: String,
}

async fn stage_mail(
    pool: &PgPool,
    b: &mut Batcher<'_>,
    summary: &mut Summary,
) -> Result<MailCarry> {
    // Accounts: identity/config columns in the module.doc (cred_id
    // deliberately absent — credentials re-enter against the OS keychain in
    // Phase 3), the whole JMAP sync state as ONE cursor.set so Phase 3
    // resumes as a delta resync. cursor.set stamps updated_at from the
    // record ts, which is the account's original updated_at.
    let rows = sqlx::query(
        "SELECT id, owner, address, jmap_url, jmap_username, jmap_account_id, email_state, \
         mailbox_state, backfill_status, backfill_cursor::text AS backfill_cursor, attempts, \
         next_attempt_at, last_error, last_synced_at, last_status, enabled, created_at, updated_at \
         FROM mail_accounts ORDER BY created_at, id",
    )
    .fetch_all(pool)
    .await?;
    let mut account_meta: HashMap<String, (String, String)> = HashMap::new(); // id → (owner, created_at)
    for r in &rows {
        let id: String = r.get("id");
        let owner: String = r.get("owner");
        let created: String = r.get("created_at");
        let updated: String = r.get("updated_at");
        b.push(
            "module.doc",
            &owner,
            norm_ts(&created, "mail_accounts.created_at")?,
            json!({
                "module": "mail", "doc_kind": "account", "id": id,
                "fields": {
                    "owner": owner,
                    "address": r.get::<String, _>("address"),
                    "jmap_url": r.get::<String, _>("jmap_url"),
                    "jmap_username": opt(r.get("jmap_username")),
                    "jmap_account_id": r.get::<String, _>("jmap_account_id"),
                    "enabled": r.get::<bool, _>("enabled"),
                    "created_at": created,
                    "updated_at": updated,
                },
                "origin": origin("mail_accounts"),
            }),
        )
        .await?;
        b.push(
            "cursor.set",
            &owner,
            norm_ts(&updated, "mail_accounts.updated_at")?,
            json!({
                "module": "mail", "account": id,
                "cursor": {
                    "email_state": opt(r.get("email_state")),
                    "mailbox_state": opt(r.get("mailbox_state")),
                    "backfill_status": r.get::<String, _>("backfill_status"),
                    "backfill_cursor": opt(r.get("backfill_cursor")),
                    "attempts": r.get::<i64, _>("attempts"),
                    "next_attempt_at": opt(r.get("next_attempt_at")),
                    "last_error": opt(r.get("last_error")),
                    "last_synced_at": opt(r.get("last_synced_at")),
                    "last_status": opt(r.get("last_status")),
                },
                "origin": origin("mail_accounts"),
            }),
        )
        .await?;
        account_meta.insert(id, (owner, created));
    }

    // Mailboxes (no timestamps of their own: the account's created_at is the
    // record ts — deterministic across runs). ingest carries over: it is
    // operator intent, and it drives FTS + embed eligibility below.
    let rows = sqlx::query(
        "SELECT id, account_id, jmap_id, name, role, ingest, sort_order FROM mail_mailboxes \
         ORDER BY account_id, sort_order, id",
    )
    .fetch_all(pool)
    .await?;
    let mut ingest_ids: HashMap<String, HashSet<String>> = HashMap::new();
    for r in &rows {
        let account_id: String = r.get("account_id");
        let jmap_id: String = r.get("jmap_id");
        let ingest: bool = r.get("ingest");
        let (owner, account_created) = account_meta
            .get(&account_id)
            .cloned()
            .ok_or_else(|| anyhow!("mailbox references missing mail account {account_id}"))?;
        if ingest {
            ingest_ids
                .entry(account_id.clone())
                .or_default()
                .insert(jmap_id.clone());
        }
        b.push(
            "module.doc",
            &owner,
            norm_ts(&account_created, "mail_accounts.created_at")?,
            json!({
                "module": "mail", "doc_kind": "mailbox", "id": r.get::<String, _>("id"),
                "fields": {
                    "account_id": account_id,
                    "jmap_id": jmap_id,
                    "name": r.get::<String, _>("name"),
                    "role": opt(r.get("role")),
                    "ingest": ingest,
                    "sort_order": r.get::<i64, _>("sort_order"),
                },
                "origin": origin("mail_mailboxes"),
            }),
        )
        .await?;
    }

    // Messages: live rows only — soft-deleted mail stays deleted (importing
    // then re-tombstoning would resurrect content into the log for nothing).
    // embed_state is recomputed with the ingest path's exact predicate
    // (ingest mailbox ∩ not junk): pending rows are what the app's backfill
    // and the Phase 3 embed drain pick up; nothing embeds during import.
    summary.mail_deleted_skipped = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM mail_messages WHERE deleted_at IS NOT NULL",
    )
    .fetch_one(pool)
    .await? as u64;
    let rows = sqlx::query(
        "SELECT id, account_id, jmap_id, jmap_thread_id, message_id_hdr, in_reply_to, \
         references_json, from_addr, from_name, to_json, cc_json, reply_to_json, subject, \
         sent_at, received_at, mailbox_ids_json, keywords_json, body_text, body_source, \
         snippet, size, has_attachments, user_scope, created_at, updated_at \
         FROM mail_messages WHERE deleted_at IS NULL ORDER BY account_id, received_at, id",
    )
    .fetch_all(pool)
    .await?;
    let mut fts: Vec<(String, String, String)> = Vec::new();
    for r in &rows {
        let id: String = r.get("id");
        let account_id: String = r.get("account_id");
        let owner: String = r.get("user_scope");
        let created: String = r.get("created_at");
        let mailbox_ids: Vec<String> = json_arr(&r.get::<String, _>("mailbox_ids_json"));
        let keywords_json: String = r.get("keywords_json");
        let subject: String = r.get("subject");
        let body_text: String = r.get("body_text");
        let eligible = !keywords_json.contains("\"$junk\"")
            && ingest_ids
                .get(&account_id)
                .is_some_and(|set| mailbox_ids.iter().any(|m| set.contains(m)));
        b.push(
            "module.doc",
            &owner,
            norm_ts(&created, "mail_messages.created_at")?,
            json!({
                "module": "mail", "doc_kind": "message", "id": id,
                "fields": {
                    "account_id": account_id,
                    "jmap_id": r.get::<String, _>("jmap_id"),
                    "jmap_thread_id": r.get::<String, _>("jmap_thread_id"),
                    "message_id_hdr": opt(r.get("message_id_hdr")),
                    "in_reply_to": opt(r.get("in_reply_to")),
                    "references_json": r.get::<String, _>("references_json"),
                    "from_addr": r.get::<String, _>("from_addr"),
                    "from_name": opt(r.get("from_name")),
                    "to_json": r.get::<String, _>("to_json"),
                    "cc_json": r.get::<String, _>("cc_json"),
                    "reply_to_json": r.get::<String, _>("reply_to_json"),
                    "subject": subject,
                    "sent_at": opt(r.get("sent_at")),
                    "received_at": r.get::<String, _>("received_at"),
                    "mailbox_ids_json": r.get::<String, _>("mailbox_ids_json"),
                    "keywords_json": keywords_json,
                    "body_text": body_text,
                    "body_source": r.get::<String, _>("body_source"),
                    "snippet": r.get::<String, _>("snippet"),
                    "size": r.get::<i64, _>("size"),
                    "has_attachments": r.get::<bool, _>("has_attachments"),
                    "embed_state": if eligible { "pending" } else { "skip" },
                    "user_scope": owner,
                    "created_at": created,
                    "updated_at": r.get::<String, _>("updated_at"),
                },
                "origin": origin("mail_messages"),
            }),
        )
        .await?;
        if eligible {
            let title = if subject.trim().is_empty() {
                "(no subject)".to_string()
            } else {
                subject.clone()
            };
            fts.push((id.clone(), title, body_text.clone()));
        }
    }

    // Attachment metadata for live messages. blob_hash is NOT carried here:
    // bytes land afterwards through the runtime path (store_attachment_blobs)
    // exactly like the fetch pipeline, and rows without local bytes keep
    // blob_hash NULL so Phase 3 can refetch by jmap_blob_id.
    let rows = sqlx::query(
        "SELECT t.id, t.message_id, t.blob_hash, t.jmap_blob_id, t.filename, t.mime, t.size, \
         t.content_id, t.disposition, t.skipped_reason, t.created_at, m.user_scope AS owner, \
         b.data AS blob_data \
         FROM mail_attachments t \
         JOIN mail_messages m ON m.id = t.message_id AND m.deleted_at IS NULL \
         LEFT JOIN blobs b ON b.hash = t.blob_hash \
         ORDER BY t.created_at, t.id",
    )
    .fetch_all(pool)
    .await?;
    let mut blobs: Vec<PendingBlob> = Vec::new();
    for r in &rows {
        let att_id: String = r.get("id");
        let owner: String = r.get("owner");
        let created: String = r.get("created_at");
        let mime: String = r.get("mime");
        b.push(
            "module.doc",
            &owner,
            norm_ts(&created, "mail_attachments.created_at")?,
            json!({
                "module": "mail", "doc_kind": "attachment", "id": att_id,
                "fields": {
                    "message_id": r.get::<String, _>("message_id"),
                    "jmap_blob_id": r.get::<String, _>("jmap_blob_id"),
                    "filename": r.get::<String, _>("filename"),
                    "mime": mime,
                    "size": r.get::<i64, _>("size"),
                    "content_id": opt(r.get("content_id")),
                    "disposition": opt(r.get("disposition")),
                    "skipped_reason": opt(r.get("skipped_reason")),
                    "created_at": created,
                },
                "origin": origin("mail_attachments"),
            }),
        )
        .await?;
        match r.get::<Option<Vec<u8>>, _>("blob_data") {
            Some(bytes) => blobs.push(PendingBlob {
                att_id,
                legacy_hash: r.get("blob_hash"),
                mime,
                bytes,
                ts: norm_ts(&created, "mail_attachments.created_at")?,
                created_at: created,
            }),
            None => summary.attachments_pending += 1,
        }
    }

    Ok(MailCarry { blobs, fts })
}

/// Stream attachment BYTEA through the blockstore via the runtime path
/// mail.rs uses (blocks + blob_refs + a module.doc pointing blob_hash at
/// them), verifying the round-trip byte-for-byte. The blob key is the blake3
/// of the bytes — the blockstore convention; a legacy hash that disagrees is
/// re-keyed and reported back for an `alias` record.
async fn store_attachment_blobs(
    store: &Store,
    blobs: Vec<PendingBlob>,
    summary: &mut Summary,
) -> Result<Vec<RekeyedAlias>> {
    let mut aliases = Vec::new();
    for blob in blobs {
        let hash = blake3::hash(&blob.bytes).to_hex().to_string();
        store
            .mail_attachment_store_blob(&blob.att_id, &hash, &blob.mime, &blob.bytes)
            .await
            .with_context(|| format!("storing attachment {} bytes", blob.att_id))?;
        let served = store
            .mail_attachment_serve(&blob.att_id)
            .await?
            .ok_or_else(|| anyhow!("attachment {} vanished after store", blob.att_id))?;
        if served.data.as_deref() != Some(blob.bytes.as_slice()) {
            bail!(
                "attachment {} failed its round-trip: blockstore bytes differ from the source",
                blob.att_id
            );
        }
        summary.blobs_stored += 1;
        if let Some(legacy) = blob.legacy_hash.filter(|h| *h != hash) {
            aliases.push(RekeyedAlias {
                from: legacy,
                to: hash,
                created_at: blob.created_at,
                ts: blob.ts,
            });
        }
    }
    Ok(aliases)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every timestamp shape the legacy instance ever wrote must normalize
    /// to the frozen envelope form: JS toISOString (already frozen), RFC
    /// 3339 with an offset, and the SQLite-era naive datetime('now')
    /// literals the original Postgres migration carried over (read as UTC).
    #[test]
    fn norm_ts_accepts_every_legacy_shape() {
        for (raw, want) in [
            ("2026-05-11T13:47:34.123Z", "2026-05-11T13:47:34.123Z"),
            ("2026-05-11T13:47:34+02:00", "2026-05-11T11:47:34.000Z"),
            ("2026-05-11 13:47:34", "2026-05-11T13:47:34.000Z"),
            ("2026-05-11 13:47:34.5", "2026-05-11T13:47:34.500Z"),
            ("2026-05-11T13:47:34", "2026-05-11T13:47:34.000Z"),
        ] {
            assert_eq!(norm_ts(raw, "t").unwrap(), want, "raw {raw:?}");
        }
        assert!(norm_ts("2026-05-11", "t").is_err());
        assert!(norm_ts("yesterday", "t").is_err());
    }

    /// grouped() must partition PLAN_TABLES: every table in exactly one
    /// group. Distinct per-table counts make any omission or double-count
    /// break the sum, so adding a source table without placing it in a
    /// group fails here (no database needed).
    #[test]
    fn grouped_covers_every_plan_table_exactly_once() {
        let plan = Plan {
            tables: PLAN_TABLES
                .iter()
                .enumerate()
                .map(|(i, t)| (*t, 1 + (i as i64)))
                .collect(),
        };
        let grouped_sum: i64 = plan.grouped().iter().map(|(_, n)| n).sum();
        assert_eq!(
            grouped_sum,
            plan.total_rows(),
            "grouped() must partition PLAN_TABLES"
        );
        assert!(plan.total_rows() > 0);
    }
}
