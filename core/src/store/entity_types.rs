// Custom entity type registry (entity_types + entity_fields). Admin-defined;
// the assembled EntityTypeView (type + ordered fields) is the kind-config
// contract the web board engine and MCP consume. Slugs are immutable (they
// are the kind string in search/links rows); evolution is additive — archive
// fields and types instead of deleting, hard delete only at zero instances.

use anyhow::Result;
use hive_shared::{
    EntityField, EntityFieldPatch, EntityTypePatch, EntityTypeView, FieldType, NewEntityField,
    NewEntityType,
};
use serde_json::json;
use sqlx::Row;

use super::entity_validation::{slugify, validate_field_slug, validate_type_slug, FieldIssue};
use super::{json_vec, new_id, now_iso, to_json, Store};

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

fn issue(field: &str, code: &'static str, message: String) -> TypeWriteError {
    TypeWriteError::Issues(vec![FieldIssue {
        field: field.to_string(),
        code,
        message,
    }])
}

impl Store {
    pub async fn entity_types_list(&self, include_archived: bool) -> Result<Vec<EntityTypeView>> {
        let rows = crate::pgq::query("SELECT * FROM entity_types ORDER BY name")
            .fetch_all(self.db())
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in &rows {
            let archived: bool = r.try_get("archived")?;
            if archived && !include_archived {
                continue;
            }
            out.push(self.assemble_type_view(r).await?);
        }
        Ok(out)
    }

    /// Fetch by id (etype_*) or slug — routes accept either.
    pub async fn entity_types_get(&self, id_or_slug: &str) -> Result<Option<EntityTypeView>> {
        let row = crate::pgq::query("SELECT * FROM entity_types WHERE id = ? OR slug = ?")
            .bind(id_or_slug)
            .bind(id_or_slug)
            .fetch_optional(self.db())
            .await?;
        match row {
            Some(r) => Ok(Some(self.assemble_type_view(&r).await?)),
            None => Ok(None),
        }
    }

    pub async fn entity_types_create(
        &self,
        input: NewEntityType,
        actor: &str,
    ) -> std::result::Result<EntityTypeView, TypeWriteError> {
        let slug = match &input.slug {
            Some(s) => s.clone(),
            None => slugify(&input.name),
        };
        validate_type_slug(&slug).map_err(|i| TypeWriteError::Issues(vec![i]))?;
        if self.entity_types_get(&slug).await?.is_some() {
            return Err(issue(
                "slug",
                "bad_slug",
                format!("type '{slug}' already exists"),
            ));
        }

        let ts = now_iso();
        let type_id = new_id("etype");
        let name_plural = input
            .name_plural
            .clone()
            .unwrap_or_else(|| format!("{}s", input.name));

        crate::pgq::query(
            "INSERT INTO entity_types (id, slug, name, name_plural, description, icon, color, board_field, archived, created_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL, FALSE, ?, ?, ?)",
        )
        .bind(&type_id)
        .bind(&slug)
        .bind(&input.name)
        .bind(&name_plural)
        .bind(&input.description)
        .bind(&input.icon)
        .bind(&input.color)
        .bind(actor)
        .bind(&ts)
        .bind(&ts)
        .execute(self.db())
        .await?;

        for (i, f) in input.fields.iter().enumerate() {
            self.entity_field_insert(&type_id, f, i as i64).await?;
        }

        // board_field is validated against the just-inserted fields so the
        // contract (null or a live choice field's slug) holds from birth.
        if let Some(bf) = &input.board_field {
            self.set_board_field(&type_id, Some(bf.clone())).await?;
        }

        self.emit(
            "entity_type.created",
            actor,
            json!({"id": type_id, "slug": slug}),
        )
        .await?;
        let view = self
            .entity_types_get(&type_id)
            .await?
            .expect("just inserted");
        Ok(view)
    }

