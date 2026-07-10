// Custom entity type registry (entity_types + entity_fields). Admin-defined;
// the assembled EntityTypeView (type + ordered fields) is the kind-config
// contract MCP consumes. Slugs are immutable (they are the kind string in
// search/links rows); evolution is additive — archive fields and types
// instead of deleting, hard delete only at zero instances. Writes are
// entity.create/entity.update/tombstone records on the entity_type and
// entity_field built-in kinds.

use anyhow::Result;
use hive_shared::{
    EntityField, EntityFieldPatch, EntityTypePatch, EntityTypeView, FieldType, NewEntityField,
    NewEntityType,
};
use rusqlite::OptionalExtension;
use serde_json::json;

use super::entity_validation::{slugify, validate_field_slug, validate_type_slug, FieldIssue};
use super::{json_vec, new_id, now_iso, to_json, Core, Draft, Store};

/// Registry mutations fail with a structured issue list (the same shape
/// instance validation produces), so routes/MCP render them uniformly.
#[derive(Debug)]
pub struct TypeIssues(pub Vec<FieldIssue>);

impl std::fmt::Display for TypeIssues {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let msgs: Vec<&str> = self.0.iter().map(|i| i.message.as_str()).collect();
        write!(f, "{}", msgs.join("; "))
    }
}
impl std::error::Error for TypeIssues {}

