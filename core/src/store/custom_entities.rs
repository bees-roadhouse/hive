// Custom entity instances: validated JSON rows registered at the seams
// (FTS via the fold, ref fields mirrored into links records, wire events).
// Writes are entity.create/entity.update/tombstone records with kind = the
// type slug; the fold routes those to the `entities` table and maintains the
// search row from entity_fields order (searchable_text parity).

use anyhow::Result;
use hive_shared::{CustomEntity, CustomEntityPatch, EntityField, EntityTypeView, NewCustomEntity};
use rusqlite::OptionalExtension;
use serde_json::{json, Map, Value};

use super::entity_validation::{merge_fields, validate_fields, FieldIssue};
use super::{new_id, now_iso, to_json, Core, Draft, Store};

pub enum EntityWriteError {
    /// Structured validation failures → 400 with the issues array.
    Issues(Vec<FieldIssue>),
    /// Unknown type slug → 404.
    UnknownType,
    /// Archived type refuses new instances → 409.
    ArchivedType,
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for EntityWriteError {
    fn from(e: E) -> Self {
        EntityWriteError::Other(e.into())
    }
}

/// List query, routes/MCP both funnel here. Filters are equality on field
/// slugs, applied (like tasks_list) in Rust after the scoped SQL fetch.
#[derive(Debug, Clone, Default)]
pub struct EntityFilter {
    pub type_slug: String,
    pub limit: i64,
    pub offset: i64,
    pub sort: Option<String>,
    pub desc: bool,
    pub fields: Vec<(String, String)>,
}

impl Store {
    pub async fn custom_entities_list(
        &self,
        filter: &EntityFilter,
    ) -> std::result::Result<Vec<CustomEntity>, EntityWriteError> {
        let filter = filter.clone();
        let out: std::result::Result<Vec<CustomEntity>, ListError> = self
            .run(move |core| {
                let Some(ty) = super::entity_types::entity_type_get(core, &filter.type_slug)?
                else {
                    return Ok(Err(ListError::UnknownType));
                };
                // Unknown sort keys fail closed, consistent with the validation posture.
                if let Some(s) = filter.sort.as_deref() {
                    let known = matches!(s, "title" | "created_at" | "updated_at")
                        || ty.fields.iter().any(|f| f.slug == s);
                    if !known {
                        return Ok(Err(ListError::Issues(vec![FieldIssue {
                            field: s.to_string(),
                            code: "unknown_field",
                            message: format!("unknown sort field '{s}'"),
                        }])));
                    }
                }

                let limit = filter.limit.clamp(1, 500);
                let mut items: Vec<CustomEntity> = {
                    let mut stmt = core.conn().prepare(
                        "SELECT id, type_id, title, fields, user_scope, origin_entry_id, created_by, created_at, updated_at \
                         FROM entities WHERE type_id = ?1 ORDER BY created_at DESC LIMIT ?2 OFFSET ?3",
                    )?;
                    let rows = stmt.query_map(
                        rusqlite::params![ty.id, limit, filter.offset],
                        |r| row_to_entity(r, &ty.slug),
                    )?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };

                // Equality filters + sort in Rust (household scale; mirrors tasks_list).
                for (k, v) in &filter.fields {
                    items.retain(|e| match e.fields.get(k) {
                        Some(Value::String(s)) => s == v,
                        Some(Value::Bool(b)) => v.parse::<bool>().map(|pv| pv == *b).unwrap_or(false),
                        Some(Value::Number(n)) => v.parse::<f64>().ok() == n.as_f64(),
                        _ => false,
                    });
                }
                if let Some(sort) = filter.sort.as_deref() {
                    let key = |e: &CustomEntity| -> (u8, String, f64) {
                        match sort {
                            "title" => (0, e.title.to_lowercase(), 0.0),
                            "created_at" => (0, e.created_at.clone(), 0.0),
                            "updated_at" => (0, e.updated_at.clone(), 0.0),
                            slug => match e.fields.get(slug) {
                                // numbers sort numerically, everything else as text;
                                // absent values sink to the end regardless of dir.
                                Some(Value::Number(n)) => (1, String::new(), n.as_f64().unwrap_or(0.0)),
                                Some(Value::String(s)) => (0, s.to_lowercase(), 0.0),
                                Some(Value::Bool(b)) => (0, b.to_string(), 0.0),
                                _ => (2, String::new(), 0.0),
                            },
                        }
                    };
                    items.sort_by(|a, b| {
                        let (ka, sa, na) = key(a);
                        let (kb, sb, nb) = key(b);
                        let absent = ka.eq(&2).cmp(&kb.eq(&2)); // absents last, both dirs
                        let ord = sa
                            .cmp(&sb)
                            .then(na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal));
                        absent.then(if filter.desc { ord.reverse() } else { ord })
                    });
                }
                Ok(Ok(items))
            })
            .await
            .map_err(EntityWriteError::Other)?;
        out.map_err(|e| match e {
            ListError::UnknownType => EntityWriteError::UnknownType,
            ListError::Issues(i) => EntityWriteError::Issues(i),
        })
    }

