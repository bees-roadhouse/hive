use std::str::FromStr;

use hive_db::enums::{Ai, Author, Owner, Severity, TaskStatus};
use hive_db::queries::{journal, notes, search, tasks, wire};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::auth::claims::{Principal, ResolvedPermissions};
use crate::error::ApiError;
use crate::routes::journal::assign_missing_task_block_ids;
use crate::state::{AppState, HiveEvent};

pub fn list_definitions() -> Value {
    json!({ "tools": tool_definitions() })
}

pub async fn call(
    state: &AppState,
    principal: Option<&Principal>,
    perms: &ResolvedPermissions,
    params: &Value,
) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "tools/call requires params.name".to_string())?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "journal_add" => {
            require_scope(perms, "journal.write")?;
            journal_add(state, &args).await.map(tool_result)
        }
        "journal_list" => {
            require_scope(perms, "journal.read")?;
            journal_list(state, principal, &args).await.map(tool_result)
        }
        "journal_search" => {
            require_scope(perms, "journal.read")?;
            journal_search(state, &args).await.map(tool_result)
        }
        "journal_get" => {
            require_scope(perms, "journal.read")?;
            journal_get(state, &args).await.map(tool_result)
        }
        "tasks_list" => {
            require_scope(perms, "tasks.read")?;
            tasks_list(state, &args).await.map(tool_result)
        }
        "notes_list" => {
            require_scope(perms, "notes.read")?;
            notes_list(state, &args).await.map(tool_result)
        }
        "wire_list" => {
            require_scope(perms, "wire.read")?;
            wire_list(state, &args).await.map(tool_result)
        }
        "wire_ack" => {
            require_scope(perms, "wire.read")?;
            wire_ack(state, &args).await.map(tool_result)
        }
        "search" => {
            require_scope(perms, "journal.read")?;
            require_scope(perms, "notes.read")?;
            combined_search(state, &args).await.map(tool_result)
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

fn require_scope(perms: &ResolvedPermissions, scope: &str) -> Result<(), String> {
    if perms.is_admin || perms.has_scope(scope) || perms.has_scope("*") {
        Ok(())
    } else {
        Err(format!("missing scope: {scope}"))
    }
}

fn tool_result(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    })
}

fn to_tool_err(e: ApiError) -> String {
    e.to_string()
}

async fn journal_add(state: &AppState, args: &Value) -> Result<Value, String> {
    let body = required_str(args, "body")?;
    let ai_str = args
        .get("ai")
        .and_then(Value::as_str)
        .unwrap_or("pia");
    let ai = Ai::from_str(ai_str).map_err(|e| to_tool_err(e.into()))?;
    let entry_body = assign_missing_task_block_ids(&body);
    let date = optional_str(args, "date").unwrap_or_else(|| {
        chrono::Local::now()
            .date_naive()
            .format("%Y-%m-%d")
            .to_string()
    });
    let title = optional_str(args, "title");
    let tags = optional_str(args, "tags");

    let e = journal::add(
        &state.pool,
        ai,
        &date,
        title.as_deref(),
        &entry_body,
        tags.as_deref(),
    )
    .await
    .map_err(|e| to_tool_err(e.into()))?;

    state.emitter.emit(
        HiveEvent::now("journal.created", "journal_entries", e.id).with_extra(json!({
            "ai": e.ai,
            "title": e.title,
            "entry_date": e.entry_date,
            "tags": e.tags,
        })),
    );
    crate::mentions::project_body(&state.pool, "journal_entries", e.id, &e.body).await;

    serde_json::to_value(e).map_err(|e| e.to_string())
}

async fn journal_list(
    state: &AppState,
    principal: Option<&Principal>,
    args: &Value,
) -> Result<Value, String> {
    let ai = optional_enum::<Ai>(args, "ai")?;
    let filters = journal::ListFilters {
        ai,
        from_date: optional_str(args, "from"),
        to_date: optional_str(args, "to"),
        tag: optional_str(args, "tag"),
        limit: optional_i64(args, "limit"),
    };
    let mut tx = state.rls_begin(principal).await.map_err(|e| to_tool_err(e.into()))?;
    let rows = journal::list_in(&mut *tx, &filters)
        .await
        .map_err(|e| to_tool_err(e.into()))?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(json!(rows))
}

async fn journal_search(state: &AppState, args: &Value) -> Result<Value, String> {
    let q = required_str(args, "q")?;
    let limit = optional_i64(args, "limit").unwrap_or(20);
    let hits = search::journal(&state.pool, &q, limit)
        .await
        .map_err(|e| to_tool_err(e.into()))?;
    Ok(json!(hits))
}

async fn journal_get(state: &AppState, args: &Value) -> Result<Value, String> {
    let id_or_slug = required_str(args, "id_or_slug")?;
    if let Ok(id) = Uuid::parse_str(&id_or_slug)
        && let Some(e) = journal::get(&state.pool, id).await.map_err(|e| to_tool_err(e.into()))?
    {
        return serde_json::to_value(e).map_err(|e| e.to_string());
    }
    let e = journal::find_by_slug(&state.pool, &id_or_slug)
        .await
        .map_err(|e| to_tool_err(e.into()))?
        .ok_or_else(|| format!("journal entry not found: {id_or_slug}"))?;
    serde_json::to_value(e).map_err(|e| e.to_string())
}

async fn tasks_list(state: &AppState, args: &Value) -> Result<Value, String> {
    let owner = optional_enum::<Owner>(args, "owner")?;
    let status = optional_enum::<TaskStatus>(args, "status")?;
    let filters = tasks::ListFilters {
        project: optional_str(args, "project"),
        owner,
        status,
        all: args.get("all").and_then(Value::as_bool).unwrap_or(false),
    };
    let rows = tasks::list(&state.pool, &filters).await.map_err(|e| to_tool_err(e.into()))?;
    Ok(json!(rows))
}