    pub async fn entity_types_update(
        &self,
        id_or_slug: &str,
        patch: EntityTypePatch,
        actor: &str,
    ) -> std::result::Result<Option<EntityTypeView>, TypeWriteError> {
        let Some(current) = self.entity_types_get(id_or_slug).await? else {
            return Ok(None);
        };

        crate::pgq::query(
            "UPDATE entity_types SET name=?, name_plural=?, description=?, icon=?, color=?, archived=?, updated_at=? WHERE id=?",
        )
        .bind(patch.name.as_deref().unwrap_or(&current.name))
        .bind(patch.name_plural.as_deref().unwrap_or(&current.name_plural))
        .bind(patch.description.as_deref().unwrap_or(&current.description))
        .bind(patch.icon.as_deref().unwrap_or(&current.icon))
        .bind(patch.color.as_deref().unwrap_or(&current.color))
        .bind(patch.archived.unwrap_or(current.archived))
        .bind(now_iso())
        .bind(&current.id)
        .execute(self.db())
        .await?;

        let base_pos = current.fields.len() as i64;
        for (i, f) in patch.add_fields.iter().enumerate() {
            if current
                .fields
                .iter()
                .any(|ef| ef.slug == f.slug.clone().unwrap_or_else(|| slugify(&f.label)))
            {
                return Err(issue(
                    "add_fields",
                    "bad_slug",
                    "field slug already exists on this type".to_string(),
                ));
            }
            self.entity_field_insert(&current.id, f, base_pos + i as i64)
                .await?;
        }

        for fp in &patch.update_fields {
            self.entity_field_update(&current.id, fp).await?;
        }

        if let Some(bf) = &patch.board_field {
            self.set_board_field(&current.id, bf.clone()).await?;
        }

        self.emit(
            "entity_type.updated",
            actor,
            json!({"id": current.id, "slug": current.slug}),
        )
        .await?;
        Ok(self.entity_types_get(&current.id).await?)
    }

    /// Hard delete only when the type has zero instances (typo cleanup);
    /// otherwise the caller should archive. Returns false when instances exist.
    pub async fn entity_types_delete(&self, id_or_slug: &str, actor: &str) -> Result<Option<bool>> {
        let Some(current) = self.entity_types_get(id_or_slug).await? else {
            return Ok(None);
        };
        let n: i64 = crate::pgq::query("SELECT COUNT(*) AS n FROM entities WHERE type_id = ?")
            .bind(&current.id)
            .fetch_one(self.db())
            .await?
            .try_get("n")?;
        if n > 0 {
            return Ok(Some(false));
        }
        crate::pgq::query("DELETE FROM entity_fields WHERE type_id = ?")
            .bind(&current.id)
            .execute(self.db())
            .await?;
        crate::pgq::query("DELETE FROM entity_types WHERE id = ?")
            .bind(&current.id)
            .execute(self.db())
            .await?;
        self.emit(
            "entity_type.deleted",
            actor,
            json!({"id": current.id, "slug": current.slug}),
        )
        .await?;
        Ok(Some(true))
    }

    // ---- internals ----

    async fn entity_field_insert(
        &self,
        type_id: &str,
        f: &NewEntityField,
        default_pos: i64,
    ) -> std::result::Result<(), TypeWriteError> {
        let slug = f.slug.clone().unwrap_or_else(|| slugify(&f.label));
        validate_field_slug(&slug).map_err(|i| TypeWriteError::Issues(vec![i]))?;
        let Some(ft) = FieldType::parse(&f.field_type) else {
            return Err(issue(
                &slug,
                "wrong_type",
                format!("unknown field_type '{}'", f.field_type),
            ));
        };
        if ft == FieldType::Choice && f.options.is_empty() {
            return Err(issue(
                &slug,
                "bad_choice",
                "choice fields need options".into(),
            ));
        }
        if ft == FieldType::Ref && f.ref_kind.is_none() {
            return Err(issue(
                &slug,
                "bad_ref_kind",
                "ref fields need ref_kind".into(),
            ));
        }
        if let Some(rk) = &f.ref_kind {
            let builtin_ok = matches!(rk.as_str(), "person" | "topic" | "project" | "task");
            if !builtin_ok && self.entity_types_get(rk).await?.is_none() {
                return Err(issue(
                    &slug,
                    "bad_ref_kind",
                    format!("ref_kind '{rk}' is neither a built-in kind nor a custom type"),
                ));
            }
        }
        let ts = now_iso();
        crate::pgq::query(
            "INSERT INTO entity_fields (id, type_id, slug, label, field_type, required, position, options, ref_kind, archived, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, FALSE, ?, ?)",
        )
        .bind(new_id("efield"))
        .bind(type_id)
        .bind(&slug)
        .bind(&f.label)
        .bind(ft.as_str())
        .bind(f.required)
        .bind(f.position.unwrap_or(default_pos))
        .bind(to_json(&f.options))
        .bind(&f.ref_kind)
        .bind(&ts)
        .bind(&ts)
        .execute(self.db())
        .await?;
        Ok(())
    }