    pub async fn custom_entities_get(&self, id: &str) -> Result<Option<CustomEntity>> {
        let id = id.to_string();
        self.run(move |core| custom_entity_get(core, &id)).await
    }

    pub async fn custom_entities_create(
        &self,
        input: NewCustomEntity,
        actor: &str,
        namespace_owner: Option<&str>,
    ) -> std::result::Result<CustomEntity, EntityWriteError> {
        let actor_s = actor.to_string();
        let namespace_owner = namespace_owner.map(str::to_string);
        let out: std::result::Result<CustomEntity, WriteError> = self
            .run(move |core| {
                let Some(ty) = super::entity_types::entity_type_get(core, &input.type_slug)? else {
                    return Ok(Err(WriteError::UnknownType));
                };
                if ty.archived {
                    return Ok(Err(WriteError::ArchivedType));
                }
                let merged =
                    match validate_entity_fields_core(core, &ty.fields, None, &input.fields)? {
                        Ok(m) => m,
                        Err(issues) => return Ok(Err(WriteError::Issues(issues))),
                    };

                // scope "me" pins the row to the writer's namespace; default global —
                // custom entities are household objects (recipes, warranties), where
                // shared is the common case. One-line default to flip if that's wrong.
                let user_scope = match input.scope.as_deref() {
                    Some("me") => namespace_owner.clone(),
                    _ => None,
                };

                let ts = now_iso();
                let e = CustomEntity {
                    id: new_id("ent"),
                    type_id: ty.id.clone(),
                    type_slug: ty.slug.clone(),
                    title: input.title,
                    fields: merged,
                    user_scope,
                    origin_entry_id: None,
                    created_by: actor_s.clone(),
                    created_at: ts.clone(),
                    updated_at: ts.clone(),
                };
                let mut batch = vec![Draft::new(
                    crate::oplog::kind::ENTITY_CREATE,
                    &actor_s,
                    &ts,
                    json!({"kind": ty.slug, "id": e.id, "fields": {
                        "type_id": e.type_id, "title": e.title,
                        "fields": to_json(&e.fields),
                        "user_scope": e.user_scope, "origin_entry_id": e.origin_entry_id,
                        "created_by": e.created_by,
                        "created_at": e.created_at, "updated_at": e.updated_at,
                    }}),
                )];
                batch.extend(ref_mirror_drafts(core, &ty, &e)?);
                core.commit(batch)?;
                Ok(Ok(e))
            })
            .await
            .map_err(EntityWriteError::Other)?;
        let e = out.map_err(WriteError::lift)?;
        self.emit(
            "entity.created",
            actor,
            json!({"id": e.id, "type": e.type_slug}),
        )
        .await
        .map_err(EntityWriteError::Other)?;
        Ok(e)
    }

