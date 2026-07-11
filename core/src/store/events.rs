// Events list/get/update/delete (store.ts `events`). Creates and updates are
// entity.create/entity.update records; deletes are tombstones. The fold
// maintains the row + FTS + vectors from these — over the EXISTING event
// columns only.
//
// deferred: rich recurrence (start/end, RRULE, timezone, all-day, reminders)
// and CalDAV sync need a batched fold migration (new columns + a FOLD_VERSION
// bump), deliberately deferred here so the calendar slice forces no re-embed —
// this file writes ONLY the columns the fold's EVENTS spec already knows.

use anyhow::Result;
use hive_shared::{EventItem, EventPatch};
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

    /// Edit an event over its EXISTING columns (title, body, at, tags,
    /// assignees) via an entity.update record — the fold's event path applies
    /// it and refreshes FTS. Mirrors tasks_update/custom_entities_update.
    ///
    /// The record carries ONLY changed-or-final column values and, crucially,
    /// NO `updated_at`: the events table (and the fold's EVENTS spec) has no
    /// such column, and the fold fails closed on any unknown column. `at` is a
    /// double Option — Some(None) clears it to NULL (unscheduled), Some(Some)
    /// sets it, None leaves the current value untouched.
    pub async fn events_update(
        &self,
        event_id: &str,
        patch: EventPatch,
        actor: &str,
    ) -> Result<Option<EventItem>> {
        let event_id_s = event_id.to_string();
        let actor_s = actor.to_string();
        let next = self
            .run(move |core| {
                let Some(current) = core
                    .conn()
                    .query_row(
                        "SELECT * FROM events WHERE id = ?1",
                        rusqlite::params![event_id_s],
                        row_to_event,
                    )
                    .optional()?
                else {
                    return Ok(None);
                };
                let next = EventItem {
                    title: patch.title.unwrap_or(current.title),
                    body: patch.body.unwrap_or(current.body),
                    // double Option: absent keeps, null clears, value sets.
                    at: match patch.at {
                        Some(v) => v,
                        None => current.at,
                    },
                    tags: patch.tags.unwrap_or(current.tags),
                    assignees: patch.assignees.unwrap_or(current.assignees),
                    ..current
                };
                // `at` binds SQL NULL when None (fold bind_value handles null),
                // so an explicit clear round-trips. No updated_at column exists.
                core.commit(vec![Draft::new(
                    crate::oplog::kind::ENTITY_UPDATE,
                    &actor_s,
                    &now_iso(),
                    json!({"kind": "event", "id": next.id, "fields": {
                        "title": next.title, "body": next.body, "at": next.at,
                        "tags": to_json(&next.tags), "assignees": to_json(&next.assignees),
                    }}),
                )])?;
                Ok(Some(next))
            })
            .await?;
        let Some(next) = next else { return Ok(None) };
        self.emit("event.updated", actor, json!({"id": next.id}))
            .await?;
        Ok(Some(next))
    }

    /// Delete an event: a tombstone record (the fold drops the row + FTS +
    /// vectors) plus link.remove records for every edge touching it (journal
    /// emergence links a source entry to its event with rel "anchors"; the
    /// fold's tombstone path does NOT clean links, so we do — mirroring
    /// custom_entities_delete).
    pub async fn events_delete(&self, event_id: &str, actor: &str) -> Result<Option<()>> {
        let event_id_s = event_id.to_string();
        let deleted = self
            .run(move |core| {
                let Some(current) = core
                    .conn()
                    .query_row(
                        "SELECT * FROM events WHERE id = ?1",
                        rusqlite::params![event_id_s],
                        row_to_event,
                    )
                    .optional()?
                else {
                    return Ok(None);
                };
                let ts = now_iso();
                let mut batch = vec![Draft::new(
                    crate::oplog::kind::TOMBSTONE,
                    "system",
                    &ts,
                    json!({"kind": "event", "id": current.id}),
                )];
                let link_ids: Vec<String> = {
                    let mut stmt = core
                        .conn()
                        .prepare("SELECT id FROM links WHERE source_id = ?1 OR target_id = ?1")?;
                    let rows = stmt.query_map(rusqlite::params![current.id], |r| r.get(0))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                for lid in &link_ids {
                    batch.push(super::links::link_remove_draft(lid, &ts));
                }
                core.commit(batch)?;
                Ok(Some(current))
            })
            .await?;
        let Some(current) = deleted else {
            return Ok(None);
        };
        self.emit("event.deleted", actor, json!({"id": current.id}))
            .await?;
        Ok(Some(()))
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
