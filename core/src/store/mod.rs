// The store — the PR 1.6 cutover shape. The public surface of every module
// (`pub async fn` per resource) is unchanged from the Postgres era; under it,
// every call ships a closure to the single writer thread that owns the
// rusqlite connection, the op-log writer, and the blockstore (store/core.rs).
// Writes are record batches through the fold; reads are plain SQL on the
// derived index. `emit()` is broadcast-bus-only: the wire table died with
// Postgres — the op log is its durable successor, and a bounded in-memory
// ring keeps "recent activity" (dashboard) and feed-ingest dedup working
// within a process lifetime.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use hive_embed::Embedder;
use hive_shared::WireEvent;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::keys::KeySource;

pub(crate) mod core;

pub mod actors;
pub mod artifacts;
pub mod cc_credentials;
pub mod config;
pub mod contacts;
pub mod custom_entities;
pub mod dashboard;
pub mod decisions;
pub mod embed_backfill;
pub mod entity_types;
pub mod entity_validation;
pub mod events;
pub mod identities;
pub mod import;
pub mod inbox;
pub mod journal;
pub mod links;
pub mod mail;
pub mod maintenance;
pub mod outbox;
pub mod people;
pub mod phases;
pub mod profile;
pub mod projects;
pub mod recall;
pub mod search;
pub mod semantic;
pub mod sources;
pub mod tasks;
pub mod topics;
pub mod workerstatus;

pub(crate) use self::core::{Core, Draft};

/// Current instant in the exact shape JS `new Date().toISOString()` produces —
/// millisecond precision, trailing `Z` — the same 24-char shape the op-log
/// envelope freezes (lexicographic order is chronological order).
pub fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// One queued unit of work for the writer thread.
type Job = Box<dyn FnOnce(&mut Core) + Send>;

/// A snapshot of the embedder actually running (see `Store::embedder_state`).
/// Every field is the truth about the live engine — the Settings pane renders
/// exactly these, never the persisted-but-pending choice.
#[derive(Debug, Clone)]
pub struct EmbedderState {
    /// "native" | "ollama" | "hash".
    pub backend: String,
    /// "CPU" | "CUDA" | "ROCm" | "Ollama" | "hash".
    pub device: String,
    /// The model name stamped on stored vectors.
    pub model: String,
    /// Degraded to the hash fallback (a real model failed to load).
    pub latched: bool,
}

/// How many recent wire events the in-memory ring retains.
const WIRE_RING_CAP: usize = 1024;

