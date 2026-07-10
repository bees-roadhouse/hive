// Custom entity instances: validated-JSONB rows registered at the seams
// (search via index_entity, ref fields mirrored into links, wire events).
// JSONB binding rule: the workspace sqlx has no `json` feature, so writes
// bind the serialized string through a ?::jsonb cast and every read projects
// fields::text — row_to_entity here is the ONLY read path; never SELECT *
// from entities (the JSONB column won't decode as String).

use anyhow::Result;
use hive_shared::{CustomEntity, CustomEntityPatch, EntityField, EntityTypeView, NewCustomEntity};
use serde_json::{json, Map, Value};
use sqlx::Row;

use super::entity_validation::{merge_fields, searchable_text, validate_fields, FieldIssue};
use super::{new_id, now_iso, Store};
use crate::Visibility;

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
        vis: &Visibility,
    ) -> std::result::Result<Vec<CustomEntity>, EntityWriteError> {
        let Some(ty) = self.entity_types_get(&filter.type_slug).await? else {
            return Err(EntityWriteError::UnknownType);
        };
        // Unknown sort keys fail closed, consistent with the validation posture.
        if let Some(s) = filter.sort.as_deref() {
            let known = matches!(s, "title" | "created_at" | "updated_at")
                || ty.fields.iter().any(|f| f.slug == s);
            if !known {
                return Err(EntityWriteError::Issues(vec![FieldIssue {
                    field: s.to_string(),
                    code: "unknown_field",
                    message: format!("unknown sort field '{s}'"),
                }]));
            }
        }

        let limit = filter.limit.clamp(1, 500);
        let rows = match vis {
            Visibility::All => {
                crate::pgq::query(
                    "SELECT id, type_id, title, fields::text AS fields, user_scope, origin_entry_id, created_by, created_at, updated_at \
                     FROM entities WHERE type_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
                )
                .bind(&ty.id)
                .bind(limit)
                .bind(filter.offset)
                .fetch_all(self.db())
                .await?
            }
            Visibility::Namespace(u) => {
                crate::pgq::query(
                    "SELECT id, type_id, title, fields::text AS fields, user_scope, origin_entry_id, created_by, created_at, updated_at \
                     FROM entities WHERE type_id = ? AND (user_scope IS NULL OR user_scope = ?) ORDER BY created_at DESC LIMIT ? OFFSET ?",
                )
                .bind(&ty.id)
                .bind(u)
                .bind(limit)
                .bind(filter.offset)
                .fetch_all(self.db())
                .await?
            }
        };
        let mut items: Vec<CustomEntity> = rows
            .iter()
            .map(|r| row_to_entity(r, &ty.slug))
            .collect::<Result<_>>()?;

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
        Ok(items)
    }

    pub async fn custom_entities_get(
        &self,
        id: &str,
        vis: &Visibility,
    ) -> Result<Option<CustomEntity>> {
        let row = crate::pgq::query(
            "SELECT e.id, e.type_id, e.title, e.fields::text AS fields, e.user_scope, e.origin_entry_id, e.created_by, e.created_at, e.updated_at, t.slug AS type_slug \
             FROM entities e JOIN entity_types t ON t.id = e.type_id WHERE e.id = ?",
        )
        .bind(id)
        .fetch_optional(self.db())
        .await?;
        let Some(r) = row else { return Ok(None) };
        let slug: String = r.try_get("type_slug")?;
        let e = row_to_entity(&r, &slug)?;
        // Invisible reads 404, not 403 — no existence leak across namespaces.
        match vis {
            Visibility::All => Ok(Some(e)),
            Visibility::Namespace(u) => Ok(match &e.user_scope {
                None => Some(e),
                Some(scope) if scope == u => Some(e),
                Some(_) => None,
            }),
        }
    }

    pub async fn custom_entities_create(
        &self,
        input: NewCustomEntity,
        actor: &str,
        namespace_owner: Option<&str>,
    ) -> std::result::Result<CustomEntity, EntityWriteError> {
        let Some(ty) = self.entity_types_get(&input.type_slug).await? else {
            return Err(EntityWriteError::UnknownType);
        };
        if ty.archived {
            return Err(EntityWriteError::ArchivedType);
        }
        let merged = self
            .validate_entity_fields(&ty.fields, None, &input.fields)
            .await?;

        // scope "me" pins the row to the writer's namespace; default global —
        // custom entities are household objects (recipes, warranties), where
        // shared is the common case. One-line default to flip if that's wrong.
        let user_scope = match input.scope.as_deref() {
            Some("me") => namespace_owner.map(String::from),
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
            created_by: actor.to_string(),
            created_at: ts.clone(),
            updated_at: ts,
        };
        crate::pgq::query(
            "INSERT INTO entities (id, type_id, title, fields, user_scope, origin_entry_id, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?::jsonb, ?, ?, ?, ?, ?)",
        )
        .bind(&e.id)
        .bind(&e.type_id)
        .bind(&e.title)
        .bind(serde_json::to_string(&e.fields)?)
        .bind(&e.user_scope)
        .bind(&e.origin_entry_id)
        .bind(&e.created_by)
        .bind(&e.created_at)
        .bind(&e.updated_at)
        .execute(self.db())
        .await?;

        self.sync_entity_seams(&ty, &e).await?;
        self.emit(
            "entity.created",
            actor,
            json!({"id": e.id, "type": ty.slug}),
        )
        .await?;
        Ok(e)
    }

    pub async fn custom_entities_update(
        &self,
        id: &str,
        patch: CustomEntityPatch,
        actor: &str,
        vis: &Visibility,
        namespace_owner: Option<&str>,
    ) -> std::result::Result<Option<CustomEntity>, EntityWriteError> {
        let Some(current) = self.custom_entities_get(id, vis).await? else {
            return Ok(None);
        };
        let ty = self
            .entity_types_get(&current.type_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("type row missing for {}", current.type_id))?;

        let merged = match &patch.fields {
            Some(p) => {
                self.validate_entity_fields(&ty.fields, Some(&current.fields), p)
                    .await?
            }
            None => current.fields.clone(),
        };
        let user_scope = match patch.scope.as_deref() {
            Some("me") => namespace_owner.map(String::from),
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
        crate::pgq::query(
            "UPDATE entities SET title=?, fields=?::jsonb, user_scope=?, updated_at=? WHERE id=?",
        )
        .bind(&next.title)
        .bind(serde_json::to_string(&next.fields)?)
        .bind(&next.user_scope)
        .bind(&next.updated_at)
        .bind(&next.id)
        .execute(self.db())
        .await?;

        self.sync_entity_seams(&ty, &next).await?;
        self.emit(
            "entity.updated",
            actor,
            json!({"id": next.id, "type": ty.slug}),
        )
        .await?;
        Ok(Some(next))
    }

    pub async fn custom_entities_delete(
        &self,
        id: &str,
        actor: &str,
        vis: &Visibility,
    ) -> Result<Option<()>> {
        let Some(current) = self.custom_entities_get(id, vis).await? else {
            return Ok(None);
        };
        crate::pgq::query("DELETE FROM entities WHERE id = ?")
            .bind(&current.id)
            .execute(self.db())
            .await?;
        self.unindex_entity(&current.type_slug, &current.id).await?;
        // Outbound mirrors and inbound dangling mirrors both go; JSONB values
        // in OTHER entities that still name this id are accepted v1 roughness —
        // they surface as unresolvable and get rejected on next touch.
        crate::pgq::query(
            "DELETE FROM links WHERE (source_id = ? OR target_id = ?) AND rel LIKE 'field:%'",
        )
        .bind(&current.id)
        .bind(&current.id)
        .execute(self.db())
        .await?;
        self.emit(
            "entity.deleted",
            actor,
            json!({"id": current.id, "type": current.type_slug}),
        )
        .await?;
        Ok(Some(()))
    }

    /// merge → pure validate → async ref-existence. Ok(merged) or the issues.
    pub(crate) async fn validate_entity_fields(
        &self,
        specs: &[EntityField],
        current: Option<&Map<String, Value>>,
        patch: &Map<String, Value>,
    ) -> std::result::Result<Map<String, Value>, EntityWriteError> {
        let empty = Map::new();
        let (merged, touched) = merge_fields(current.unwrap_or(&empty), patch);
        let refs = validate_fields(specs, &merged, &touched).map_err(EntityWriteError::Issues)?;
        let mut issues = Vec::new();
        for rc in refs {
            if !self.ref_target_exists(&rc.kind, &rc.id).await? {
                issues.push(FieldIssue {
                    field: rc.field,
                    code: "ref_not_found",
                    message: format!("no {} with id '{}'", rc.kind, rc.id),
                });
            }
        }
        if issues.is_empty() {
            Ok(merged)
        } else {
            Err(EntityWriteError::Issues(issues))
        }
    }

    /// Ref targets: the four linkable built-ins hit their concrete tables;
    /// a registered custom slug requires id AND type to agree.
    async fn ref_target_exists(&self, kind: &str, id: &str) -> Result<bool> {
        let table = match kind {
            "person" => Some("people"),
            "topic" => Some("topics"),
            "project" => Some("projects"),
            "task" => Some("tasks"),
            _ => None,
        };
        let row = match table {
            Some(t) => {
                crate::pgq::query(&format!("SELECT 1 AS x FROM {t} WHERE id = ?"))
                    .bind(id)
                    .fetch_optional(self.db())
                    .await?
            }
            None => {
                crate::pgq::query(
                    "SELECT 1 AS x FROM entities e JOIN entity_types t ON t.id = e.type_id \
                     WHERE e.id = ? AND t.slug = ?",
                )
                .bind(id)
                .bind(kind)
                .fetch_optional(self.db())
                .await?
            }
        };
        Ok(row.is_some())
    }

    /// FTS row + ref-field link mirrors, replace-don't-diff on every write.
    async fn sync_entity_seams(&self, ty: &EntityTypeView, e: &CustomEntity) -> Result<()> {
        self.index_entity(
            &ty.slug,
            &e.id,
            &e.title,
            &searchable_text(&ty.fields, &e.fields),
            &[],
        )
        .await?;
        crate::pgq::query(
            "DELETE FROM links WHERE source_kind = ? AND source_id = ? AND rel LIKE 'field:%'",
        )
        .bind(&ty.slug)
        .bind(&e.id)
        .execute(self.db())
        .await?;
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
            self.links_create(
                &ty.slug,
                &e.id,
                target_kind,
                target_id,
                &format!("field:{}", spec.slug),
            )
            .await?;
        }
        Ok(())
    }
}

fn row_to_entity(r: &sqlx::postgres::PgRow, type_slug: &str) -> Result<CustomEntity> {
    let fields_text: String = r.try_get("fields")?;
    Ok(CustomEntity {
        id: r.try_get("id")?,
        type_id: r.try_get("type_id")?,
        type_slug: type_slug.to_string(),
        title: r.try_get("title")?,
        fields: serde_json::from_str(&fields_text).unwrap_or_default(),
        user_scope: r.try_get("user_scope")?,
        origin_entry_id: r.try_get("origin_entry_id")?,
        created_by: r.try_get("created_by")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
    })
}
