// The derived index (PR 1.5, D18/D27): an encrypted SQLite database holding
// ONLY state rebuildable from the op log — current-entity tables, the FTS5
// search index, and the embeddings/ANN candidate store. The op log
// (core/src/oplog) is the source of truth; this file is a projection of it,
// maintained by core/src/fold and disposable by design: drop the file, replay
// the logs, get byte-identical state back.
//
// ── Encryption at rest ──────────────────────────────────────────────────────
//
// The database is SQLCipher (rusqlite `bundled-sqlcipher-vendored-openssl`,
// the exact feature combo the PR 1.4 spike proved). It is keyed with the
// RAW-KEY form (`PRAGMA key = "x'<64 hex>'"`), which bypasses SQLCipher's
// PBKDF2 passphrase KDF — the key is already high-entropy. The key is NOT the
// master key itself: it is derived through a dedicated blake3 context,
//
//   index_key = blake3::derive_key("hive-index-key-v1", master)
//
// so the index never holds the root of the key hierarchy (same domain-
// separation discipline as the oplog segment keys and blob keys; a leaked
// index key cannot unwrap segments or blobs, and rotating the index key is
// independent of the log).
//
// ── Fold versioning ─────────────────────────────────────────────────────────
//
// `PRAGMA user_version` stores fold::FOLD_VERSION. On mismatch, open DROPS
// every derived object and resets the fold watermark, then lays fresh DDL —
// log replay rebuilds the state; that is the design (D14: "migration" mostly
// means bumping the fold version). A mismatch on a non-empty database logs a
// warning so a surprising rebuild is at least a visible one.
//
// ── Schema (translated from core/src/db.rs, the Postgres reference) ─────────
//
// Column names are kept EXACTLY aligned with the Postgres DDL so the PR 1.6
// store port is mechanical. Deliberate divergences, in one place:
//
//   - tsvector/GIN artifacts don't exist; FTS5 (`search` + external-content
//     `search_fts` + sync triggers) replaces them. The `search` table keeps
//     the Postgres column set (kind, ref_id, title, body) minus `tsv`.
//   - JSONB columns are TEXT holding JSON (queried via json_extract; a JSONB
//     type name would get NUMERIC affinity in SQLite and mangle strings).
//   - embeddings drops the pgvector-only `vec_v` column, its HNSW index, and
//     the vec-presence CHECK; `vec` (packed little-endian f32 BLOB) is NOT
//     NULL instead. `ann_keys` (new) pairs u64 ANN handles with
//     (ref_kind, ref_id, chunk_idx).
//   - mail_attachments' UNIQUE NULLS NOT DISTINCT becomes a unique expression
//     index over COALESCE(content_id, '') — SQLite UNIQUE always treats NULLs
//     as distinct.
//   - mail_attachments.blob_hash no longer REFERENCES a blobs table: bytes
//     live in the PR 1.4 blockstore; the column keeps the blake3 hash.
//   - tasks.phase/tasks.due (ALTER-added in Postgres) are in the base DDL.
//   - Postgres-era tables that do NOT cross: wire (bus-only after 1.6),
//     outbox, blobs (blockstore), cc_credentials (Phase 3 keychain),
//     identities (not in the 1.5 set).
//   - aliases (new, fold-owned) projects `alias` records; fold_meta (new)
//     holds the per-device applied-seq watermark.
//   - worker_status crosses (trivially portable) but is RUNTIME state the
//     1.6 store writes directly — the fold never touches it.
//
// Timestamps stay TEXT in the store's 24-char ISO shape (see store/mod.rs) —
// lexicographic order is chronological order.
//
// ── Determinism ─────────────────────────────────────────────────────────────
//
// This module sits inside the determinism grep fence (core/tests/
// determinism.rs) with oplog/blockstore/fold: no clocks, no RNG, no
// environment reads. Key material arrives via keys::KeySource; every
// timestamp arrives from callers or records.

pub mod ann;
pub mod fts;

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use hive_shared::SearchHit;
use rusqlite::{params, Connection, OptionalExtension};

use crate::fold::FOLD_VERSION;
use crate::keys::KeySource;
use ann::{new_ann_index, AnnIndex};

/// blake3 derive_key context for the SQLCipher raw key (frozen; a new context
/// string is a new, incompatible index key — replay would rebuild).
pub const INDEX_KEY_CONTEXT: &str = "hive-index-key-v1";

/// File name of the derived database inside the data dir.
pub const INDEX_DB_FILE: &str = "index.db";

/// One ANN candidate, hydrated back to its embeddings-row identity. Chunk
/// collapse and kind weighting happen above (store/semantic.rs shape).
#[derive(Debug, Clone, PartialEq)]
pub struct AnnCandidate {
    pub ref_kind: String,
    pub ref_id: String,
    pub chunk_idx: i64,
    /// Cosine similarity, higher is better (the Postgres probes' orientation).
    pub score: f32,
}

/// The encrypted derived store: one rusqlite connection (single writer — from
/// PR 1.6 it lives on the store's writer thread) plus the in-memory ANN
/// structures, one per embedding model, rebuilt from the `embeddings` table
/// at open.
pub struct SqliteIndex {
    conn: Connection,
    anns: HashMap<String, Box<dyn AnnIndex>>,
}

