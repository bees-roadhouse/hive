use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use hive_db::PgPool;
use tokio::sync::broadcast;

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub emitter: EventEmitter,
}

/// Canonical event payload for the SSE stream + downstream subscribers.
///
/// `kind` is the high-level event name (e.g. `task.created`, `journal.created`,
/// `message.sent`). `source_table` + `source_id` mirror the on-disk events.log
/// format hive.py writes (`tasks`, `journal_entries`, `messages`) so `hive listen`
/// stays compatible. `extra` is freeform per-event context (title, sender_ai,
/// recipient_ai, owner, etc.) ... handlers fill what makes sense.
#[derive(Clone, Debug, serde::Serialize)]
pub struct HiveEvent {
    pub kind: String,
    pub source_table: String,
    pub source_id: i64,
    pub ts: String,
    #[serde(skip_serializing_if = "serde_json::Value::is_null")]
    pub extra: serde_json::Value,
}

impl HiveEvent {
    pub fn now(kind: impl Into<String>, source_table: impl Into<String>, source_id: i64) -> Self {
        Self {
            kind: kind.into(),
            source_table: source_table.into(),
            source_id,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            extra: serde_json::Value::Null,
        }
    }

    pub fn with_extra(mut self, extra: serde_json::Value) -> Self {
        self.extra = extra;
        self
    }
}

/// Broadcast + jsonl-append fan-out for hive events.
///
/// `broadcast::Sender::send` returns Err only when no receivers are subscribed
/// ... that's a normal idle state, not an error, so we ignore it.
///
/// The on-disk format we append is the minimal hive.py shape
/// (`{ts, table, op, id}`) so the existing `hive listen` tail-follower keeps
/// working. The full HiveEvent (kind + extra) only travels over the broadcast
/// channel ... consumers that want the richer shape subscribe via SSE.
#[derive(Clone)]
pub struct EventEmitter {
    inner: Arc<EmitterInner>,
}

struct EmitterInner {
    tx: broadcast::Sender<HiveEvent>,
    log_path: PathBuf,
}

impl EventEmitter {
    pub fn new(log_path: PathBuf) -> Self {
        let (tx, _rx) = broadcast::channel(256);
        Self {
            inner: Arc::new(EmitterInner { tx, log_path }),
        }
    }

    pub fn emit(&self, event: HiveEvent) {
        if let Err(e) = self.append_jsonl(&event) {
            tracing::warn!(
                error = %e,
                path = %self.inner.log_path.display(),
                "events.log append failed; broadcast still fired"
            );
        }
        let _ = self.inner.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<HiveEvent> {
        self.inner.tx.subscribe()
    }

    pub fn log_path(&self) -> &PathBuf {
        &self.inner.log_path
    }

    fn append_jsonl(&self, event: &HiveEvent) -> std::io::Result<()> {
        // Match hive.py's _append_event line shape exactly: {ts, table, op, id}.
        // op is always "insert" today; if we later emit updates/deletes we'll
        // thread the op into HiveEvent.
        let line = serde_json::json!({
            "ts": event.ts,
            "table": event.source_table,
            "op": "insert",
            "id": event.source_id,
        });
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.inner.log_path)?;
        writeln!(f, "{}", line)
    }
}