pub enum TypeWriteError {
    Issues(Vec<FieldIssue>),
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for TypeWriteError {
    fn from(e: E) -> Self {
        TypeWriteError::Other(e.into())
    }
}

fn issue(field: &str, code: &'static str, message: String) -> Vec<FieldIssue> {
    vec![FieldIssue {
        field: field.to_string(),
        code,
        message,
    }]
}

impl Store {
    pub async fn entity_types_list(&self, include_archived: bool) -> Result<Vec<EntityTypeView>> {
        self.run(move |core| {
            let ids: Vec<(String, bool)> = {
                let mut stmt = core
                    .conn()
                    .prepare("SELECT id, archived FROM entity_types ORDER BY name")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
                rows.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut out = Vec::with_capacity(ids.len());
            for (id, archived) in ids {
                if archived && !include_archived {
                    continue;
                }
                if let Some(view) = entity_type_get(core, &id)? {
                    out.push(view);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Fetch by id (etype_*) or slug — routes accept either.
    pub async fn entity_types_get(&self, id_or_slug: &str) -> Result<Option<EntityTypeView>> {
        let id_or_slug = id_or_slug.to_string();
        self.run(move |core| entity_type_get(core, &id_or_slug))
            .await
    }

    pub async fn entity_types_create(
        &self,
        input: NewEntityType,
        actor: &str,
    ) -> std::result::Result<EntityTypeView, TypeWriteError> {
        let actor_s = actor.to_string();
        let out: std::result::Result<EntityTypeView, Vec<FieldIssue>> = self
            .run(move |core| {
                let slug = match &input.slug {
                    Some(s) => s.clone(),
                    None => slugify(&input.name),
                };
                if let Err(i) = validate_type_slug(&slug) {
                    return Ok(Err(vec![i]));
                }
                if entity_type_get(core, &slug)?.is_some() {
                    return Ok(Err(issue(
                        "slug",
                        "bad_slug",
                        format!("type '{slug}' already exists"),
                    )));
                }

                let ts = now_iso();
                let type_id = new_id("etype");
                let name_plural = input
                    .name_plural
                    .clone()
                    .unwrap_or_else(|| format!("{}s", input.name));

                let mut batch = vec![Draft::new(
                    crate::oplog::kind::ENTITY_CREATE,
                    &actor_s,
                    &ts,
                    json!({"kind": "entity_type", "id": type_id, "fields": {
                        "slug": slug, "name": input.name, "name_plural": name_plural,
                        "description": input.description, "icon": input.icon,
                        "color": input.color, "board_field": null, "archived": false,
                        "created_by": actor_s, "created_at": ts, "updated_at": ts,
                    }}),
                )];
                let mut planned: Vec<EntityField> = Vec::new();
                for (i, f) in input.fields.iter().enumerate() {
                    match entity_field_insert_plan(core, &type_id, f, i as i64, &planned)? {
                        Ok((field, draft)) => {
                            planned.push(field);
                            batch.push(draft);
                        }
                        Err(issues) => return Ok(Err(issues)),
                    }
                }

                // board_field is validated against the just-planned fields so the
                // contract (null or a live choice field's slug) holds from birth.
                if let Some(bf) = &input.board_field {
                    let ok = planned
                        .iter()
                        .any(|f| f.slug == *bf && f.field_type == FieldType::Choice && !f.archived);
                    if !ok {
                        return Ok(Err(issue(
                            "board_field",
                            "bad_choice",
                            format!("board_field '{bf}' must be a live choice field"),
                        )));
                    }
                    batch.push(Draft::new(
                        crate::oplog::kind::ENTITY_UPDATE,
                        &actor_s,
                        &ts,
                        json!({"kind": "entity_type", "id": type_id, "fields": {
                            "board_field": bf, "updated_at": now_iso(),
                        }}),
                    ));
                }

                core.commit(batch)?;
                let view = entity_type_get(core, &type_id)?.expect("just inserted");
                Ok(Ok(view))
            })
            .await
            .map_err(TypeWriteError::Other)?;
        let view = out.map_err(TypeWriteError::Issues)?;
        self.emit(
            "entity_type.created",
            actor,
            json!({"id": view.id, "slug": view.slug}),
        )
        .await
        .map_err(TypeWriteError::Other)?;
        Ok(view)
    }

    pub async fn entity_types_update(
        &self,
        id_or_slug: &str,
        patch: EntityTypePatch,
        actor: &str,
    ) -> std::result::Result<Option<EntityTypeView>, TypeWriteError> {
        let id_or_slug_s = id_or_slug.to_string();
        let actor_s = actor.to_string();
        let out: std::result::Result<Option<EntityTypeView>, Vec<FieldIssue>> = self
            .run(move |core| {
                let Some(current) = entity_type_get(core, &id_or_slug_s)? else {
                    return Ok(Ok(None));
                };

                let mut batch = vec![Draft::new(
                    crate::oplog::kind::ENTITY_UPDATE,
                    &actor_s,
                    &now_iso(),
                    json!({"kind": "entity_type", "id": current.id, "fields": {
                        "name": patch.name.as_deref().unwrap_or(&current.name),
                        "name_plural": patch.name_plural.as_deref().unwrap_or(&current.name_plural),
                        "description": patch.description.as_deref().unwrap_or(&current.description),
                        "icon": patch.icon.as_deref().unwrap_or(&current.icon),
                        "color": patch.color.as_deref().unwrap_or(&current.color),
                        "archived": patch.archived.unwrap_or(current.archived),
                        "updated_at": now_iso(),
                    }}),
                )];

                let base_pos = current.fields.len() as i64;
                let mut planned: Vec<EntityField> = current.fields.clone();
                for (i, f) in patch.add_fields.iter().enumerate() {
                    if planned
                        .iter()
                        .any(|ef| ef.slug == f.slug.clone().unwrap_or_else(|| slugify(&f.label)))
                    {
                        return Ok(Err(issue(
                            "add_fields",
                            "bad_slug",
                            "field slug already exists on this type".to_string(),
                        )));
                    }
                    match entity_field_insert_plan(
                        core,
                        &current.id,
                        f,
                        base_pos + i as i64,
                        &planned,
                    )? {
                        Ok((field, draft)) => {
                            planned.push(field);
                            batch.push(draft);
                        }
                        Err(issues) => return Ok(Err(issues)),
                    }
                }

                for fp in &patch.update_fields {
                    match entity_field_update_plan(core, &current.id, fp)? {
                        Ok(draft) => batch.push(draft),
                        Err(issues) => return Ok(Err(issues)),
                    }
                }

                if let Some(bf) = &patch.board_field {
                    if let Some(bf_slug) = bf {
                        let ok = planned.iter().any(|f| {
                            f.slug == *bf_slug && f.field_type == FieldType::Choice && !f.archived
                        });
                        if !ok {
                            return Ok(Err(issue(
                                "board_field",
                                "bad_choice",
                                format!("board_field '{bf_slug}' must be a live choice field"),
                            )));
                        }
                    }
                    batch.push(Draft::new(
                        crate::oplog::kind::ENTITY_UPDATE,
                        &actor_s,
                        &now_iso(),
                        json!({"kind": "entity_type", "id": current.id, "fields": {
                            "board_field": bf, "updated_at": now_iso(),
                        }}),
                    ));
                }

                core.commit(batch)?;
                Ok(Ok(entity_type_get(core, &current.id)?))
            })
            .await
            .map_err(TypeWriteError::Other)?;
        let view = out.map_err(TypeWriteError::Issues)?;
        let Some(view) = view else { return Ok(None) };
        self.emit(
            "entity_type.updated",
            actor,
            json!({"id": view.id, "slug": view.slug}),
        )
        .await
        .map_err(TypeWriteError::Other)?;
        Ok(Some(view))
    }

    /// Hard delete only when the type has zero instances (typo cleanup);
    /// otherwise the caller should archive. Returns false when instances exist.
    pub async fn entity_types_delete(&self, id_or_slug: &str, actor: &str) -> Result<Option<bool>> {
        let id_or_slug_s = id_or_slug.to_string();
        let result = self
            .run(move |core| {
                let Some(current) = entity_type_get(core, &id_or_slug_s)? else {
                    return Ok(None);
                };
                let n: i64 = core.conn().query_row(
                    "SELECT COUNT(*) FROM entities WHERE type_id = ?1",
                    rusqlite::params![current.id],
                    |r| r.get(0),
                )?;
                if n > 0 {
                    return Ok(Some((current, false)));
                }
                let ts = now_iso();
                let mut batch: Vec<Draft> = current
                    .fields
                    .iter()
                    .map(|f| {
                        Draft::new(
                            crate::oplog::kind::TOMBSTONE,
                            "system",
                            &ts,
                            json!({"kind": "entity_field", "id": f.id}),
                        )
                    })
                    .collect();
                batch.push(Draft::new(
                    crate::oplog::kind::TOMBSTONE,
                    "system",
                    &ts,
                    json!({"kind": "entity_type", "id": current.id}),
                ));
                core.commit(batch)?;
                Ok(Some((current, true)))
            })
            .await?;
        let Some((current, deleted)) = result else {
            return Ok(None);
        };
        if deleted {
            self.emit(
                "entity_type.deleted",
                actor,
                json!({"id": current.id, "slug": current.slug}),
            )
            .await?;
        }
        Ok(Some(deleted))
    }
}

// ---- internals (core-level; the async fns above ride them) ----

/// Validate one new field and produce its entity.create draft. `planned`
/// carries same-batch fields so ref_kind checks see them.
fn entity_field_insert_plan(
    core: &Core,
    type_id: &str,
    f: &NewEntityField,
    default_pos: i64,
    planned: &[EntityField],
) -> Result<std::result::Result<(EntityField, Draft), Vec<FieldIssue>>> {
    let slug = f.slug.clone().unwrap_or_else(|| slugify(&f.label));
    if let Err(i) = validate_field_slug(&slug) {
        return Ok(Err(vec![i]));
    }
    let Some(ft) = FieldType::parse(&f.field_type) else {
        return Ok(Err(issue(
            &slug,
            "wrong_type",
            format!("unknown field_type '{}'", f.field_type),
        )));
    };
    if ft == FieldType::Choice && f.options.is_empty() {
        return Ok(Err(issue(
            &slug,
            "bad_choice",
            "choice fields need options".into(),
        )));
    }
    if ft == FieldType::Ref && f.ref_kind.is_none() {
        return Ok(Err(issue(
            &slug,
            "bad_ref_kind",
            "ref fields need ref_kind".into(),
        )));
    }
    if let Some(rk) = &f.ref_kind {
        let builtin_ok = matches!(rk.as_str(), "person" | "topic" | "project" | "task");
        if !builtin_ok && entity_type_get(core, rk)?.is_none() {
            return Ok(Err(issue(
                &slug,
                "bad_ref_kind",
                format!("ref_kind '{rk}' is neither a built-in kind nor a custom type"),
            )));
        }
    }
    let _ = planned; // same-batch ref_kind targets are types, checked above
    let ts = now_iso();
    let field = EntityField {
        id: new_id("efield"),
        slug: slug.clone(),
        label: f.label.clone(),
        field_type: ft,
        required: f.required,
        position: f.position.unwrap_or(default_pos),
        options: f.options.clone(),
        ref_kind: f.ref_kind.clone(),
        archived: false,
    };
    let draft = Draft::new(
        crate::oplog::kind::ENTITY_CREATE,
        "system",
        &ts,
        json!({"kind": "entity_field", "id": field.id, "fields": {
            "type_id": type_id, "slug": field.slug, "label": field.label,
            "field_type": field.field_type.as_str(), "required": field.required,
            "position": field.position, "options": to_json(&field.options),
            "ref_kind": field.ref_kind, "archived": false,
            "created_at": ts, "updated_at": ts,
        }}),
    );
    Ok(Ok((field, draft)))
}

fn entity_field_update_plan(
    core: &Core,
    type_id: &str,
    fp: &EntityFieldPatch,
) -> Result<std::result::Result<Draft, Vec<FieldIssue>>> {
    struct Row {
        id: String,
        label: String,
        required: bool,
        position: i64,
        archived: bool,
        options: Vec<String>,
        field_type: String,
    }
    let row = core
        .conn()
        .query_row(
            "SELECT * FROM entity_fields WHERE type_id = ?1 AND slug = ?2",
            rusqlite::params![type_id, fp.slug],
            |r| {
                Ok(Row {
                    id: r.get("id")?,
                    label: r.get("label")?,
                    required: r.get("required")?,
                    position: r.get("position")?,
                    archived: r.get("archived")?,
                    options: json_vec(r.get::<_, String>("options")?.as_str()),
                    field_type: r.get("field_type")?,
                })
            },
        )
        .optional()?;
    let Some(r) = row else {
        return Ok(Err(issue(
            &fp.slug,
            "unknown_field",
            format!("no field '{}' on this type", fp.slug),
        )));
    };
    if FieldType::parse(&r.field_type) == Some(FieldType::Choice) {
        if let Some(opts) = &fp.options {
            if opts.is_empty() {
                return Ok(Err(issue(
                    &fp.slug,
                    "bad_choice",
                    "choice fields need options".into(),
                )));
            }
        }
    }
    Ok(Ok(Draft::new(
        crate::oplog::kind::ENTITY_UPDATE,
        "system",
        &now_iso(),
        json!({"kind": "entity_field", "id": r.id, "fields": {
            "label": fp.label.as_deref().unwrap_or(&r.label),
            "required": fp.required.unwrap_or(r.required),
            "position": fp.position.unwrap_or(r.position),
            "options": to_json(fp.options.as_ref().unwrap_or(&r.options)),
            "archived": fp.archived.unwrap_or(r.archived),
            "updated_at": now_iso(),
        }}),
    )))
}

pub(crate) fn entity_type_get(core: &Core, id_or_slug: &str) -> Result<Option<EntityTypeView>> {
    struct TypeRow {
        id: String,
        slug: String,
        name: String,
        name_plural: String,
        description: String,
        icon: String,
        color: String,
        board_field: Option<String>,
        archived: bool,
        created_by: String,
        created_at: String,
        updated_at: String,
    }
    let row = core
        .conn()
        .query_row(
            "SELECT * FROM entity_types WHERE id = ?1 OR slug = ?1",
            rusqlite::params![id_or_slug],
            |r| {
                Ok(TypeRow {
                    id: r.get("id")?,
                    slug: r.get("slug")?,
                    name: r.get("name")?,
                    name_plural: r.get("name_plural")?,
                    description: r.get("description")?,
                    icon: r.get("icon")?,
                    color: r.get("color")?,
                    board_field: r.get("board_field")?,
                    archived: r.get("archived")?,
                    created_by: r.get("created_by")?,
                    created_at: r.get("created_at")?,
                    updated_at: r.get("updated_at")?,
                })
            },
        )
        .optional()?;
    let Some(t) = row else { return Ok(None) };
    let fields = entity_fields_of_conn(core, &t.id)?;
    Ok(Some(EntityTypeView {
        id: t.id,
        slug: t.slug,
        name: t.name,
        name_plural: t.name_plural,
        description: t.description,
        icon: t.icon,
        color: t.color,
        board_field: t.board_field,
        archived: t.archived,
        created_by: t.created_by,
        created_at: t.created_at,
        updated_at: t.updated_at,
        fields,
    }))
}

pub(crate) fn entity_fields_of_conn(core: &Core, type_id: &str) -> Result<Vec<EntityField>> {
    let mut stmt = core
        .conn()
        .prepare("SELECT * FROM entity_fields WHERE type_id = ?1 ORDER BY position, created_at")?;
    let rows = stmt.query_map(rusqlite::params![type_id], |r| {
        let ft: String = r.get("field_type")?;
        Ok((
            EntityField {
                id: r.get("id")?,
                slug: r.get("slug")?,
                label: r.get("label")?,
                field_type: FieldType::Text, // placeholder; fixed below
                required: r.get("required")?,
                position: r.get("position")?,
                options: json_vec(r.get::<_, String>("options")?.as_str()),
                ref_kind: r.get("ref_kind")?,
                archived: r.get("archived")?,
            },
            ft,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (mut field, ft) = row?;
        // Registry rows are only written through FieldType::parse, so an
        // unparseable stored value is corruption — surface it.
        field.field_type =
            FieldType::parse(&ft).ok_or_else(|| anyhow::anyhow!("corrupt field_type '{ft}'"))?;
        out.push(field);
    }
    Ok(out)
}
