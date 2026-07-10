// Events list/get (store.ts `events`). Creates are entity.create records.

use anyhow::Result;
use hive_shared::EventItem;
use rusqlite::OptionalExtension;
use serde_json::json;

use super::{json_vec, new_id, now_iso, to_json, Draft, Store};

/// Inputs for the internal creation path (store.ts `events.create` input shape).
#[derive(Debug, Clone, Default)]
pub struct EventCreate {
    pub title: String,
    pub body: String,
    pub at: Option<String>,
    pub tags: Vec<String>,
    pub assignees: Vec<String>,
    pub origin_entry_id: Option<String>,
    pub anchor_text: Option<String>,
}

impl Store {
    pub async fn events_list(&self) -> Result<Vec<EventItem>> {
        self.run(|core| {
            let mut stmt = core
                .conn()
                .prepare("SELECT * FROM events ORDER BY COALESCE(at, created_at) DESC")?;
            let rows = stmt.query_map([], row_to_event)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
    }

    pub async fn events_get(&self, event_id: &str) -> Result<Option<EventItem>> {
        let event_id = event_id.to_string();
        self.run(move |core| {
            Ok(core
                .conn()
                .query_row(
                    "SELECT * FROM events WHERE id = ?1",
                    rusqlite::params![event_id],
                    row_to_event,
                )
                .optional()?)
        })
        .await
    }

    pub async fn events_create(&self, input: EventCreate, actor: &str) -> Result<EventItem> {
        let actor_s = actor.to_string();
        let e = self
            .run(move |core| {
                let (e, draft) = event_create_plan(input, &actor_s);
                core.commit(vec![draft])?;
                Ok(e)
            })
            .await?;
        self.emit(
            "event.created",
            actor,
            json!({"id": e.id, "title": e.title}),
        )
        .await?;
        Ok(e)
    }
}

/// The entity.create payload for one event (also journal.append's `emerged`
/// element shape).
pub(crate) fn event_create_payload(e: &EventItem) -> serde_json::Value {
    json!({"kind": "event", "id": e.id, "fields": {
        "title": e.title, "body": e.body, "at": e.at,
        "tags": to_json(&e.tags), "assignees": to_json(&e.assignees),
        "origin_entry_id": e.origin_entry_id, "anchor_text": e.anchor_text,
        "created_at": e.created_at,
    }})
}

pub(crate) fn event_create_plan(input: EventCreate, actor: &str) -> (EventItem, Draft) {
    let e = EventItem {
        id: new_id("evt"),
        title: input.title,
        body: input.body,
        at: input.at,
        tags: input.tags,
        assignees: input.assignees,
        origin_entry_id: input.origin_entry_id,
        anchor_text: input.anchor_text,
        created_at: now_iso(),
    };
    let draft = Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        actor,
        &e.created_at,
        event_create_payload(&e),
    );
    (e, draft)
}

pub(crate) fn row_to_event(r: &rusqlite::Row) -> rusqlite::Result<EventItem> {
    Ok(EventItem {
        id: r.get("id")?,
        title: r.get("title")?,
        body: r.get("body")?,
        at: r.get("at")?,
        tags: json_vec(r.get::<_, String>("tags")?.as_str()),
        assignees: json_vec(r.get::<_, String>("assignees")?.as_str()),
        origin_entry_id: r.get("origin_entry_id")?,
        anchor_text: r.get("anchor_text")?,
        created_at: r.get("created_at")?,
    })
}