    pub async fn custom_entities_update(
        &self,
        id: &str,
        patch: CustomEntityPatch,
        actor: &str,
        namespace_owner: Option<&str>,
    ) -> std::result::Result<Option<CustomEntity>, EntityWriteError> {
        let id_s = id.to_string();
        let actor_s = actor.to_string();
        let namespace_owner = namespace_owner.map(str::to_string);
        let out: std::result::Result<Option<CustomEntity>, WriteError> = self
            .run(move |core| {
                let Some(current) = custom_entity_get(core, &id_s)? else {
                    return Ok(Ok(None));
                };
                let ty = super::entity_types::entity_type_get(core, &current.type_id)?
                    .ok_or_else(|| anyhow::anyhow!("type row missing for {}", current.type_id))?;

                let merged = match &patch.fields {
                    Some(p) => {
                        match validate_entity_fields_core(
                            core,
                            &ty.fields,
                            Some(&current.fields),
                            p,
                        )? {
                            Ok(m) => m,
                            Err(issues) => return Ok(Err(WriteError::Issues(issues))),
                        }
                    }
                    None => current.fields.clone(),
                };
                let user_scope = match patch.scope.as_deref() {
                    Some("me") => namespace_owner.clone(),
                    Some("global") => None,
                    _ => current.user_scope.clone(),
                };

                let next = CustomEntity {
                    title: patch.title.unwrap_or(current.title),
                    fields: merged,
                    user_scope,
                    updated_at: now_iso(),
                    ..current
                };
                // The record carries the PATCH for the inner JSON column (the
                // fold merges per key, null removes — merge_fields parity);
                // direct columns carry final values.
                let field_patch = patch.fields.clone().unwrap_or_default();
                let mut batch = vec![Draft::new(
                    crate::oplog::kind::ENTITY_UPDATE,
                    &actor_s,
                    &next.updated_at,
                    json!({"kind": ty.slug, "id": next.id, "fields": {
                        "title": next.title, "fields": field_patch,
                        "user_scope": next.user_scope, "updated_at": next.updated_at,
                    }}),
                )];
                batch.extend(ref_mirror_drafts(core, &ty, &next)?);
                core.commit(batch)?;
                Ok(Ok(Some(next)))
            })
            .await
            .map_err(EntityWriteError::Other)?;
        let next = out.map_err(WriteError::lift)?;
        let Some(next) = next else { return Ok(None) };
        self.emit(
            "entity.updated",
            actor,
            json!({"id": next.id, "type": next.type_slug}),
        )
        .await
        .map_err(EntityWriteError::Other)?;
        Ok(Some(next))
    }

