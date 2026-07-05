// People + profile + identities (server.ts people/profile sections plus the
// Rust-branch identities API).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::{Extension, Router};
use hive_shared::{ActorKind, IdentityPatch, NewIdentity, PersonPatch, ProfilePatch};
use serde::Deserialize;
use serde_json::json;

use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::{can_act_for_identity, AuthCtx};
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/people", get(people_list).post(people_create))
        .route("/api/people/{slug}", get(people_get).patch(people_update))
        .route(
            "/api/profile/{actor}",
            get(profile_get).post(profile_update),
        )
        .route(
            "/api/identities",
            get(identities_list).post(identities_create),
        )
        .route(
            "/api/identities/{id}",
            get(identities_get)
                .patch(identities_update)
                .delete(identities_remove),
        )
        .route(
            "/api/identities/resolve/{platform}/{platform_id}",
            get(identities_resolve),
        )
        .route("/api/inbox/{recipient}", get(inbox_list))
        .route("/api/inbox/{recipient}/read", post(inbox_read_all))
        .route("/api/inbox/item/{id}/read", post(inbox_read_item))
}

// ---- people ----

async fn people_list(State(s): State<Store>) -> ApiResult {
    Ok(Json(s.people_list().await?).into_response())
}

async fn people_get(State(s): State<Store>, Path(slug): Path<String>) -> ApiResult {
    match s.people_get(&slug).await? {
        Some(p) => Ok(Json(p).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct PersonCreateBody {
    name: Option<String>,
    kind: Option<ActorKind>,
}

async fn people_create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(body): Json<PersonCreateBody>,
) -> ApiResult {
    let Some(name) = body.name.filter(|n| !n.trim().is_empty()) else {
        return Ok(err(StatusCode::BAD_REQUEST, "name required"));
    };
    let p = s
        .people_create(&name, body.kind.unwrap_or(ActorKind::Human), ctx.actor())
        .await?;
    Ok((StatusCode::CREATED, Json(p)).into_response())
}

async fn people_update(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(slug): Path<String>,
    Json(patch): Json<PersonPatch>,
) -> ApiResult {
    let Some(target) = s.people_get(&slug).await? else {
        return Ok(not_found());
    };
    // Editable by an admin, the AI's owner, or the identity itself.
    let me = ctx.actor.as_deref();
    if !ctx.is_admin() && target.owner.as_deref() != me && Some(target.slug.as_str()) != me {
        return Ok(forbidden());
    }
    match s.people_update(&slug, patch, ctx.actor()).await? {
        Some(p) => Ok(Json(p).into_response()),
        None => Ok(not_found()),
    }
}

// ---- profile ----

async fn profile_get(State(s): State<Store>, Path(actor): Path<String>) -> ApiResult {
    match s.profile_get(&actor).await? {
        Some(p) => Ok(Json(p).into_response()),
        None => Ok(not_found()),
    }
}

async fn profile_update(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(actor): Path<String>,
    Json(patch): Json<ProfilePatch>,
) -> ApiResult {
    if !can_edit_actor_profile(&s, &ctx, &actor).await? {
        return Ok(forbidden());
    }
    Ok(Json(s.profile_update(&actor, patch, ctx.actor()).await?).into_response())
}

async fn can_edit_actor_profile(s: &Store, ctx: &AuthCtx, actor: &str) -> anyhow::Result<bool> {
    if ctx.is_admin() || actor == ctx.actor() {
        return Ok(true);
    }
    if ctx.principal == Some("session") {
        let Some(target) = s.people_get(actor).await? else {
            return Ok(false);
        };
        return Ok(target.owner.as_deref() == Some(ctx.actor()));
    }
    Ok(false)
}

// ---- inbox ----

#[derive(Deserialize)]
struct InboxQuery {
    unread: Option<String>,
}

async fn inbox_list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(recipient): Path<String>,
    Query(q): Query<InboxQuery>,
) -> ApiResult {
    // An inbox is private to its recipient — snippets quote entries other
    // viewers may not see (DIRECTION.md Phase 0 item 3).
    if !can_act_for_identity(&s, &ctx, &recipient).await? {
        return Ok(forbidden());
    }
    let unread = matches!(q.unread.as_deref(), Some("1") | Some("true"));
    Ok(Json(s.inbox_list(&recipient, unread).await?).into_response())
}

async fn inbox_read_all(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(recipient): Path<String>,
) -> ApiResult {
    if !can_act_for_identity(&s, &ctx, &recipient).await? {
        return Ok(forbidden());
    }
    let marked = s.inbox_mark_all_read(&recipient).await?;
    Ok(Json(json!({"marked": marked})).into_response())
}

async fn inbox_read_item(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    // Resolve the item's recipient and gate on it — marking another actor's
    // notifications read is cross-namespace tampering. Missing and foreign
    // ids answer the same {"marked": false} so the route doesn't oracle
    // which ids exist in others' inboxes (same as the MCP twin).
    let allowed = match s.inbox_recipient(&id).await? {
        Some(recipient) => can_act_for_identity(&s, &ctx, &recipient).await?,
        None => false,
    };
    if !allowed {
        return Ok(Json(json!({"marked": false})).into_response());
    }
    let marked = s.inbox_mark_read(&id).await? > 0;
    Ok(Json(json!({"marked": marked})).into_response())
}

// ---- identities ----

#[derive(Deserialize)]
struct IdentitiesQuery {
    actor: Option<String>,
}

async fn identities_list(State(s): State<Store>, Query(q): Query<IdentitiesQuery>) -> ApiResult {
    let list = match q.actor {
        Some(actor) => s.identities_for_actor(&actor).await?,
        None => s.identities_list().await?,
    };
    Ok(Json(list).into_response())
}

async fn identities_get(State(s): State<Store>, Path(id): Path<String>) -> ApiResult {
    match s.identities_get(&id).await? {
        Some(i) => Ok(Json(i).into_response()),
        None => Ok(not_found()),
    }
}

async fn identities_create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(body): Json<NewIdentity>,
) -> ApiResult {
    if s.people_get(&body.actor).await?.is_none() {
        return Ok(err(
            StatusCode::NOT_FOUND,
            &format!("actor '{}' not found", body.actor),
        ));
    }
    let item = s.identities_create(body, ctx.actor()).await?;
    Ok((StatusCode::CREATED, Json(item)).into_response())
}

async fn identities_update(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(patch): Json<IdentityPatch>,
) -> ApiResult {
    match s.identities_update(&id, patch, ctx.actor()).await? {
        Some(i) => Ok(Json(i).into_response()),
        None => Ok(not_found()),
    }
}

async fn identities_remove(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if s.identities_remove(&id, ctx.actor()).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok(not_found())
    }
}

async fn identities_resolve(
    State(s): State<Store>,
    Path((platform, platform_id)): Path<(String, String)>,
) -> ApiResult {
    match s.identities_resolve(&platform, &platform_id).await? {
        Some(actor) => Ok(Json(json!({"actor": actor})).into_response()),
        None => Ok((StatusCode::NOT_FOUND, Json(json!({"actor": null}))).into_response()),
    }
}
