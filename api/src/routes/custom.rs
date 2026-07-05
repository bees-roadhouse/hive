// Custom entity types + instances REST surface. Types are admin-defined
// (same gate as sources/actors); instances are open to any authenticated
// actor with visibility scoped through AuthCtx exactly like journal reads.
// Validation failures return the structured issue list:
//   400 {"error": "validation failed", "issues": [{field, code, message}]}

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::{Extension, Router};
use hive_shared::{CustomEntityPatch, EntityTypePatch, NewCustomEntity, NewEntityType};
use serde_json::json;

use super::admin::require_admin_actor;
use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::custom_entities::{EntityFilter, EntityWriteError};
use crate::store::entity_types::TypeWriteError;
use crate::store::entity_validation::FieldIssue;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/entity-types", get(types_list).post(types_create))
        .route(
            "/api/entity-types/{id_or_slug}",
            get(types_get).patch(types_update).delete(types_delete),
        )
        .route("/api/entities", get(entities_list).post(entities_create))
        .route(
            "/api/entities/{id}",
            get(entities_get)
                .patch(entities_update)
                .delete(entities_delete),
        )
}

fn issues_response(issues: Vec<FieldIssue>) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": "validation failed", "issues": issues})),
    )
        .into_response()
}

/// The two store error enums flatten to the same HTTP shapes.
fn entity_error(e: EntityWriteError) -> Result<axum::response::Response, crate::error::ApiError> {
    Ok(match e {
        EntityWriteError::Issues(issues) => issues_response(issues),
        EntityWriteError::UnknownType => err(StatusCode::NOT_FOUND, "unknown entity type"),
        EntityWriteError::ArchivedType => err(
            StatusCode::CONFLICT,
            "type is archived; unarchive it to add instances",
        ),
        EntityWriteError::Other(e) => return Err(e.into()),
    })
}

fn type_error(e: TypeWriteError) -> Result<axum::response::Response, crate::error::ApiError> {
    Ok(match e {
        TypeWriteError::Issues(issues) => issues_response(issues),
        TypeWriteError::Other(e) => return Err(e.into()),
    })
}

// ---- entity types ----

#[derive(serde::Deserialize)]
struct TypesListQuery {
    include_archived: Option<String>,
}

async fn types_list(State(s): State<Store>, Query(q): Query<TypesListQuery>) -> ApiResult {
    let include = matches!(q.include_archived.as_deref(), Some("1") | Some("true"));
    Ok(Json(s.entity_types_list(include).await?).into_response())
}

async fn types_create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(input): Json<NewEntityType>,
) -> ApiResult {
    if !require_admin_actor(&s, &ctx).await? {
        return Ok(forbidden());
    }
    match s.entity_types_create(input, ctx.actor()).await {
        Ok(view) => Ok((StatusCode::CREATED, Json(view)).into_response()),
        Err(e) => type_error(e),
    }
}

async fn types_get(State(s): State<Store>, Path(id_or_slug): Path<String>) -> ApiResult {
    match s.entity_types_get(&id_or_slug).await? {
        Some(view) => Ok(Json(view).into_response()),
        None => Ok(not_found()),
    }
}

async fn types_update(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id_or_slug): Path<String>,
    Json(patch): Json<EntityTypePatch>,
) -> ApiResult {
    if !require_admin_actor(&s, &ctx).await? {
        return Ok(forbidden());
    }
    match s.entity_types_update(&id_or_slug, patch, ctx.actor()).await {
        Ok(Some(view)) => Ok(Json(view).into_response()),
        Ok(None) => Ok(not_found()),
        Err(e) => type_error(e),
    }
}

async fn types_delete(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id_or_slug): Path<String>,
) -> ApiResult {
    if !require_admin_actor(&s, &ctx).await? {
        return Ok(forbidden());
    }
    match s.entity_types_delete(&id_or_slug, ctx.actor()).await? {
        None => Ok(not_found()),
        Some(false) => Ok(err(
            StatusCode::CONFLICT,
            "type has instances; archive instead",
        )),
        Some(true) => Ok(StatusCode::NO_CONTENT.into_response()),
    }
}

// ---- entity instances ----

async fn entities_list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<HashMap<String, String>>,
) -> ApiResult {
    let Some(type_slug) = q.get("type").filter(|t| !t.is_empty()).cloned() else {
        return Ok(err(StatusCode::BAD_REQUEST, "type query param is required"));
    };
    let filter = EntityFilter {
        type_slug,
        limit: q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(100),
        offset: q.get("offset").and_then(|v| v.parse().ok()).unwrap_or(0),
        sort: q.get("sort").filter(|v| !v.is_empty()).cloned(),
        desc: !matches!(q.get("dir").map(String::as_str), Some("asc")),
        // f.<field_slug>=value equality filters, empty values skipped
        // (the Node falsy-filter convention).
        fields: q
            .iter()
            .filter_map(|(k, v)| {
                let slug = k.strip_prefix("f.")?;
                (!v.is_empty()).then(|| (slug.to_string(), v.clone()))
            })
            .collect(),
    };
    match s.custom_entities_list(&filter, &ctx.visibility()).await {
        Ok(items) => Ok(Json(items).into_response()),
        Err(e) => entity_error(e),
    }
}

async fn entities_create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(input): Json<NewCustomEntity>,
) -> ApiResult {
    if input.title.trim().is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "title is required"));
    }
    match s
        .custom_entities_create(input, ctx.actor(), ctx.namespace_owner())
        .await
    {
        Ok(e) => Ok((StatusCode::CREATED, Json(e)).into_response()),
        Err(e) => entity_error(e),
    }
}

async fn entities_get(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    // Invisible reads 404 (inside the store), never 403 — no existence leak.
    match s.custom_entities_get(&id, &ctx.visibility()).await? {
        Some(e) => Ok(Json(e).into_response()),
        None => Ok(not_found()),
    }
}

async fn entities_update(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(patch): Json<CustomEntityPatch>,
) -> ApiResult {
    match s
        .custom_entities_update(
            &id,
            patch,
            ctx.actor(),
            &ctx.visibility(),
            ctx.namespace_owner(),
        )
        .await
    {
        Ok(Some(e)) => Ok(Json(e).into_response()),
        Ok(None) => Ok(not_found()),
        Err(e) => entity_error(e),
    }
}

async fn entities_delete(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    match s
        .custom_entities_delete(&id, ctx.actor(), &ctx.visibility())
        .await?
    {
        Some(()) => Ok(StatusCode::NO_CONTENT.into_response()),
        None => Ok(not_found()),
    }
}
