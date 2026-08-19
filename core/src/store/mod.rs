// The store — parity port of packages/api/src/store.ts, split per resource so
// each area can evolve independently. Every module is an `impl Store` block;
// the struct itself is just the pool plus the in-process SSE bus (Node's
// bus.ts), so `emit()` both persists the wire event and fans it to listeners.

use anyhow::Result;
use hive_shared::WireEvent;
use sqlx::PgPool;
use tokio::sync::broadcast;

pub mod actors;
pub mod artifacts;
pub mod cc_credentials;
pub mod config;
pub mod conversations;
pub mod custom_entities;
pub mod dashboard;
pub mod decisions;
pub mod embed_backfill;
pub mod entity_types;
pub mod entity_validation;
pub mod events;
pub mod identities;
pub mod identity_artifacts;
pub mod import;
pub mod inbox;
pub mod journal;
pub mod links;
pub mod mail;
pub mod maintenance;
pub mod oauth;
pub mod orgs;
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
pub mod workspaces;

/// Current instant in the exact shape JS `new Date().toISOString()` produces —
/// millisecond precision, trailing `Z` — so rows sort lexicographically next to
/// rows written by the Node API.
pub fn now_iso() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// A wire event plus the org it happened in. The bus is one in-process
/// broadcast channel shared by every connected client, and a broadcast channel
/// has no policies — so the org rides along and `subscribe` filters on it.
/// Without this the SSE stream would hand every org's mutations to every
/// listener, which is a leak RLS cannot see.
#[derive(Clone, Debug)]
pub struct ScopedEvent {
    pub org: Option<uuid::Uuid>,
    pub event: WireEvent,
}

#[derive(Clone)]
pub struct Store {
    db: PgPool,
    bus: broadcast::Sender<ScopedEvent>,
}

impl Store {
    pub fn new(db: PgPool) -> Self {
        let (bus, _) = broadcast::channel(1024);
        Self { db, bus }
    }

    pub fn db(&self) -> &PgPool {
        &self.db
    }

    /// Subscribe to live wire events for one org (the SSE stream's feed).
    /// Events from any other org are dropped before the caller sees them.
    pub fn subscribe(&self, org: uuid::Uuid) -> impl futures_core::Stream<Item = WireEvent> {
        let mut rx = self.bus.subscribe();
        async_stream::stream! {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.org == Some(org) => yield ev.event,
                    Ok(_) => continue,
                    // A slow consumer that missed events just keeps going.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }

    /// Append a wire event and fan it out to SSE subscribers (store.emit + bus.publish).
    pub async fn emit(
        &self,
        kind: &str,
        actor: &str,
        payload: serde_json::Value,
    ) -> Result<WireEvent> {
        let ev = emit_persist(&self.db, kind, actor, payload).await?;
        self.publish(ev.clone());
        Ok(ev)
    }

    /// Fan a persisted event out to SSE subscribers. Kept apart from
    /// [`emit_persist`] because a transaction's events must not reach the bus
    /// until AFTER commit — a listener must never observe an event whose row
    /// could roll back. A lagging/absent subscriber must never fail the
    /// mutation path.
    pub(crate) fn publish(&self, ev: WireEvent) {
        let _ = self.bus.send(ScopedEvent {
            org: crate::acting::current().map(|s| s.org),
            event: ev,
        });
    }

    /// Publish a transaction's events in persist order. Call only once the
    /// commit is durable.
    pub(crate) fn publish_all(&self, events: Vec<WireEvent>) {
        for ev in events {
            self.publish(ev);
        }
    }

    /// The wire log, newest first.
    pub async fn wire_log(&self, limit: i64) -> Result<Vec<WireEvent>> {
        let rows = crate::pgq::query_as::<WireRow>(
            "SELECT id, kind, actor, payload, created_at FROM wire ORDER BY created_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(WireRow::into_event).collect())
    }
}

/// The persist half of `emit`, split from the bus fan-out so a transaction can
/// hold the wire row until commit. Callers on a transaction collect the
/// returned events and `publish` them themselves once the commit lands.
pub(crate) async fn emit_persist<'e, E>(
    exec: E,
    kind: &str,
    actor: &str,
    payload: serde_json::Value,
) -> Result<WireEvent>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let ev = WireEvent {
        id: new_id("wire"),
        kind: kind.to_string(),
        actor: actor.to_string(),
        payload,
        created_at: now_iso(),
    };
    crate::pgq::query(
        "INSERT INTO wire (id, kind, actor, payload, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&ev.id)
    .bind(&ev.kind)
    .bind(&ev.actor)
    .bind(ev.payload.to_string())
    .bind(&ev.created_at)
    .execute(exec)
    .await?;
    Ok(ev)
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
