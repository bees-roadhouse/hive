use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::{Extension, Router};
use serde::Deserialize;

use crate::error::ApiResult;
use crate::middleware::AuthCtx;
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/mail", get(list))
        .route("/api/mail/messages", get(list))
        .route("/api/mail/search", get(search))
        .route("/api/mail/thread/{thread_id}", get(thread))
        .route("/api/mail/accounts", get(accounts))
}

#[derive(Deserialize)]
struct MailQuery {
    q: Option<String>,
    query: Option<String>,
    account_id: Option<String>,
    limit: Option<i64>,
}

fn viewer(ctx: &AuthCtx) -> Option<&str> {
    if ctx.is_admin() {
        None
    } else {
        Some(ctx.namespace_user())
    }
}

async fn list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<MailQuery>,
) -> ApiResult {
    Ok(Json(
        s.mail_messages_list(
            viewer(&ctx),
            q.query.as_deref().or(q.q.as_deref()),
            q.account_id.as_deref(),
            q.limit.unwrap_or(50),
        )
        .await?,
    )
    .into_response())
}

async fn search(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<MailQuery>,
) -> ApiResult {
    Ok(Json(
        s.mail_search(
            &q.q.unwrap_or_default(),
            viewer(&ctx),
            q.limit.unwrap_or(50),
        )
        .await?,
    )
    .into_response())
}

async fn thread(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(thread_id): Path<String>,
) -> ApiResult {
    Ok(Json(s.mail_thread_get(&thread_id, viewer(&ctx)).await?).into_response())
}

async fn accounts(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    Ok(Json(s.mail_accounts_list(viewer(&ctx)).await?).into_response())
}