    async fn entity_field_update(
        &self,
        type_id: &str,
        fp: &EntityFieldPatch,
    ) -> std::result::Result<(), TypeWriteError> {
        let row = crate::pgq::query("SELECT * FROM entity_fields WHERE type_id = ? AND slug = ?")
            .bind(type_id)
            .bind(&fp.slug)
            .fetch_optional(self.db())
            .await?;
        let Some(r) = row else {
            return Err(issue(
                &fp.slug,
                "unknown_field",
                format!("no field '{}' on this type", fp.slug),
            ));
        };
        let label: String = r.try_get("label")?;
        let required: bool = r.try_get("required")?;
        let position: i64 = r.try_get("position")?;
        let archived: bool = r.try_get("archived")?;
        let options = json_vec(r.try_get::<String, _>("options")?.as_str());
        let field_type: String = r.try_get("field_type")?;
        if FieldType::parse(&field_type) == Some(FieldType::Choice) {
            if let Some(opts) = &fp.options {
                if opts.is_empty() {
                    return Err(issue(
                        &fp.slug,
                        "bad_choice",
                        "choice fields need options".into(),
                    ));
                }
            }
        }
        crate::pgq::query(
            "UPDATE entity_fields SET label=?, required=?, position=?, options=?, archived=?, updated_at=? WHERE type_id=? AND slug=?",
        )
        .bind(fp.label.as_deref().unwrap_or(&label))
        .bind(fp.required.unwrap_or(required))
        .bind(fp.position.unwrap_or(position))
        .bind(to_json(fp.options.as_ref().unwrap_or(&options)))
        .bind(fp.archived.unwrap_or(archived))
        .bind(now_iso())
        .bind(type_id)
        .bind(&fp.slug)
        .execute(self.db())
        .await?;
        Ok(())
    }

    /// board_field contract: None, or the slug of a live (non-archived)
    /// choice field of this type. Enforced on every write path that touches it.
    async fn set_board_field(
        &self,
        type_id: &str,
        board_field: Option<String>,
    ) -> std::result::Result<(), TypeWriteError> {
        if let Some(bf) = &board_field {
            let fields = self.entity_fields_of(type_id).await?;
            let ok = fields
                .iter()
                .any(|f| f.slug == *bf && f.field_type == FieldType::Choice && !f.archived);
            if !ok {
                return Err(issue(
                    "board_field",
                    "bad_choice",
                    format!("board_field '{bf}' must be a live choice field"),
                ));
            }
        }
        crate::pgq::query("UPDATE entity_types SET board_field=?, updated_at=? WHERE id=?")
            .bind(&board_field)
            .bind(now_iso())
            .bind(type_id)
            .execute(self.db())
            .await?;
        Ok(())
    }

    pub(crate) async fn entity_fields_of(&self, type_id: &str) -> Result<Vec<EntityField>> {
        let rows = crate::pgq::query(
            "SELECT * FROM entity_fields WHERE type_id = ? ORDER BY position, created_at",
        )
        .bind(type_id)
        .fetch_all(self.db())
        .await?;
        rows.iter()
            .map(|r| {
                let ft: String = r.try_get("field_type")?;
                Ok(EntityField {
                    id: r.try_get("id")?,
                    slug: r.try_get("slug")?,
                    label: r.try_get("label")?,
                    // Registry rows are only written through FieldType::parse,
                    // so an unparseable stored value is corruption — surface it.
                    field_type: FieldType::parse(&ft)
                        .ok_or_else(|| anyhow::anyhow!("corrupt field_type '{ft}'"))?,
                    required: r.try_get("required")?,
                    position: r.try_get("position")?,
                    options: json_vec(r.try_get::<String, _>("options")?.as_str()),
                    ref_kind: r.try_get("ref_kind")?,
                    archived: r.try_get("archived")?,
                })
            })
            .collect()
    }

    async fn assemble_type_view(&self, r: &sqlx::postgres::PgRow) -> Result<EntityTypeView> {
        let id: String = r.try_get("id")?;
        let fields = self.entity_fields_of(&id).await?;
        Ok(EntityTypeView {
            id,
            slug: r.try_get("slug")?,
            name: r.try_get("name")?,
            name_plural: r.try_get("name_plural")?,
            description: r.try_get("description")?,
            icon: r.try_get("icon")?,
            color: r.try_get("color")?,
            board_field: r.try_get("board_field")?,
            archived: r.try_get("archived")?,
            created_by: r.try_get("created_by")?,
            created_at: r.try_get("created_at")?,
            updated_at: r.try_get("updated_at")?,
            fields,
        })
    }
}
