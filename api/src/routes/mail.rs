use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::Deserialize;

use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::Store;

/// The whole mail surface ships dark until the operator flips this. Routes
/// answer 404 (not 403) when disabled so the feature's existence isn't an
/// oracle.
pub fn mail_enabled() -> bool {
    std::env::var("HIVE_MAIL_ENABLED")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/mail", get(list))
        .route("/api/mail/messages", get(list))
        .route("/api/mail/search", get(search))
        .route("/api/mail/thread/{thread_id}", get(thread))
        .route("/api/mail/accounts", get(accounts).post(account_create))
        .route("/api/mail/accounts/manage", get(accounts_manage))
        .route("/api/mail/accounts/{id}", delete(account_delete))
        .route("/api/mail/accounts/{id}/enabled", post(account_set_enabled))
        .route("/api/mail/accounts/{id}/resync", post(account_resync))
        .route("/api/mail/accounts/{id}/mailboxes", get(account_mailboxes))
        .route("/api/mail/mailboxes/{id}/ingest", post(mailbox_set_ingest))
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

fn gate() -> Option<Response> {
    if mail_enabled() {
        None
    } else {
        Some(not_found())
    }
}

/// Owner-or-admin: management actions on an account belong to its namespace.
fn may_manage(ctx: &AuthCtx, owner: &str) -> bool {
    ctx.is_admin() || ctx.namespace_user() == owner
}

async fn list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<MailQuery>,
) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
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
    if let Some(resp) = gate() {
        return Ok(resp);
    }
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
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    Ok(Json(s.mail_thread_get(&thread_id, viewer(&ctx)).await?).into_response())
}

async fn accounts(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    Ok(Json(s.mail_accounts_list(viewer(&ctx)).await?).into_response())
}

// ---- account management (the connect surface) ----

#[derive(Deserialize)]
struct ConnectAccount {
    address: String,
    jmap_url: String,
    /// Login principal when it differs from the address (Stalwart accounts).
    username: Option<String>,
    secret: String,
    /// Admin-only override: connect on behalf of another namespace (how the
    /// household onboards).
    owner: Option<String>,
}

/// Admin-only in v1: `jmap_url` is a server-side fetch target (SSRF into the
/// LAN) and the flow stores a live mailbox credential.
async fn account_create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(body): Json<ConnectAccount>,
) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    let address = body.address.trim().to_string();
    let jmap_url = body.jmap_url.trim().trim_end_matches('/').to_string();
    if address.is_empty() || jmap_url.is_empty() || body.secret.is_empty() {
        return Ok(err(
            axum::http::StatusCode::BAD_REQUEST,
            "address, jmap_url, and secret are required",
        ));
    }
    if !jmap_url.starts_with("https://") && !jmap_url.starts_with("http://") {
        return Ok(err(
            axum::http::StatusCode::BAD_REQUEST,
            "jmap_url must be an http(s) URL",
        ));
    }
    let username = body
        .username
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or(&address)
        .to_string();

    // Validate the credential and capture the JMAP account id BEFORE storing
    // anything: a typo'd secret should fail here, not as a poisoned account
    // that backs off forever.
    let jmap_account_id = match discover_account_id(&jmap_url, &username, &body.secret).await {
        Ok(id) => id,
        Err(detail) => {
            return Ok(err(
                axum::http::StatusCode::BAD_REQUEST,
                &format!("JMAP session discovery failed: {detail}"),
            ));
        }
    };

    let owner = body
        .owner
        .as_deref()
        .map(str::trim)
        .filter(|o| !o.is_empty())
        .unwrap_or(ctx.namespace_user())
        .to_string();
    let view = s
        .mail_account_create(
            &owner,
            &address,
            &jmap_url,
            Some(&username)
                .filter(|u| **u != address)
                .map(|u| u.as_str()),
            &jmap_account_id,
            &body.secret,
        )
        .await?;
    Ok(Json(view).into_response())
}

/// RFC 8620 session discovery: GET {jmap_url}/.well-known/jmap with Basic
/// auth, take the primary mail account. Kept as a small hand-rolled request —
/// the api crate deliberately does not depend on jmap-sync.
async fn discover_account_id(
    jmap_url: &str,
    username: &str,
    secret: &str,
) -> Result<String, String> {
    let auth = format!("Basic {}", B64.encode(format!("{username}:{secret}")));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(format!("{jmap_url}/.well-known/jmap"))
        .header(reqwest::header::AUTHORIZATION, auth)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("server answered {status}"));
    }
    let session: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    session
        .get("primaryAccounts")
        .and_then(|p| p.get("urn:ietf:params:jmap:mail"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            session
                .get("accounts")
                .and_then(|a| a.as_object())
                .and_then(|o| o.keys().next().cloned())
        })
        .ok_or_else(|| "session exposes no mail account".to_string())
}

async fn accounts_manage(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    Ok(Json(s.mail_accounts_admin_list(viewer(&ctx)).await?).into_response())
}

async fn account_delete(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    let Some(owner) = s.mail_account_owner(&id).await? else {
        return Ok(not_found());
    };
    if !may_manage(&ctx, &owner) {
        return Ok(forbidden());
    }
    s.mail_account_delete(&id).await?;
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct SetEnabled {
    enabled: bool,
}

async fn account_set_enabled(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(body): Json<SetEnabled>,
) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    let Some(owner) = s.mail_account_owner(&id).await? else {
        return Ok(not_found());
    };
    if !may_manage(&ctx, &owner) {
        return Ok(forbidden());
    }
    s.mail_account_set_enabled(&id, body.enabled).await?;
    Ok(Json(serde_json::json!({"ok": true, "enabled": body.enabled})).into_response())
}

/// Admin-only: forcing a full reconciliation is an ops action.
async fn account_resync(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    if !s.mail_account_force_resync(&id).await? {
        return Ok(not_found());
    }
    Ok(Json(serde_json::json!({"ok": true})).into_response())
}

async fn account_mailboxes(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    let Some(owner) = s.mail_account_owner(&id).await? else {
        return Ok(not_found());
    };
    if !may_manage(&ctx, &owner) {
        return Ok(forbidden());
    }
    Ok(Json(s.mail_mailboxes_list(&id).await?).into_response())
}

#[derive(Deserialize)]
struct SetIngest {
    ingest: bool,
}

async fn mailbox_set_ingest(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(body): Json<SetIngest>,
) -> ApiResult {
    if let Some(resp) = gate() {
        return Ok(resp);
    }
    let Some(owner) = s.mail_mailbox_owner(&id).await? else {
        return Ok(not_found());
    };
    if !may_manage(&ctx, &owner) {
        return Ok(forbidden());
    }
    s.mail_mailbox_set_ingest(&id, body.ingest).await?;
    Ok(Json(serde_json::json!({"ok": true, "ingest": body.ingest})).into_response())
}
