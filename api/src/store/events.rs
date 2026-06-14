// Events list/get (store.ts `events`). Owned by core-stores.

use anyhow::Result;
use hive_shared::EventItem;
use serde_json::json;
use sqlx::Row;

use super::{json_vec, new_id, now_iso, to_json, Store};

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
        let rows = crate::pgq::query("SELECT * FROM events ORDER BY COALESCE(at, created_at) DESC")
            .fetch_all(self.db())
            .await?;
        rows.iter().map(row_to_event).collect()
    }

    pub async fn events_get(&self, event_id: &str) -> Result<Option<EventItem>> {
        let row = crate::pgq::query("SELECT * FROM events WHERE id = ?")
            .bind(event_id)
            .fetch_optional(self.db())
            .await?;
        row.as_ref().map(row_to_event).transpose()
    }

    pub async fn events_create(&self, input: EventCreate, actor: &str) -> Result<EventItem> {
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
        crate::pgq::query(
            "INSERT INTO events (id, title, body, at, tags, assignees, origin_entry_id, anchor_text, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&e.id)
        .bind(&e.title)
        .bind(&e.body)
        .bind(&e.at)
        .bind(to_json(&e.tags))
        .bind(to_json(&e.assignees))
        .bind(&e.origin_entry_id)
        .bind(&e.anchor_text)
        .bind(&e.created_at)
        .execute(self.db())
        .await?;
        self.index_entity("event", &e.id, &e.title, &e.body, &e.tags)
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

pub(crate) fn row_to_event(r: &sqlx::postgres::PgRow) -> Result<EventItem> {
    Ok(EventItem {
        id: r.try_get("id")?,
        title: r.try_get("title")?,
        body: r.try_get("body")?,
        at: r.try_get("at")?,
        tags: json_vec(r.try_get::<String, _>("tags")?.as_str()),
        assignees: json_vec(r.try_get::<String, _>("assignees")?.as_str()),
        origin_entry_id: r.try_get("origin_entry_id")?,
        anchor_text: r.try_get("anchor_text")?,
        created_at: r.try_get("created_at")?,
    })
}
