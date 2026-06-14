// The store — parity port of packages/api/src/store.ts, split per resource so
// each area can evolve independently. Every module is an `impl Store` block;
// the struct itself is just the pool plus the in-process SSE bus (Node's
// bus.ts), so `emit()` both persists the wire event and fans it to listeners.

use anyhow::Result;
use hive_shared::WireEvent;
use sqlx::SqlitePool;
use tokio::sync::broadcast;

pub mod actors;
pub mod config;
pub mod dashboard;
pub mod decisions;
pub mod events;
pub mod identities;
pub mod import;
pub mod inbox;
pub mod journal;
pub mod links;
pub mod oauth;
pub mod outbox;
pub mod people;
pub mod phases;
pub mod profile;
pub mod projects;
pub mod recall;
pub mod search;
pub mod semantic;
pub mod sessions;
pub mod shares;
pub mod sources;
pub mod tasks;
pub mod tokens;
pub mod topics;
pub mod users;
pub mod workerstatus;

pub use crate::auth::now_iso;

#[derive(Clone)]
pub struct Store {
    db: SqlitePool,
    bus: broadcast::Sender<WireEvent>,
}

impl Store {
    pub fn new(db: SqlitePool) -> Self {
        let (bus, _) = broadcast::channel(1024);
        Self { db, bus }
    }

    pub fn db(&self) -> &SqlitePool {
        &self.db
    }

    /// Subscribe to live wire events (the SSE stream's feed).
    pub fn subscribe(&self) -> broadcast::Receiver<WireEvent> {
        self.bus.subscribe()
    }

    /// Append a wire event and fan it out to SSE subscribers (store.emit + bus.publish).
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
        sqlx::query(
            "INSERT INTO wire (id, kind, actor, payload, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&ev.id)
        .bind(&ev.kind)
        .bind(&ev.actor)
        .bind(ev.payload.to_string())
        .bind(&ev.created_at)
        .execute(&self.db)
        .await?;
        // A lagging/absent subscriber must never fail the mutation path.
        let _ = self.bus.send(ev.clone());
        Ok(ev)
    }

    /// The wire log, newest first.
    pub async fn wire_log(&self, limit: i64) -> Result<Vec<WireEvent>> {
        let rows = sqlx::query_as::<_, WireRow>(
            "SELECT id, kind, actor, payload, created_at FROM wire ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(WireRow::into_event).collect())
    }
}

#[derive(sqlx::FromRow)]
struct WireRow {
    id: String,
    kind: String,
    actor: String,
    payload: String,
    created_at: String,
}

impl WireRow {
    fn into_event(self) -> WireEvent {
        WireEvent {
            id: self.id,
            kind: self.kind,
            actor: self.actor,
            payload: serde_json::from_str(&self.payload).unwrap_or(serde_json::Value::Null),
            created_at: self.created_at,
        }
    }
}

/// `prefix_<nanoid(12)>` — the Node id() helper.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", nanoid::nanoid!(12))
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