async fn notes_list(state: &AppState, args: &Value) -> Result<Value, String> {
    let author = optional_enum::<Author>(args, "author")?;
    let filters = notes::ListFilters {
        author,
        project: optional_str(args, "project"),
        tag: optional_str(args, "tag"),
        limit: optional_i64(args, "limit"),
    };
    let rows = notes::list(&state.pool, &filters).await.map_err(|e| to_tool_err(e.into()))?;
    Ok(json!(rows))
}

async fn wire_list(state: &AppState, args: &Value) -> Result<Value, String> {
    let severity = optional_enum::<Severity>(args, "severity")?;
    let filters = wire::ListFilters {
        source: optional_str(args, "source"),
        severity,
        unacknowledged: args
            .get("unacknowledged")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        limit: optional_i64(args, "limit"),
    };
    let rows = wire::list(&state.pool, &filters).await.map_err(|e| to_tool_err(e.into()))?;
    Ok(json!(rows))
}

async fn wire_ack(state: &AppState, args: &Value) -> Result<Value, String> {
    let id_str = required_str(args, "id")?;
    let id = Uuid::parse_str(&id_str).map_err(|e| format!("invalid id: {e}"))?;
    wire::ack(&state.pool, id).await.map_err(|e| to_tool_err(e.into()))?;
    state
        .emitter
        .emit(HiveEvent::now("wire.acked", "wire_events", id));
    Ok(json!({ "acknowledged": true, "id": id }))
}

async fn combined_search(state: &AppState, args: &Value) -> Result<Value, String> {
    let q = required_str(args, "q")?;
    let limit = optional_i64(args, "limit").unwrap_or(10);
    let journal_hits = search::journal(&state.pool, &q, limit)
        .await
        .map_err(|e| to_tool_err(e.into()))?;
    let note_hits = search::notes(&state.pool, &q, limit)
        .await
        .map_err(|e| to_tool_err(e.into()))?;
    Ok(json!({
        "journal": journal_hits,
        "notes": note_hits,
    }))
}

fn required_str(args: &Value, field: &str) -> Result<String, String> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required argument: {field}"))
}

fn optional_str(args: &Value, field: &str) -> Option<String> {
    args.get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn optional_i64(args: &Value, field: &str) -> Option<i64> {
    args.get(field).and_then(|v| v.as_i64())
}

fn optional_enum<T>(args: &Value, field: &str) -> Result<Option<T>, String>
where
    T: FromStr,
    T::Err: std::fmt::Display,
{
    match args.get(field).and_then(Value::as_str) {
        Some(s) => T::from_str(s).map(Some).map_err(|e| e.to_string()),
        None => Ok(None),
    }
}

fn tool_definitions() -> Vec<Value> {
    vec![
    json!({
        "name": "journal_add",
        "description": "Create a journal entry (canonical write surface). Tasks, notes, and links project from the body.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "ai": { "type": "string", "description": "Author: pia, apis, cera, nate, maggie (default pia)" },
                "body": { "type": "string", "description": "Markdown body with checkboxes and [[[note ...]]] blocks" },
                "title": { "type": "string" },
                "date": { "type": "string", "description": "YYYY-MM-DD (default today)" },
                "tags": { "type": "string", "description": "Comma-separated tags" }
            },
            "required": ["body"]
        }
    }),
    json!({
        "name": "journal_list",
        "description": "List journal entries with optional filters.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "ai": { "type": "string" },
                "from": { "type": "string", "description": "YYYY-MM-DD" },
                "to": { "type": "string", "description": "YYYY-MM-DD" },
                "tag": { "type": "string" },
                "limit": { "type": "integer" }
            }
        }
    }),
    json!({
        "name": "journal_search",
        "description": "Full-text search journal entries.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "q": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["q"]
        }
    }),
    json!({
        "name": "journal_get",
        "description": "Fetch one journal entry by UUID or slug.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id_or_slug": { "type": "string" }
            },
            "required": ["id_or_slug"]
        }
    }),
    json!({
        "name": "tasks_list",
        "description": "List tasks (read-only under journal-canonical enforce mode).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "project": { "type": "string" },
                "owner": { "type": "string" },
                "status": { "type": "string" },
                "all": { "type": "boolean" }
            }
        }
    }),
    json!({
        "name": "notes_list",
        "description": "List notes (read-only under journal-canonical enforce mode).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "author": { "type": "string" },
                "project": { "type": "string" },
                "tag": { "type": "string" },
                "limit": { "type": "integer" }
            }
        }
    }),
    json!({
        "name": "wire_list",
        "description": "List wire events (CVE, outages, RSS ingest).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "source": { "type": "string" },
                "severity": { "type": "string" },
                "unacknowledged": { "type": "boolean" },
                "limit": { "type": "integer" }
            }
        }
    }),
    json!({
        "name": "wire_ack",
        "description": "Acknowledge a wire event by UUID.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "Wire event UUID" }
            },
            "required": ["id"]
        }
    }),
    json!({
        "name": "search",
        "description": "Combined full-text search across journal entries and notes.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "q": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["q"]
        }
    }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_catalog_has_journal_add() {
        let defs = tool_definitions();
        let names: Vec<_> = defs
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str))
            .collect();
        assert!(names.contains(&"journal_add"));
        assert_eq!(names.len(), 9);
    }

    #[test]
    fn scope_gate_denies_empty() {
        let perms = ResolvedPermissions::none();
        assert!(require_scope(&perms, "journal.write").is_err());
    }

    #[test]
    fn scope_gate_allows_wildcard() {
        let perms = ResolvedPermissions::full();
        assert!(require_scope(&perms, "journal.write").is_ok());
    }
}
