//! `/events` ... Server-Sent Events stream of hive activity.
//!
//! Subscribes to the broadcast channel on [`AppState::emitter`] and re-emits
//! each [`HiveEvent`] as an SSE frame. Optional `?since=<ISO-8601>` query param
//! backfills from the on-disk events.log before joining the live stream so
//! late-arriving clients don't miss anything between reconnects.
//!
//! Backfill uses the hive.py compatible jsonl shape (`{ts, table, op, id}`).
//! Live frames carry the richer [`HiveEvent`] (kind + extra).

use std::convert::Infallible;
use std::io::{BufRead, BufReader};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::get;
use futures_util::stream::{Stream, StreamExt};
use serde::Deserialize;
use tokio_stream::wrappers::BroadcastStream;
use uuid::Uuid;

use crate::state::{AppState, HiveEvent};

pub fn router() -> Router<AppState> {
    Router::new().route("/events", get(stream))
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    /// ISO-8601 timestamp. If set, replay log entries with `ts >= since`
    /// before joining the live broadcast.
    since: Option<String>,
    /// Cap on backfill entries (default 200, max 1000) so a huge log doesn't
    /// blow up a client that just asked for "everything today".
    #[serde(default)]
    limit: Option<usize>,
}

async fn stream(
    State(state): State<AppState>,
    Query(q): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let backfill = match q.since.as_deref() {
        Some(since) => read_backfill(
            state.emitter.log_path(),
            since,
            q.limit.unwrap_or(200).min(1000),
        ),
        None => Vec::new(),
    };

    let rx = state.emitter.subscribe();
    let live = BroadcastStream::new(rx).filter_map(|res| async move {
        match res {
            Ok(ev) => Some(to_sse(&ev)),
            Err(tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(n)) => {
                tracing::warn!(skipped = n, "sse client lagged broadcast channel");
                None
            }
        }
    });

    let backfill_stream = futures_util::stream::iter(backfill.into_iter().map(Ok::<_, Infallible>));
    let live_stream = live.map(Ok::<_, Infallible>);
    let combined = backfill_stream.chain(live_stream);

    Sse::new(combined).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keepalive"),
    )
}

fn to_sse(ev: &HiveEvent) -> Event {
    // Best-effort serialize; fallback to a minimal event if JSON fails (it shouldn't).
    let payload = serde_json::to_string(ev).unwrap_or_else(|_| {
        format!(
            r#"{{"kind":"{}","source_table":"{}","source_id":"{}","ts":"{}"}}"#,
            ev.kind, ev.source_table, ev.source_id, ev.ts
        )
    });
    Event::default().event(ev.kind.clone()).data(payload)
}

/// Read the on-disk events.log and replay entries with ts >= `since`.
///
/// hive.py writes the minimal `{ts, table, op, id}` shape ... we synthesize a
/// [`HiveEvent`] with `kind = "<table>.replay"` so backfill is distinguishable
/// from live events. Failures are logged + swallowed; backfill is best-effort.
fn read_backfill(path: &std::path::Path, since: &str, limit: usize) -> Vec<Event> {
    let Ok(file) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let reader = BufReader::new(file);
    let mut out: Vec<Event> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let ts = parsed.get("ts").and_then(|v| v.as_str()).unwrap_or("");
        if ts < since {
            continue;
        }
        let table = parsed.get("table").and_then(|v| v.as_str()).unwrap_or("");
        // Post task-5 the on-disk log emits id as the canonical hyphenated
        // UUIDv7 string. Skip malformed rows rather than fail the backfill.
        let Some(id_str) = parsed.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(id_str) else {
            continue;
        };
        let kind = format!("{}.replay", normalize_table(table));
        let ev = HiveEvent {
            kind: kind.clone(),
            source_table: table.to_string(),
            source_id: id,
            ts: ts.to_string(),
            extra: serde_json::Value::Null,
        };
        out.push(to_sse(&ev));
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Map raw table name to the event-kind prefix.
///
/// `tasks` -> `task`, `journal_entries` -> `journal`, `messages` -> `message`.
/// Anything else passes through untouched.
fn normalize_table(table: &str) -> &str {
    match table {
        "tasks" => "task",
        "journal_entries" => "journal",
        "messages" => "message",
        other => other,
    }
}