#[derive(Clone)]
pub struct Store {
    jobs: mpsc::UnboundedSender<Job>,
    bus: broadcast::Sender<WireEvent>,
    wire: Arc<Mutex<VecDeque<WireEvent>>>,
    embedder: Arc<dyn Embedder>,
    device: String,
    data_dir: PathBuf,
    /// The writer thread's handle, for `shutdown` (reopen-the-same-dir tests
    /// and an orderly app exit).
    writer: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl Store {
    /// Open (or create) a store under `data_dir`. Spawns the writer thread;
    /// heals any unfolded op-log tail before the first command runs.
    pub fn new(
        data_dir: &Path,
        keys: Arc<dyn KeySource + Send + Sync>,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Store> {
        let mut core = core::open_core(data_dir, keys)?;
        let device = core.device.clone();
        let (jobs, mut rx) = mpsc::unbounded_channel::<Job>();
        let writer = std::thread::Builder::new()
            .name("hive-store-writer".to_string())
            .spawn(move || {
                while let Some(job) = rx.blocking_recv() {
                    job(&mut core);
                }
            })
            .map_err(|e| anyhow!("spawning the store writer thread: {e}"))?;
        let (bus, _) = broadcast::channel(1024);
        Ok(Store {
            jobs,
            bus,
            wire: Arc::new(Mutex::new(VecDeque::with_capacity(64))),
            embedder,
            device,
            data_dir: data_dir.to_path_buf(),
            writer: Arc::new(Mutex::new(Some(writer))),
        })
    }

    /// Orderly close: drain the queue, stop the writer thread, and wait for
    /// it to release the index/log files. Only meaningful on the last clone —
    /// other live clones keep the thread running (join would block, so this
    /// hands the handle back untouched in that case).
    pub async fn shutdown(self) -> Result<()> {
        let Store { jobs, writer, .. } = self;
        drop(jobs);
        let handle = writer.lock().expect("writer handle poisoned").take();
        if let Some(handle) = handle {
            tokio::task::spawn_blocking(move || {
                handle
                    .join()
                    .map_err(|_| anyhow!("store writer thread panicked"))
            })
            .await??;
        }
        Ok(())
    }

    /// This installation's device id (the op-log author device).
    pub fn device(&self) -> &str {
        &self.device
    }

    /// The data dir this store was opened on.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The injected embedding engine.
    pub(crate) fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
    }

    /// The TRUTHFUL running-engine state for the Settings readout: the actual
    /// backend family, the actual accelerator, the model stamped on vectors,
    /// and whether it has degraded to the hash fallback. Callers must show THIS,
    /// never the user's saved-but-not-yet-running choice. `latched` means a real
    /// model was configured but failed to load, so search is keyword-only until
    /// the next launch.
    pub fn embedder_state(&self) -> EmbedderState {
        EmbedderState {
            backend: self.embedder.backend(),
            device: self.embedder.device(),
            model: self.embedder.model(),
            latched: self.embedder.latched(),
        }
    }

    /// Run one closure on the writer thread and await its result — the
    /// mpsc-command / oneshot-reply seam every store method rides.
    pub(crate) async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Core) -> Result<T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Box::new(move |core| {
                let _ = tx.send(f(core));
            }))
            .map_err(|_| anyhow!("store writer thread is gone"))?;
        rx.await
            .map_err(|_| anyhow!("store writer thread dropped the reply"))?
    }

    /// Subscribe to live wire events (the app shell's notification feed).
    pub fn subscribe(&self) -> broadcast::Receiver<WireEvent> {
        self.bus.subscribe()
    }

    /// Publish a wire event: in-memory ring + broadcast fan-out. No table —
    /// durable history is the op log's job now.
    pub async fn emit(
        &self,
        kind: &str,
        actor: &str,
        payload: serde_json::Value,
    ) -> Result<WireEvent> {
        let ev = WireEvent {
            id: new_id("wire"),
            kind: kind.to_string(),
            actor: actor.to_string(),
            payload,
            created_at: now_iso(),
        };
        {
            let mut ring = self.wire.lock().expect("wire ring poisoned");
            if ring.len() == WIRE_RING_CAP {
                ring.pop_front();
            }
            ring.push_back(ev.clone());
        }
        // A lagging/absent subscriber must never fail the mutation path.
        let _ = self.bus.send(ev.clone());
        Ok(ev)
    }

    /// Recent wire events, newest first (this process's ring — session-scoped
    /// by design since the wire table died).
    pub async fn wire_log(&self, limit: i64) -> Result<Vec<WireEvent>> {
        let ring = self.wire.lock().expect("wire ring poisoned");
        Ok(ring
            .iter()
            .rev()
            .take(limit.max(0) as usize)
            .cloned()
            .collect())
    }

    /// Ring probe: does any retained event of `kind` carry `guid` in its
    /// payload? (Feed-ingest dedup — best-effort within process lifetime.)
    pub(crate) fn wire_ring_has_guid(&self, kind: &str, guid: &str) -> bool {
        let ring = self.wire.lock().expect("wire ring poisoned");
        ring.iter().any(|ev| {
            ev.kind == kind && ev.payload.get("guid").and_then(|v| v.as_str()) == Some(guid)
        })
    }

    /// Rewrite ring authorship (actor merge) — returns how many events moved.
    pub(crate) fn wire_ring_reassign(&self, from: &str, to: &str) -> i64 {
        let mut ring = self.wire.lock().expect("wire ring poisoned");
        let mut n = 0;
        for ev in ring.iter_mut() {
            if ev.actor == from {
                ev.actor = to.to_string();
                n += 1;
            }
        }
        n
    }

    /// Count ring events authored by `actor` (preview accounting).
    pub(crate) fn wire_ring_count(&self, actor: &str) -> i64 {
        let ring = self.wire.lock().expect("wire ring poisoned");
        ring.iter().filter(|ev| ev.actor == actor).count() as i64
    }

    /// Drop ring events authored by `actor` (actor delete) — returns count.
    pub(crate) fn wire_ring_purge(&self, actor: &str) -> i64 {
        let mut ring = self.wire.lock().expect("wire ring poisoned");
        let before = ring.len();
        ring.retain(|ev| ev.actor != actor);
        (before - ring.len()) as i64
    }

    // ── diagnostics / test seams ────────────────────────────────────────────

    /// Canonical text dump of every fold-owned table (fixed table order,
    /// primary-key row order, stable rendering — see
    /// `SqliteIndex::canonical_dump`). The rebuild-verification oracle: two
    /// stores folded from the same op log must dump byte-identically.
    /// Test/diagnostic seam.
    #[doc(hidden)]
    pub async fn canonical_dump(&self) -> Result<String> {
        self.run(|core| core.index.canonical_dump()).await
    }

    /// Run one SQL statement on the derived index, binding JSON params
    /// (strings/numbers/bools/null). SELECTs return rows as JSON values;
    /// other statements return one row holding rows_affected. Test/diagnostic
    /// seam ONLY: writes through here bypass the op log and will not survive
    /// an index rebuild — production code goes through records.
    #[doc(hidden)]
    pub async fn raw_sql(
        &self,
        sql: &str,
        params: Vec<serde_json::Value>,
    ) -> Result<Vec<Vec<serde_json::Value>>> {
        let sql = sql.to_string();
        self.run(move |core| {
            let conn = core.conn();
            let mut stmt = conn.prepare(&sql)?;
            let binds: Vec<Box<dyn rusqlite::types::ToSql>> = params
                .iter()
                .map(|v| -> Box<dyn rusqlite::types::ToSql> {
                    match v {
                        serde_json::Value::Null => Box::new(rusqlite::types::Null),
                        serde_json::Value::Bool(b) => Box::new(*b),
                        serde_json::Value::Number(n) => {
                            if let Some(i) = n.as_i64() {
                                Box::new(i)
                            } else {
                                Box::new(n.as_f64().unwrap_or(0.0))
                            }
                        }
                        other => Box::new(
                            other
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| other.to_string()),
                        ),
                    }
                })
                .collect();
            let bind_refs = rusqlite::params_from_iter(binds.iter().map(|b| b.as_ref()));
            if stmt.column_count() == 0 {
                let n = stmt.execute(bind_refs)?;
                return Ok(vec![vec![serde_json::json!(n as i64)]]);
            }
            let ncols = stmt.column_count();
            let mut rows = stmt.query(bind_refs)?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let mut vals = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    use rusqlite::types::ValueRef;
                    vals.push(match row.get_ref(i)? {
                        ValueRef::Null => serde_json::Value::Null,
                        ValueRef::Integer(v) => serde_json::json!(v),
                        ValueRef::Real(v) => serde_json::json!(v),
                        ValueRef::Text(t) => {
                            serde_json::Value::String(String::from_utf8_lossy(t).to_string())
                        }
                        ValueRef::Blob(b) => {
                            serde_json::Value::String(data_encoding::HEXLOWER.encode(b))
                        }
                    });
                }
                out.push(vals);
            }
            Ok(out)
        })
        .await
    }

    /// Insert/replace one embedding row (and its ANN entry) directly. Test
    /// seam for crafted-vector fixtures; the production path is
    /// `backfill_embeddings`.
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_embedding_raw(
        &self,
        ref_kind: &str,
        ref_id: &str,
        chunk_idx: i64,
        model: &str,
        owner: Option<&str>,
        vec: Vec<f32>,
        hash: &str,
    ) -> Result<()> {
        let (ref_kind, ref_id, model, hash) = (
            ref_kind.to_string(),
            ref_id.to_string(),
            model.to_string(),
            hash.to_string(),
        );
        let owner = owner.map(str::to_string);
        let now = now_iso();
        self.run(move |core| {
            core.index.upsert_embedding(
                &ref_kind,
                &ref_id,
                chunk_idx,
                &model,
                owner.as_deref(),
                &vec,
                &hash,
                &now,
            )
        })
        .await
    }
}

/// `prefix_<nanoid(12)>` — the Node id() helper.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", nanoid::nanoid!(12))
}

/// `?,?,?` for n binds, or the never-matching literal Node uses when a set is empty.
pub(crate) fn placeholders_or_never(n: usize) -> String {
    if n == 0 {
        "'__never__'".to_string()
    } else {
        vec!["?"; n].join(",")
    }
}

/// Truncate to 140 chars with `…` (the Node snip default).
pub fn snip140(s: &str) -> String {
    hive_shared::snip(s, 140)
}

/// Parse a JSON-array column, tolerating legacy garbage.
pub fn json_vec(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

pub fn to_json<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "null".to_string())
}

#[cfg(test)]
mod tests {
    use super::now_iso;

    #[test]
    fn iso_format_matches_js() {
        let s = now_iso();
        assert_eq!(s.len(), 24);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[19..20], ".");
    }
}