impl SqliteIndex {
    /// Open (creating if absent) `<data_dir>/index.db`, keyed from
    /// `keys.master_key()` via `INDEX_KEY_CONTEXT`. Applies idempotent DDL,
    /// enforces the fold version (see module header), turns on WAL and
    /// foreign keys, and rebuilds the ANN structures from `embeddings`.
    pub fn open(data_dir: &Path, keys: &dyn KeySource) -> Result<SqliteIndex> {
        Self::open_with_fold_version(data_dir, keys, FOLD_VERSION)
    }

    /// `open` with an explicit fold version. Exists for tests (exercising the
    /// drop-and-replay reset without editing fold::FOLD_VERSION); production
    /// code uses `open`. Same precedent as oplog's open_with_segment_limit.
    pub fn open_with_fold_version(
        data_dir: &Path,
        keys: &dyn KeySource,
        fold_version: u32,
    ) -> Result<SqliteIndex> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;
        let path = data_dir.join(INDEX_DB_FILE);
        let conn = Connection::open(&path)
            .with_context(|| format!("opening index db {}", path.display()))?;

        // Key must be the first statement on the connection. Raw-key form:
        // the VALUE SQLCipher receives is the string x'<hex>' (rusqlite's
        // pragma quoting preserves it), which routes around the passphrase
        // KDF and uses the 32 bytes directly.
        let master = keys.master_key()?;
        let index_key = blake3::derive_key(INDEX_KEY_CONTEXT, &master);
        let hex = data_encoding::HEXLOWER.encode(&index_key);
        conn.pragma_update(None, "key", format!("x'{hex}'"))
            .context("applying SQLCipher key")?;
        // Prove the key actually opens the file (SQLCipher defers the check
        // to the first real read) — and that this is a SQLCipher build at
        // all, not stock SQLite silently writing plaintext.
        let cipher_version: Option<String> = conn
            .query_row("PRAGMA cipher_version", [], |r| r.get(0))
            .optional()
            .context("PRAGMA cipher_version")?;
        if cipher_version.is_none_or(|v| v.is_empty()) {
            bail!("rusqlite was built without SQLCipher (cipher_version empty)");
        }
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))
            .context("index db key check failed (wrong master key or corrupted file)")?;

        // WAL for crash-safe single-writer throughput; FKs for the mail
        // cascade semantics the Postgres schema encodes.
        let _mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
            .context("enabling WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .context("enabling foreign keys")?;

        // Fold-version gate. A fresh database (user_version 0, no tables) is
        // bootstrapped silently; a version mismatch on existing state drops
        // and warns — replay rebuilds, by design.
        let on_disk: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("reading user_version")?;
        if on_disk != fold_version {
            let has_state: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'fold_meta')",
                    [],
                    |r| r.get(0),
                )
                .context("probing for existing derived tables")?;
            if has_state {
                tracing::warn!(
                    on_disk,
                    expected = fold_version,
                    "fold version mismatch: dropping derived tables and resetting the \
                     fold watermark; state rebuilds by op-log replay"
                );
                conn.execute_batch(DROP_DERIVED)
                    .context("dropping derived tables for fold-version reset")?;
            }
            conn.pragma_update(None, "user_version", fold_version)
                .context("stamping user_version")?;
        }

        conn.execute_batch(DDL).context("applying derived DDL")?;

        let mut index = SqliteIndex {
            conn,
            anns: HashMap::new(),
        };
        index.rebuild_ann()?;
        Ok(index)
    }

    /// The underlying connection (read paths, tests, and the 1.6 store port).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Mutable connection access — the seam the fold's transaction runs
    /// through (see `Self::fold`).
    pub fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    // ── fold plumbing ───────────────────────────────────────────────────────

    /// Apply a batch of op-log records in ONE transaction, advancing the
    /// per-device watermark. Records at or below their device's watermark are
    /// SKIPPED (the idempotent crash-heal path: tail replay after a crash may
    /// re-present already-folded records); records beyond watermark+1 are a
    /// gap and fail the whole batch (nothing commits). Returns how many
    /// records were actually applied.
    pub fn fold(&mut self, records: &[crate::oplog::Record]) -> Result<usize> {
        let tx = self.conn.transaction().context("beginning fold tx")?;
        let mut applied = 0usize;
        for rec in records {
            let watermark = fold_watermark(&tx, &rec.device)?;
            if watermark.is_some_and(|w| rec.seq <= w) {
                continue; // already folded — idempotent skip
            }
            crate::fold::apply(&tx, rec)
                .with_context(|| format!("folding {} {}#{}", rec.kind, rec.device, rec.seq))?;
            applied += 1;
        }
        tx.commit().context("committing fold tx")?;
        Ok(applied)
    }

    /// The fold watermark for a device (None = nothing folded yet).
    pub fn applied_seq(&self, device: &str) -> Result<Option<u64>> {
        fold_watermark(&self.conn, device)
    }

    // ── keyword search (FTS5) ───────────────────────────────────────────────

    /// Keyword search over the FTS5 index — the SQLite successor of
    /// store/semantic.rs `search()`, same result shape (SearchHit field names
    /// unchanged). `kinds` filters (not boosts); None = all kinds. Ranking is
    /// `bm25()` ascending, surfaced as the normalized descending
    /// `fts::bm25_score`; excerpts come from `snippet()` with the same
    /// `[`/`]` markers ts_headline used.
    pub fn keyword_search(
        &self,
        query: &str,
        kinds: Option<&[&str]>,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        let match_q = fts::fts5_query(query);
        if match_q.is_empty() {
            return Ok(vec![]);
        }
        let mut sql = String::from(
            "SELECT kind, ref_id, title, \
             snippet(search_fts, 3, '[', ']', '…', 14) AS snip, \
             bm25(search_fts) AS rank \
             FROM search_fts WHERE search_fts MATCH ?1",
        );
        if let Some(ks) = kinds {
            if ks.is_empty() {
                return Ok(vec![]);
            }
            sql.push_str(" AND kind IN (");
            sql.push_str(&vec!["?"; ks.len()].join(","));
            sql.push(')');
        }
        sql.push_str(" ORDER BY rank LIMIT ?");

        let mut stmt = self.conn.prepare(&sql)?;
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        params_vec.push(Box::new(match_q));
        for k in kinds.unwrap_or(&[]) {
            params_vec.push(Box::new(k.to_string()));
        }
        params_vec.push(Box::new(limit as i64));
        let rows = stmt.query_map(
            rusqlite::params_from_iter(params_vec.iter().map(|p| p.as_ref())),
            |r| {
                Ok(SearchHit {
                    kind: r.get(0)?,
                    id: r.get(1)?,
                    title: r.get(2)?,
                    snippet: r.get(3)?,
                    score: fts::bm25_score(r.get::<_, f64>(4)?),
                })
            },
        )?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        Ok(hits)
    }

    // ── embeddings + ANN ────────────────────────────────────────────────────

    /// Insert or replace one chunk vector. Columns mirror the Postgres
    /// embeddings table (owner/chunk_idx/hash carried; dim = vec.len()).
    /// `created_at` arrives from the caller — this layer reads no clock. The
    /// vector persists as a packed little-endian f32 BLOB and the in-memory
    /// ANN for `model` updates incrementally.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_embedding(
        &mut self,
        ref_kind: &str,
        ref_id: &str,
        chunk_idx: i64,
        model: &str,
        owner: Option<&str>,
        vec: &[f32],
        hash: &str,
        created_at: &str,
    ) -> Result<()> {
        let old_model: Option<String> = self
            .conn
            .query_row(
                "SELECT model FROM embeddings WHERE ref_kind = ?1 AND ref_id = ?2 AND chunk_idx = ?3",
                params![ref_kind, ref_id, chunk_idx],
                |r| r.get(0),
            )
            .optional()?;
        self.conn.execute(
            "INSERT INTO embeddings (ref_kind, ref_id, chunk_idx, model, dim, owner, vec, hash, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
             ON CONFLICT (ref_kind, ref_id, chunk_idx) DO UPDATE SET \
             model = excluded.model, dim = excluded.dim, owner = excluded.owner, \
             vec = excluded.vec, hash = excluded.hash, created_at = excluded.created_at",
            params![
                ref_kind,
                ref_id,
                chunk_idx,
                model,
                vec.len() as i64,
                owner,
                pack_vec(vec),
                hash,
                created_at
            ],
        )?;
        self.conn.execute(
            "INSERT OR IGNORE INTO ann_keys (ref_kind, ref_id, chunk_idx) VALUES (?1, ?2, ?3)",
            params![ref_kind, ref_id, chunk_idx],
        )?;
        let key: u64 = self.conn.query_row(
            "SELECT key FROM ann_keys WHERE ref_kind = ?1 AND ref_id = ?2 AND chunk_idx = ?3",
            params![ref_kind, ref_id, chunk_idx],
            |r| r.get::<_, i64>(0).map(|k| k as u64),
        )?;
        // A model swap on an existing chunk must not leave the key behind in
        // the old model's structure.
        if let Some(old) = old_model.filter(|m| m != model) {
            if let Some(ann) = self.anns.get_mut(&old) {
                ann.remove(key);
            }
        }
        self.anns
            .entry(model.to_string())
            .or_insert_with(|| new_ann_index(vec.len()))
            .upsert(key, vec);
        Ok(())
    }

    /// Drop every chunk vector for (ref_kind, ref_id): embeddings rows,
    /// ann_keys rows, and the in-memory ANN entries.
    pub fn remove_embeddings(&mut self, ref_kind: &str, ref_id: &str) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT k.key, e.model FROM ann_keys k \
             JOIN embeddings e ON e.ref_kind = k.ref_kind AND e.ref_id = k.ref_id AND e.chunk_idx = k.chunk_idx \
             WHERE k.ref_kind = ?1 AND k.ref_id = ?2",
        )?;
        let pairs: Vec<(u64, String)> = stmt
            .query_map(params![ref_kind, ref_id], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        for (key, model) in &pairs {
            if let Some(ann) = self.anns.get_mut(model) {
                ann.remove(*key);
            }
        }
        self.conn.execute(
            "DELETE FROM embeddings WHERE ref_kind = ?1 AND ref_id = ?2",
            params![ref_kind, ref_id],
        )?;
        self.conn.execute(
            "DELETE FROM ann_keys WHERE ref_kind = ?1 AND ref_id = ?2",
            params![ref_kind, ref_id],
        )?;
        Ok(())
    }

    /// ANN candidate probe for one model: the k nearest chunk vectors,
    /// hydrated to (ref_kind, ref_id, chunk_idx, similarity). Chunk collapse,
    /// kind weights, and row hydration happen above this call, exactly as in
    /// store/semantic.rs. Keys whose ann_keys row has vanished (a fold
    /// tombstone raced the in-memory structure) are silently dropped — same
    /// orphan tolerance as the Postgres path's hydrate-miss rule.
    pub fn ann_candidates(
        &self,
        model: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<AnnCandidate>> {
        let Some(ann) = self.anns.get(model) else {
            return Ok(vec![]);
        };
        let mut stmt = self
            .conn
            .prepare("SELECT ref_kind, ref_id, chunk_idx FROM ann_keys WHERE key = ?1")?;
        let mut out = Vec::new();
        for (key, score) in ann.candidates(query, k) {
            let row = stmt
                .query_row(params![key as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .optional()?;
            if let Some((ref_kind, ref_id, chunk_idx)) = row {
                out.push(AnnCandidate {
                    ref_kind,
                    ref_id,
                    chunk_idx,
                    score,
                });
            }
        }
        Ok(out)
    }

    /// Vectors currently indexed for `model` (0 when the model is unknown).
    pub fn ann_len(&self, model: &str) -> usize {
        self.anns.get(model).map_or(0, |a| a.len())
    }

    /// Rebuild the in-memory ANN structures from the embeddings table (the
    /// open path; also the recovery story for any in-memory staleness).
    fn rebuild_ann(&mut self) -> Result<()> {
        self.anns.clear();
        let mut stmt = self.conn.prepare(
            "SELECT k.key, e.model, e.dim, e.vec FROM embeddings e \
             JOIN ann_keys k ON k.ref_kind = e.ref_kind AND k.ref_id = e.ref_id AND k.chunk_idx = e.chunk_idx \
             ORDER BY k.key",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? as usize,
                r.get::<_, Vec<u8>>(3)?,
            ))
        })?;
        for row in rows {
            let (key, model, dim, blob) = row?;
            self.anns
                .entry(model)
                .or_insert_with(|| new_ann_index(dim))
                .upsert(key, &unpack_vec(&blob));
        }
        Ok(())
    }

    // ── diagnostics ─────────────────────────────────────────────────────────

    /// A canonical text dump of every fold-owned table, for byte-equality
    /// assertions (replay determinism) and rebuild verification. Fixed table
    /// order, rows ordered by primary key, stable value rendering. Excludes
    /// the runtime/non-fold tables (embeddings, ann_keys, worker_status) and
    /// the FTS shadow (its content table `search` IS included; consistency of
    /// the shadow itself is checked via fts_integrity_check).
    pub fn canonical_dump(&self) -> Result<String> {
        let mut out = String::new();
        for (table, order_by) in DUMP_TABLES {
            let sql = format!("SELECT * FROM {table} ORDER BY {order_by}");
            let mut stmt = self.conn.prepare(&sql)?;
            let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                out.push_str(table);
                for (i, col) in cols.iter().enumerate() {
                    use rusqlite::types::ValueRef;
                    let rendered = match row.get_ref(i)? {
                        ValueRef::Null => "∅".to_string(),
                        ValueRef::Integer(v) => v.to_string(),
                        ValueRef::Real(v) => format!("{v:?}"),
                        ValueRef::Text(t) => format!("{:?}", String::from_utf8_lossy(t)),
                        ValueRef::Blob(b) => data_encoding::HEXLOWER.encode(b),
                    };
                    out.push_str(&format!("|{col}={rendered}"));
                }
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Run FTS5's external-content integrity check (errors if the search_fts
    /// shadow disagrees with the `search` content table).
    pub fn fts_integrity_check(&self) -> Result<()> {
        self.conn
            .execute_batch("INSERT INTO search_fts(search_fts, rank) VALUES ('integrity-check', 0)")
            .context("search_fts integrity check failed")?;
        Ok(())
    }
}

/// Watermark lookup shared by `fold` (on the tx) and `applied_seq` (on the
/// connection).
fn fold_watermark(conn: &Connection, device: &str) -> Result<Option<u64>> {
    Ok(conn
        .query_row(
            "SELECT applied_seq FROM fold_meta WHERE device = ?1",
            params![device],
            |r| r.get::<_, i64>(0).map(|v| v as u64),
        )
        .optional()?)
}

/// Pack an f32 slice as little-endian bytes (the embeddings.vec format —
/// identical to hive_embed::to_blob, local so the fenced layer stands alone).
pub fn pack_vec(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for f in v {
        out.extend_from_slice(&f.to_le_bytes());
    }
    out
}

/// Reverse of `pack_vec`; a trailing partial float is dropped.
pub fn unpack_vec(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Tables in the canonical dump, with their ORDER BY (primary key) so two
/// databases folded from the same records render identically regardless of
/// physical row order.
const DUMP_TABLES: &[(&str, &str)] = &[
    ("journal", "id"),
    ("anchors", "id"),
    ("projects", "id"),
    ("topics", "id"),
    ("phases", "id"),
    ("tasks", "id"),
    ("decisions", "id"),
    ("events", "id"),
    ("people", "id"),
    ("profile", "actor"),
    ("inbox", "id"),
    ("links", "id"),
    ("config", "key"),
    ("identity_artifacts", "id"),
    ("entity_types", "id"),
    ("entity_fields", "id"),
    ("entities", "id"),
    ("mail_accounts", "id"),
    ("mail_mailboxes", "id"),
    ("mail_messages", "id"),
    ("mail_attachments", "id"),
    ("sources", "id"),
    ("aliases", "namespace, \"from\""),
    ("search", "kind, ref_id"),
    ("fold_meta", "device"),
];

/// Everything `open` drops on a fold-version mismatch. Triggers and indexes
/// go with their tables; sqlite_sequence rows (ann_keys AUTOINCREMENT) go
/// with ann_keys.
const DROP_DERIVED: &str = r#"
    DROP TABLE IF EXISTS search_fts;
    DROP TABLE IF EXISTS search;
    DROP TABLE IF EXISTS journal;
    DROP TABLE IF EXISTS anchors;
    DROP TABLE IF EXISTS projects;
    DROP TABLE IF EXISTS topics;
    DROP TABLE IF EXISTS phases;
    DROP TABLE IF EXISTS tasks;
    DROP TABLE IF EXISTS decisions;
    DROP TABLE IF EXISTS events;
    DROP TABLE IF EXISTS people;
    DROP TABLE IF EXISTS profile;
    DROP TABLE IF EXISTS inbox;
    DROP TABLE IF EXISTS links;
    DROP TABLE IF EXISTS config;
    DROP TABLE IF EXISTS identity_artifacts;
    DROP TABLE IF EXISTS entity_types;
    DROP TABLE IF EXISTS entity_fields;
    DROP TABLE IF EXISTS entities;
    DROP TABLE IF EXISTS mail_attachments;
    DROP TABLE IF EXISTS mail_messages;
    DROP TABLE IF EXISTS mail_mailboxes;
    DROP TABLE IF EXISTS mail_accounts;
    DROP TABLE IF EXISTS sources;
    DROP TABLE IF EXISTS worker_status;
    DROP TABLE IF EXISTS embeddings;
    DROP TABLE IF EXISTS ann_keys;
    DROP TABLE IF EXISTS aliases;
    DROP TABLE IF EXISTS fold_meta;
"#;

/// The derived schema. Idempotent (IF NOT EXISTS throughout); column names
/// and order track core/src/db.rs — see the module header for the deliberate
/// divergences.
const DDL: &str = r#"
    -- The journal projection: append-only prose, one row per journal.append.
    CREATE TABLE IF NOT EXISTS journal (
      id         TEXT PRIMARY KEY,
      author     TEXT NOT NULL,
      body       TEXT NOT NULL,
      tags       TEXT NOT NULL DEFAULT '[]',
      mentions   TEXT NOT NULL DEFAULT '[]',
      user_scope TEXT,
      created_at TEXT NOT NULL
    );

    -- A span of a journal entry that produced a structured entity.
    CREATE TABLE IF NOT EXISTS anchors (
      id         TEXT PRIMARY KEY,
      entry_id   TEXT NOT NULL,
      start      BIGINT NOT NULL,
      "end"      BIGINT NOT NULL,
      text       TEXT NOT NULL,
      kind       TEXT NOT NULL,
      ref_id     TEXT NOT NULL,
      created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS anchors_entry ON anchors (entry_id);
    CREATE INDEX IF NOT EXISTS anchors_ref ON anchors (ref_id);

    CREATE TABLE IF NOT EXISTS projects (
      id         TEXT PRIMARY KEY,
      name       TEXT NOT NULL UNIQUE,
      slug       TEXT NOT NULL DEFAULT '',
      created_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS topics (
      id         TEXT PRIMARY KEY,
      name       TEXT NOT NULL,
      slug       TEXT NOT NULL UNIQUE,
      created_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS phases (
      id         TEXT PRIMARY KEY,
      project    TEXT NOT NULL,
      name       TEXT NOT NULL,
      position   BIGINT NOT NULL DEFAULT 0,
      created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS phases_project ON phases (project);

    -- phase/due were ALTER-added in Postgres; base columns here.
    CREATE TABLE IF NOT EXISTS tasks (
      id              TEXT PRIMARY KEY,
      project         TEXT,
      phase           TEXT,
      due             TEXT,
      title           TEXT NOT NULL,
      body            TEXT NOT NULL DEFAULT '',
      status          TEXT NOT NULL DEFAULT 'todo',
      priority        TEXT NOT NULL DEFAULT 'normal',
      tags            TEXT NOT NULL DEFAULT '[]',
      assignees       TEXT NOT NULL DEFAULT '[]',
      origin_entry_id TEXT,
      anchor_text     TEXT,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS decisions (
      id              TEXT PRIMARY KEY,
      title           TEXT NOT NULL,
      context         TEXT NOT NULL DEFAULT '',
      decision        TEXT NOT NULL,
      consequences    TEXT NOT NULL DEFAULT '',
      status          TEXT NOT NULL DEFAULT 'proposed',
      tags            TEXT NOT NULL DEFAULT '[]',
      assignees       TEXT NOT NULL DEFAULT '[]',
      project         TEXT,
      supersedes      TEXT,
      origin_entry_id TEXT,
      anchor_text     TEXT,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS events (
      id              TEXT PRIMARY KEY,
      title           TEXT NOT NULL,
      body            TEXT NOT NULL DEFAULT '',
      at              TEXT,
      tags            TEXT NOT NULL DEFAULT '[]',
      assignees       TEXT NOT NULL DEFAULT '[]',
      origin_entry_id TEXT,
      anchor_text     TEXT,
      created_at      TEXT NOT NULL
    );

    -- Per-actor inbox. Fan-out rows are COMMAND-LAYER products: the fold only
    -- inserts what a record's payload explicitly carries (determinism).
    CREATE TABLE IF NOT EXISTS inbox (
      id         TEXT PRIMARY KEY,
      recipient  TEXT NOT NULL,
      "from"     TEXT NOT NULL,
      reason     TEXT NOT NULL,
      ref_kind   TEXT NOT NULL,
      ref_id     TEXT NOT NULL,
      entry_id   TEXT,
      snippet    TEXT NOT NULL DEFAULT '',
      created_at TEXT NOT NULL,
      read_at    TEXT
    );
    CREATE INDEX IF NOT EXISTS inbox_recipient ON inbox (recipient, read_at);

    CREATE TABLE IF NOT EXISTS links (
      id          TEXT PRIMARY KEY,
      source_kind TEXT NOT NULL,
      source_id   TEXT NOT NULL,
      target_kind TEXT NOT NULL,
      target_id   TEXT NOT NULL,
      rel         TEXT NOT NULL DEFAULT 'relates',
      created_at  TEXT NOT NULL
    );

    -- Worker config: external feeds polled into notifications.
    CREATE TABLE IF NOT EXISTS sources (
      id            TEXT PRIMARY KEY,
      name          TEXT NOT NULL,
      url           TEXT NOT NULL,
      kind          TEXT NOT NULL DEFAULT 'rss',
      category      TEXT,
      severity      TEXT NOT NULL DEFAULT 'info',
      interval_secs BIGINT NOT NULL DEFAULT 900,
      notify        TEXT,
      enabled       BOOLEAN NOT NULL DEFAULT TRUE,
      owner         TEXT,
      last_polled_at TEXT,
      last_status   TEXT,
      created_at    TEXT NOT NULL
    );

    -- Local embeddings, one row per chunk. vec = packed little-endian f32
    -- (NOT NULL here: the pgvector vec_v column and its CHECK don't cross).
    -- Maintained by the embed pipeline ABOVE the fold, except that tombstone/
    -- redact records delete rows so shredded content leaves retrieval.
    CREATE TABLE IF NOT EXISTS embeddings (
      ref_kind   TEXT NOT NULL,
      ref_id     TEXT NOT NULL,
      chunk_idx  INT  NOT NULL DEFAULT 0,
      model      TEXT NOT NULL,
      dim        BIGINT NOT NULL,
      owner      TEXT,
      vec        BLOB NOT NULL,
      hash       TEXT NOT NULL,
      created_at TEXT NOT NULL,
      PRIMARY KEY (ref_kind, ref_id, chunk_idx)
    );
    CREATE INDEX IF NOT EXISTS embeddings_owner ON embeddings (owner);
    CREATE INDEX IF NOT EXISTS embeddings_kind ON embeddings (ref_kind);

    -- u64 ANN handle ↔ embeddings row. AUTOINCREMENT so a deleted row's key
    -- is never reissued (a reissued key could alias a stale in-memory entry).
    CREATE TABLE IF NOT EXISTS ann_keys (
      key       INTEGER PRIMARY KEY AUTOINCREMENT,
      ref_kind  TEXT NOT NULL,
      ref_id    TEXT NOT NULL,
      chunk_idx INT NOT NULL DEFAULT 0,
      UNIQUE (ref_kind, ref_id, chunk_idx)
    );

    -- Single-row worker heartbeat / last-run stats. Runtime state, written
    -- directly by the store (never the fold).
    CREATE TABLE IF NOT EXISTS worker_status (
      id         BIGINT PRIMARY KEY CHECK (id = 1),
      heartbeat  TEXT,
      last_run   TEXT
    );

    -- Writers: every human and AI that can author journal entries.
    CREATE TABLE IF NOT EXISTS people (
      id         TEXT PRIMARY KEY,
      slug       TEXT NOT NULL UNIQUE,
      name       TEXT NOT NULL,
      kind       TEXT NOT NULL DEFAULT 'human',
      owner      TEXT,
      bio        TEXT,
      role       TEXT,
      created_at TEXT NOT NULL
    );

    -- Key/value instance config.
    CREATE TABLE IF NOT EXISTS config (
      key        TEXT PRIMARY KEY,
      value      TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );

    -- Mutable per-actor card (humans + AIs); body is a JSON object.
    CREATE TABLE IF NOT EXISTS profile (
      actor        TEXT PRIMARY KEY,
      kind         TEXT NOT NULL DEFAULT 'human',
      display_name TEXT NOT NULL DEFAULT '',
      body         TEXT NOT NULL DEFAULT '{}',
      source       TEXT NOT NULL DEFAULT 'manual',
      derived_at   TEXT,
      updated_at   TEXT NOT NULL
    );

    -- Claude Code artifacts (skills / agents / slash-commands) per AI identity.
    CREATE TABLE IF NOT EXISTS identity_artifacts (
      id          TEXT PRIMARY KEY,
      actor       TEXT NOT NULL,
      kind        TEXT NOT NULL,
      name        TEXT NOT NULL,
      content     TEXT NOT NULL,
      description TEXT NOT NULL DEFAULT '',
      enabled     BOOLEAN NOT NULL DEFAULT TRUE,
      created_at  TEXT NOT NULL,
      updated_at  TEXT NOT NULL,
      UNIQUE (actor, kind, name)
    );
    CREATE INDEX IF NOT EXISTS identity_artifacts_actor ON identity_artifacts (actor);

    -- The unified full-text content table (Postgres `search` minus tsv) and
    -- its external-content FTS5 shadow. The triggers keep the shadow in
    -- lockstep with every INSERT/UPDATE/DELETE, so fold handlers only touch
    -- `search` itself (the same DELETE+INSERT path the Postgres store uses).
    CREATE TABLE IF NOT EXISTS search (
      kind   TEXT NOT NULL,
      ref_id TEXT NOT NULL,
      title  TEXT NOT NULL DEFAULT '',
      body   TEXT NOT NULL DEFAULT '',
      PRIMARY KEY (kind, ref_id)
    );
    CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
      kind UNINDEXED, ref_id UNINDEXED, title, body,
      content='search', content_rowid='rowid'
    );
    CREATE TRIGGER IF NOT EXISTS search_fts_ai AFTER INSERT ON search BEGIN
      INSERT INTO search_fts(rowid, kind, ref_id, title, body)
      VALUES (new.rowid, new.kind, new.ref_id, new.title, new.body);
    END;
    CREATE TRIGGER IF NOT EXISTS search_fts_ad AFTER DELETE ON search BEGIN
      INSERT INTO search_fts(search_fts, rowid, kind, ref_id, title, body)
      VALUES ('delete', old.rowid, old.kind, old.ref_id, old.title, old.body);
    END;
    CREATE TRIGGER IF NOT EXISTS search_fts_au AFTER UPDATE ON search BEGIN
      INSERT INTO search_fts(search_fts, rowid, kind, ref_id, title, body)
      VALUES ('delete', old.rowid, old.kind, old.ref_id, old.title, old.body);
      INSERT INTO search_fts(rowid, kind, ref_id, title, body)
      VALUES (new.rowid, new.kind, new.ref_id, new.title, new.body);
    END;

    -- User-defined custom entity types: registry + instances. `fields` is
    -- JSON TEXT (json_extract queries), not Postgres JSONB.
    CREATE TABLE IF NOT EXISTS entity_types (
      id          TEXT PRIMARY KEY,
      slug        TEXT NOT NULL UNIQUE,
      name        TEXT NOT NULL,
      name_plural TEXT NOT NULL DEFAULT '',
      description TEXT NOT NULL DEFAULT '',
      icon        TEXT NOT NULL DEFAULT '',
      color       TEXT NOT NULL DEFAULT '',
      board_field TEXT,
      archived    BOOLEAN NOT NULL DEFAULT FALSE,
      created_by  TEXT NOT NULL,
      created_at  TEXT NOT NULL,
      updated_at  TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS entity_fields (
      id         TEXT PRIMARY KEY,
      type_id    TEXT NOT NULL,
      slug       TEXT NOT NULL,
      label      TEXT NOT NULL,
      field_type TEXT NOT NULL,
      required   BOOLEAN NOT NULL DEFAULT FALSE,
      position   BIGINT NOT NULL DEFAULT 0,
      options    TEXT NOT NULL DEFAULT '[]',
      ref_kind   TEXT,
      archived   BOOLEAN NOT NULL DEFAULT FALSE,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      UNIQUE (type_id, slug)
    );
    CREATE INDEX IF NOT EXISTS entity_fields_type ON entity_fields (type_id, position);

    CREATE TABLE IF NOT EXISTS entities (
      id              TEXT PRIMARY KEY,
      type_id         TEXT NOT NULL,
      title           TEXT NOT NULL,
      fields          TEXT NOT NULL DEFAULT '{}',
      user_scope      TEXT,
      origin_entry_id TEXT,
      created_by      TEXT NOT NULL,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS entities_type  ON entities (type_id, created_at);
    CREATE INDEX IF NOT EXISTS entities_scope ON entities (user_scope);

    -- Mail archive. Sync state lives on mail_accounts (cursor.set records);
    -- attachment BYTES live in the blockstore — blob_hash names them.
    CREATE TABLE IF NOT EXISTS mail_accounts (
      id              TEXT PRIMARY KEY,
      owner           TEXT NOT NULL,
      address         TEXT NOT NULL,
      jmap_url        TEXT NOT NULL DEFAULT '',
      jmap_username   TEXT,
      jmap_account_id TEXT NOT NULL DEFAULT '',
      cred_id         TEXT,
      email_state     TEXT,
      mailbox_state   TEXT,
      backfill_status TEXT NOT NULL DEFAULT 'pending',
      backfill_cursor TEXT,
      attempts        BIGINT NOT NULL DEFAULT 0,
      next_attempt_at TEXT,
      last_error      TEXT,
      last_synced_at  TEXT,
      last_status     TEXT,
      enabled         BOOLEAN NOT NULL DEFAULT TRUE,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL,
      UNIQUE (owner, address)
    );
    CREATE INDEX IF NOT EXISTS mail_accounts_owner ON mail_accounts (owner, address);

    CREATE TABLE IF NOT EXISTS mail_mailboxes (
      id         TEXT PRIMARY KEY,
      account_id TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
      jmap_id    TEXT NOT NULL,
      name       TEXT NOT NULL,
      role       TEXT,
      ingest     BOOLEAN NOT NULL DEFAULT FALSE,
      sort_order BIGINT NOT NULL DEFAULT 0,
      UNIQUE (account_id, jmap_id)
    );
    CREATE INDEX IF NOT EXISTS mail_mailboxes_account ON mail_mailboxes (account_id, sort_order);

    CREATE TABLE IF NOT EXISTS mail_messages (
      id               TEXT PRIMARY KEY,
      account_id       TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
      jmap_id          TEXT NOT NULL,
      jmap_thread_id   TEXT NOT NULL,
      message_id_hdr   TEXT,
      in_reply_to      TEXT,
      references_json  TEXT NOT NULL DEFAULT '[]',
      from_addr        TEXT NOT NULL DEFAULT '',
      from_name        TEXT,
      to_json          TEXT NOT NULL DEFAULT '[]',
      cc_json          TEXT NOT NULL DEFAULT '[]',
      reply_to_json    TEXT NOT NULL DEFAULT '[]',
      subject          TEXT NOT NULL DEFAULT '',
      sent_at          TEXT,
      received_at      TEXT NOT NULL,
      mailbox_ids_json TEXT NOT NULL DEFAULT '[]',
      keywords_json    TEXT NOT NULL DEFAULT '{}',
      body_text        TEXT NOT NULL DEFAULT '',
      body_source      TEXT NOT NULL DEFAULT 'plain',
      snippet          TEXT NOT NULL DEFAULT '',
      size             BIGINT NOT NULL DEFAULT 0,
      has_attachments  BOOLEAN NOT NULL DEFAULT FALSE,
      embed_state      TEXT NOT NULL DEFAULT 'pending',
      user_scope       TEXT NOT NULL,
      deleted_at       TEXT,
      created_at       TEXT NOT NULL,
      updated_at       TEXT NOT NULL,
      UNIQUE (account_id, jmap_id)
    );
    CREATE INDEX IF NOT EXISTS mail_messages_scope_received ON mail_messages (user_scope, received_at DESC);
    CREATE INDEX IF NOT EXISTS mail_messages_account_thread ON mail_messages (account_id, jmap_thread_id);
    CREATE INDEX IF NOT EXISTS mail_messages_message_id ON mail_messages (message_id_hdr);
    CREATE INDEX IF NOT EXISTS mail_messages_subject ON mail_messages (subject);
    CREATE INDEX IF NOT EXISTS mail_messages_embed_pending
      ON mail_messages (account_id, received_at DESC)
      WHERE embed_state = 'pending' AND deleted_at IS NULL;

    -- Attachment metadata only; bytes live in the blockstore under blob_hash.
    -- The unique expression index reproduces Postgres UNIQUE NULLS NOT
    -- DISTINCT (message_id, jmap_blob_id, content_id).
    CREATE TABLE IF NOT EXISTS mail_attachments (
      id             TEXT PRIMARY KEY,
      message_id     TEXT NOT NULL REFERENCES mail_messages(id) ON DELETE CASCADE,
      blob_hash      TEXT,
      jmap_blob_id   TEXT NOT NULL DEFAULT '',
      filename       TEXT NOT NULL DEFAULT '',
      mime           TEXT NOT NULL DEFAULT 'application/octet-stream',
      size           BIGINT NOT NULL DEFAULT 0,
      content_id     TEXT,
      disposition    TEXT,
      skipped_reason TEXT,
      created_at     TEXT NOT NULL
    );
    CREATE UNIQUE INDEX IF NOT EXISTS mail_attachments_dedup
      ON mail_attachments (message_id, jmap_blob_id, COALESCE(content_id, ''));
    CREATE INDEX IF NOT EXISTS mail_attachments_message ON mail_attachments (message_id);
    CREATE INDEX IF NOT EXISTS mail_attachments_blob ON mail_attachments (blob_hash);

    -- Identifier remapping (fold-owned; projects `alias` records). The 1.7
    -- importer emits these for re-keyed blob hashes.
    CREATE TABLE IF NOT EXISTS aliases (
      namespace  TEXT NOT NULL,
      "from"     TEXT NOT NULL,
      "to"       TEXT NOT NULL,
      created_at TEXT NOT NULL,
      PRIMARY KEY (namespace, "from")
    );

    -- Per-device fold watermark: the highest op-log seq applied.
    CREATE TABLE IF NOT EXISTS fold_meta (
      device      TEXT PRIMARY KEY,
      applied_seq INTEGER NOT NULL
    );
"#;