    pub async fn custom_entities_delete(&self, id: &str, actor: &str) -> Result<Option<()>> {
        let id_s = id.to_string();
        let deleted = self
            .run(move |core| {
                let Some(current) = custom_entity_get(core, &id_s)? else {
                    return Ok(None);
                };
                let ts = now_iso();
                let mut batch = vec![Draft::new(
                    crate::oplog::kind::TOMBSTONE,
                    "system",
                    &ts,
                    json!({"kind": current.type_slug, "id": current.id}),
                )];
                // Outbound mirrors and inbound dangling mirrors both go; JSON values
                // in OTHER entities that still name this id are accepted v1 roughness —
                // they surface as unresolvable and get rejected on next touch.
                let link_ids: Vec<String> = {
                    let mut stmt = core.conn().prepare(
                        "SELECT id FROM links WHERE (source_id = ?1 OR target_id = ?1) AND rel LIKE 'field:%'",
                    )?;
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
        self.emit(
            "entity.deleted",
            actor,
            json!({"id": current.id, "type": current.type_slug}),
        )
        .await?;
        Ok(Some(()))
    }
}

/// list()'s inner error carrier (closures return one Result level).
enum ListError {
    UnknownType,
    Issues(Vec<FieldIssue>),
}

/// create/update's inner error carrier.
enum WriteError {
    UnknownType,
    ArchivedType,
    Issues(Vec<FieldIssue>),
}

impl WriteError {
    fn lift(self) -> EntityWriteError {
        match self {
            WriteError::UnknownType => EntityWriteError::UnknownType,
            WriteError::ArchivedType => EntityWriteError::ArchivedType,
            WriteError::Issues(i) => EntityWriteError::Issues(i),
        }
    }
}

pub(crate) fn custom_entity_get(core: &Core, id: &str) -> Result<Option<CustomEntity>> {
    Ok(core
        .conn()
        .query_row(
            "SELECT e.id, e.type_id, e.title, e.fields, e.user_scope, e.origin_entry_id, e.created_by, e.created_at, e.updated_at, t.slug AS type_slug \
             FROM entities e JOIN entity_types t ON t.id = e.type_id WHERE e.id = ?1",
            rusqlite::params![id],
            |r| {
                let slug: String = r.get("type_slug")?;
                row_to_entity(r, &slug)
            },
        )
        .optional()?)
}

/// merge → pure validate → ref-existence, all on the core (Ok(merged) or the
/// issues). The async path in the Postgres port becomes plain reads here.
pub(crate) fn validate_entity_fields_core(
    core: &Core,
    specs: &[EntityField],
    current: Option<&Map<String, Value>>,
    patch: &Map<String, Value>,
) -> Result<std::result::Result<Map<String, Value>, Vec<FieldIssue>>> {
    let empty = Map::new();
    let (merged, touched) = merge_fields(current.unwrap_or(&empty), patch);
    let refs = match validate_fields(specs, &merged, &touched) {
        Ok(refs) => refs,
        Err(issues) => return Ok(Err(issues)),
    };
    let mut issues = Vec::new();
    for rc in refs {
        if !ref_target_exists(core, &rc.kind, &rc.id)? {
            issues.push(FieldIssue {
                field: rc.field,
                code: "ref_not_found",
                message: format!("no {} with id '{}'", rc.kind, rc.id),
            });
        }
    }
    if issues.is_empty() {
        Ok(Ok(merged))
    } else {
        Ok(Err(issues))
    }
}

/// Ref targets: the four linkable built-ins hit their concrete tables;
/// a registered custom slug requires id AND type to agree.
fn ref_target_exists(core: &Core, kind: &str, id: &str) -> Result<bool> {
    let table = match kind {
        "person" => Some("people"),
        "topic" => Some("topics"),
        "project" => Some("projects"),
        "task" => Some("tasks"),
        _ => None,
    };
    let exists: bool = match table {
        Some(t) => core.conn().query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {t} WHERE id = ?1)"),
            rusqlite::params![id],
            |r| r.get(0),
        )?,
        None => core.conn().query_row(
            "SELECT EXISTS(SELECT 1 FROM entities e JOIN entity_types t ON t.id = e.type_id \
             WHERE e.id = ?1 AND t.slug = ?2)",
            rusqlite::params![id, kind],
            |r| r.get(0),
        )?,
    };
    Ok(exists)
}

/// Ref-field link mirrors, replace-don't-diff on every write: link.remove
/// records for the existing field:% edges + link.add records for the current
/// ref values. (FTS is the fold's job now.)
fn ref_mirror_drafts(core: &Core, ty: &EntityTypeView, e: &CustomEntity) -> Result<Vec<Draft>> {
    let ts = now_iso();
    let mut drafts = Vec::new();
    let existing: Vec<String> = {
        let mut stmt = core.conn().prepare(
            "SELECT id FROM links WHERE source_kind = ?1 AND source_id = ?2 AND rel LIKE 'field:%'",
        )?;
        let rows = stmt.query_map(rusqlite::params![ty.slug, e.id], |r| r.get(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for lid in &existing {
        drafts.push(super::links::link_remove_draft(lid, &ts));
    }
    for spec in &ty.fields {
        if spec.field_type != hive_shared::FieldType::Ref {
            continue;
        }
        let (Some(target_kind), Some(target_id)) = (
            spec.ref_kind.as_deref(),
            e.fields.get(&spec.slug).and_then(Value::as_str),
        ) else {
            continue;
        };
        drafts.push(super::links::link_draft(
            &ty.slug,
            &e.id,
            target_kind,
            target_id,
            &format!("field:{}", spec.slug),
            &ts,
        ));
    }
    Ok(drafts)
}

fn row_to_entity(r: &rusqlite::Row, type_slug: &str) -> rusqlite::Result<CustomEntity> {
    let fields_text: String = r.get("fields")?;
    Ok(CustomEntity {
        id: r.get("id")?,
        type_id: r.get("type_id")?,
        type_slug: type_slug.to_string(),
        title: r.get("title")?,
        fields: serde_json::from_str(&fields_text).unwrap_or_default(),
        user_scope: r.get("user_scope")?,
        origin_entry_id: r.get("origin_entry_id")?,
        created_by: r.get("created_by")?,
        created_at: r.get("created_at")?,
        updated_at: r.get("updated_at")?,
    })
}
