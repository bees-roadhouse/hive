use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Sse, sse::Event},
    routing::{delete, get, patch, post},
    Json, Router, Extension,
};
use futures::stream::{self, Stream};
use hive_shared::*;
use serde::Deserialize;
use serde_json::json;
use std::{convert::Infallible, sync::Arc, time::Duration};
use tokio::sync::broadcast;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::{auth, store::Store};

pub type AppState = Arc<AppStateInner>;

pub struct AppStateInner {
    pub store: Store,
    pub bus: broadcast::Sender<WireEvent>,
}

impl AppStateInner {
    pub fn new(store: Store) -> Self {
        let (tx, _rx) = broadcast::channel::<WireEvent>(1024);
        Self { store, bus: tx }
    }
}

// ---- auth context ----

#[derive(Clone, Debug)]
pub struct AuthCtx {
    pub actor: String,
    pub principal: Option<String>,
    pub role: Option<UserRole>,
    pub session_token: Option<String>,
}

impl AuthCtx {
    pub fn is_admin(&self) -> bool {
        self.role == Some(UserRole::Admin)
    }
}

// ---- CORS ----

pub fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(Any)
        .allow_methods(Any)
        .allow_credentials(true)
}

// ---- router ----

pub fn router(state: AppState) -> Router {
    Router::new()
        // Health
        .route("/api/healthz", get(healthz))
        .route("/api/onboarding/status", get(onboarding_status))
        .route("/api/onboarding", post(onboarding_complete))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/logout", post(auth_logout))
        .route("/api/auth/me", get(auth_me))
        .route("/api/auth/config", get(auth_config))
        // People / identities
        .route("/api/people", get(people_list))
        .route("/api/people/:slug", get(people_get))
        .route("/api/identities", get(identities_list).post(identities_create))
        .route("/api/identities/:id", get(identities_get).patch(identities_update).delete(identities_remove))
        .route("/api/identities/resolve/:platform/:platform_id", get(identities_resolve))
        // Projects
        .route("/api/projects", get(projects_list).post(projects_create))
        .route("/api/projects/:slug", get(projects_get))
        // Journal
        .route("/api/journal", get(journal_list).post(journal_create))
        .route("/api/journal/:id", get(journal_get))
        // Tasks
        .route("/api/tasks", get(tasks_list).post(tasks_create))
        .route("/api/tasks/:id", get(tasks_get).patch(tasks_update))
        // Decisions
        .route("/api/decisions", get(decisions_list))
        .route("/api/decisions/:id", get(decisions_get))
        // Inbox
        .route("/api/inbox", get(inbox_list))
        .route("/api/inbox/read", post(inbox_mark_read))
        .route("/api/inbox/read-all", post(inbox_mark_all_read))
        // Profile
        .route("/api/profile/:actor", get(profile_get).post(profile_update))
        // Recall
        .route("/api/recall", post(recall))
        // Search
        .route("/api/search", get(search))
        // Wire / SSE
        .route("/api/wire", get(wire_log))
        .route("/api/events", get(sse_events))
        // MCP
        .route("/mcp", post(mcp_handler))
        // Admin
        .route("/api/users", get(users_list).post(users_create))
        .route("/api/tokens", get(tokens_list).post(tokens_create))
        .route("/api/tokens/:id", delete(tokens_remove))
        // Fallback
        .layer(cors_layer())
        .with_state(state)
}

// ---- health ----

async fn healthz() -> impl IntoResponse {
    Json(json!({"ok": true, "service": "hive-rust-api", "version": APP_VERSION}))
}

// ---- onboarding ----

#[derive(Deserialize)]
struct OnboardingBody {
    instance_name: String,
    admin_name: String,
    admin_email: String,
    password: String,
}

async fn onboarding_status(State(s): State<AppState>) -> impl IntoResponse {
    let completed = s.store.users_count().await.unwrap_or(0) > 0;
    let instance_name = s.store.config_get("instance.name").await.ok().flatten();
    Json(OnboardingStatus { completed, instance_name, version: APP_VERSION.to_string() })
}

async fn onboarding_complete(State(s): State<AppState>, Json(body): Json<OnboardingBody>) -> impl IntoResponse {
    if s.store.users_count().await.unwrap_or(0) > 0 {
        return (StatusCode::CONFLICT, Json(json!({"error": "already_completed"})));
    }
    if body.password.len() < 8 {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "password must be at least 8 characters"})));
    }
    let _ = s.store.config_set("instance.name", &body.instance_name).await;
    match s.store.users_create(&body.admin_name, &body.admin_email, &body.password, UserRole::Admin, Some(&slugify(&body.admin_name)), "system").await {
        Ok(user) => {
            let session = s.store.sessions_create(&user.id).await.unwrap_or_default();
            (StatusCode::CREATED, Json(json!({ "user": user, "session": session })))
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

// ---- auth ----

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

async fn auth_login(State(s): State<AppState>, Json(body): Json<LoginBody>) -> impl IntoResponse {
    match s.store.users_authenticate(&body.email, &body.password).await {
        Ok(Some(user)) => {
            let safe = SafeUser { id: user.id, actor: user.actor, email: user.email, name: user.name, role: user.role };
            let session = s.store.sessions_create(&safe.id).await.unwrap_or_default();
            (StatusCode::OK, Json(json!({ "user": safe, "session": session })))
        }
        _ => (StatusCode::UNAUTHORIZED, Json(json!({"error": "invalid credentials"}))),
    }
}

async fn auth_logout(State(s): State<AppState>, Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    if let Some(token) = body.get("session").and_then(|v| v.as_str()) {
        let _ = s.store.sessions_destroy(token).await;
    }
    Json(json!({"ok": true}))
}

async fn auth_me(State(s): State<AppState>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    // Try session cookie first
    let actor = resolve_actor_from_headers(&s.store, &headers).await;
    let user = if let Some(ref a) = actor {
        s.store.users_list().await.ok().and_then(|users| users.into_iter().find(|u| u.actor == *a))
    } else {
        None
    };
    Json(AuthMe { user, principal: actor.map(|_| Principal::Session) })
}

async fn auth_config(State(s): State<AppState>) -> impl IntoResponse {
    let oidc = s.store.config_get("oidc.issuer").await.ok().flatten().is_some();
    let instance_name = s.store.config_get("instance.name").await.ok().flatten();
    Json(AuthConfig { oidc, instance_name })
}

async fn resolve_actor_from_headers(store: &Store, headers: &axum::http::HeaderMap) -> Option<String> {
    // Bearer token
    if let Some(auth) = headers.get("authorization") {
        if let Ok(s) = auth.to_str() {
            if let Some(token) = s.strip_prefix("Bearer ") {
                if let Ok(Some(actor)) = store.tokens_resolve(token).await {
                    return Some(actor);
                }
            }
        }
    }
    // Session cookie
    if let Some(cookie) = headers.get("cookie") {
        if let Ok(s) = cookie.to_str() {
            for part in s.split(';') {
                let part = part.trim();
                if let Some(val) = part.strip_prefix("hive_session=") {
                    if let Ok(Some(user)) = store.sessions_resolve(val).await {
                        return Some(user.actor);
                    }
                }
            }
        }
    }
    None
}

// ---- people ----

async fn people_list(State(s): State<AppState>) -> impl IntoResponse {
    match s.store.people_list().await {
        Ok(list) => Json(list).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn people_get(State(s): State<AppState>, Path(slug): Path<String>) -> impl IntoResponse {
    match s.store.people_get(&slug).await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- identities ----

#[derive(Deserialize)]
struct IdentitiesQuery {
    actor: Option<String>,
}

async fn identities_list(State(s): State<AppState>, Query(q): Query<IdentitiesQuery>) -> impl IntoResponse {
    let list = if let Some(actor) = q.actor {
        s.store.identities_for_actor(&actor).await
    } else {
        s.store.identities_list().await
    };
    match list {
        Ok(items) => Json(items).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn identities_get(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match s.store.identities_get(&id).await {
        Ok(Some(item)) => Json(item).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct IdentityCreateBody {
    platform: String,
    platform_id: String,
    actor: String,
}

async fn identities_create(State(s): State<AppState>, Json(body): Json<IdentityCreateBody>) -> impl IntoResponse {
    if let Ok(None) = s.store.people_get(&body.actor).await {
        return (StatusCode::NOT_FOUND, Json(json!({"error": format!("actor '{}' not found", body.actor)})));
    }
    match s.store.identities_create(NewIdentity { platform: body.platform, platform_id: body.platform_id, actor: body.actor }, "api").await {
        Ok(item) => (StatusCode::CREATED, Json(item)),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

#[derive(Deserialize)]
struct IdentityUpdateBody {
    actor: String,
}

async fn identities_update(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<IdentityUpdateBody>) -> impl IntoResponse {
    match s.store.identities_update(&id, IdentityPatch { actor: Some(body.actor) }, "api").await {
        Ok(Some(item)) => (StatusCode::OK, Json(item)),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"error": "not found"}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn identities_remove(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match s.store.identities_remove(&id, "api").await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn identities_resolve(State(s): State<AppState>, Path((platform, platform_id)): Path<(String, String)>) -> impl IntoResponse {
    match s.store.identities_resolve(&platform, &platform_id).await {
        Ok(Some(actor)) => Json(json!({"actor": actor})).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, Json(json!({"actor": serde_json::Value::Null}))).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- projects ----

async fn projects_list(State(s): State<AppState>) -> impl IntoResponse {
    match s.store.projects_list().await {
        Ok(list) => Json(list).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ProjectCreateBody {
    name: String,
}

async fn projects_create(State(s): State<AppState>, Json(body): Json<ProjectCreateBody>) -> impl IntoResponse {
    match s.store.projects_ensure(&body.name).await {
        Ok(p) => (StatusCode::CREATED, Json(p)),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn projects_get(State(s): State<AppState>, Path(slug): Path<String>) -> impl IntoResponse {
    match s.store.projects_by_slug(&slug).await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- journal ----

#[derive(Deserialize)]
struct JournalListQuery {
    author: Option<String>,
    limit: Option<i64>,
}

async fn journal_list(State(s): State<AppState>, Query(q): Query<JournalListQuery>) -> impl IntoResponse {
    match s.store.journal_list(q.limit.unwrap_or(100), q.author.as_deref()).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn journal_get(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match s.store.journal_get(&id).await {
        Ok(Some(e)) => Json(e).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn journal_create(State(s): State<AppState>, Json(body): Json<NewJournalEntry>) -> impl IntoResponse {
    match s.store.journal_create(body).await {
        Ok(entry) => (StatusCode::CREATED, Json(entry)),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

// ---- tasks ----

#[derive(Deserialize)]
struct TasksQuery {
    project: Option<String>,
    status: Option<String>,
}

async fn tasks_list(State(s): State<AppState>, Query(q): Query<TasksQuery>) -> impl IntoResponse {
    let status = q.status.and_then(|s| match s.as_str() { "todo" => Some(TaskStatus::Todo), "doing" => Some(TaskStatus::Doing), "blocked" => Some(TaskStatus::Blocked), "done" => Some(TaskStatus::Done), _ => None });
    match s.store.tasks_list(q.project.as_deref(), status).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct TaskCreateBody {
    title: String,
    body: String,
}

async fn tasks_create(State(s): State<AppState>, Json(body): Json<TaskCreateBody>) -> impl IntoResponse {
    // TODO: get actor from auth context
    match s.store.tasks_create(&body.title, &body.body, "anon").await {
        Ok(t) => (StatusCode::CREATED, Json(t)),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn tasks_get(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match s.store.tasks_get(&id).await {
        Ok(Some(t)) => Json(t).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn tasks_update(State(s): State<AppState>, Path(id): Path<String>, Json(body): Json<TaskPatch>) -> impl IntoResponse {
    match s.store.tasks_update(&id, body, "anon").await {
        Ok(Some(t)) => Json(t).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- decisions ----

async fn decisions_list(State(s): State<AppState>) -> impl IntoResponse {
    match s.store.decisions_list().await {
        Ok(list) => Json(list).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn decisions_get(State(s): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match s.store.decisions_get(&id).await {
        Ok(Some(d)) => Json(d).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- inbox ----

#[derive(Deserialize)]
struct InboxQuery {
    unread_only: Option<bool>,
}

async fn inbox_list(State(s): State<AppState>, headers: axum::http::HeaderMap, Query(q): Query<InboxQuery>) -> impl IntoResponse {
    let actor = resolve_actor_from_headers(&s.store, &headers).await.unwrap_or_else(|| "anon".to_string());
    match s.store.inbox_list(&actor, q.unread_only.unwrap_or(false)).await {
        Ok(list) => Json(list).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct InboxReadBody {
    id: String,
}

async fn inbox_mark_read(State(s): State<AppState>, Json(body): Json<InboxReadBody>) -> impl IntoResponse {
    match s.store.inbox_mark_read(&body.id).await {
        Ok(true) => Json(json!({"ok": true})).into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn inbox_mark_all_read(State(s): State<AppState>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    let actor = resolve_actor_from_headers(&s.store, &headers).await.unwrap_or_else(|| "anon".to_string());
    match s.store.inbox_mark_all_read(&actor).await {
        Ok(n) => Json(json!({"marked": n})).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- profile ----

async fn profile_get(State(s): State<AppState>, Path(actor): Path<String>) -> impl IntoResponse {
    match s.store.profile_get(&actor).await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => (StatusCode::NOT_FOUND, "not found").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn profile_update(State(s): State<AppState>, Path(actor): Path<String>, Json(body): Json<ProfilePatch>) -> impl IntoResponse {
    match s.store.profile_update(&actor, body, "api").await {
        Ok(p) => Json(p).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- recall ----

#[derive(Deserialize)]
struct RecallBody {
    identity: Option<String>,
    peer: Option<String>,
    peer_platform: Option<String>,
    peer_platform_id: Option<String>,
    query: Option<String>,
    budget: Option<usize>,
}

async fn recall(State(s): State<AppState>, headers: axum::http::HeaderMap, Json(body): Json<RecallBody>) -> impl IntoResponse {
    let identity = body.identity.or_else(|| resolve_actor_from_headers(&s.store, &headers).await).unwrap_or_else(|| "anon".to_string());
    match s.store.recall(&identity, body.peer.as_deref(), body.peer_platform.as_deref(), body.peer_platform_id.as_deref(), body.query.as_deref(), body.budget).await {
        Ok(result) => Json(result).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- search ----

#[derive(Deserialize)]
struct SearchQuery {
    q: String,
    limit: Option<i64>,
}

async fn search(State(s): State<AppState>, Query(q): Query<SearchQuery>) -> impl IntoResponse {
    match s.store.search(&q.q, q.limit.unwrap_or(20)).await {
        Ok(hits) => Json(hits).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- wire log ----

async fn wire_log(State(s): State<AppState>) -> impl IntoResponse {
    match s.store.wire_log(100).await {
        Ok(events) => Json(events).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

// ---- SSE ----

async fn sse_events(State(s): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = s.bus.subscribe();
    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = match serde_json::to_string(&ev) {
                        Ok(d) => d,
                        Err(_) => continue,
                    };
                    yield Ok(Event::default().event("wire").data(data));
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
}

// ---- MCP ----

async fn mcp_handler(State(s): State<AppState>, body: axum::body::Bytes) -> impl IntoResponse {
    // Parse JSON-RPC 2.0 request
    let req: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"jsonrpc": "2.0", "error": {"code": -32700, "message": e.to_string()}, "id": null}))),
    };

    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);

    let result = match method {
        "initialize" => mcp_initialize(),
        "tools/list" => mcp_tools_list(),
        "tools/call" => {
            if let Some(params) = req.get("params") {
                mcp_tools_call(&s.store, params).await
            } else {
                json!({"error": {"code": -32602, "message": "params required"}})
            }
        }
        _ => json!({"error": {"code": -32601, "message": format!("method not found: {}", method)}}),
    };

    Json(json!({"jsonrpc": "2.0", "result": result, "id": id}))
}

fn mcp_initialize() -> serde_json::Value {
    json!({
        "protocolVersion": "2024-11-05",
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "hive-rust-mcp", "version": APP_VERSION }
    })
}

fn mcp_tools_list() -> serde_json::Value {
    json!({
        "tools": [
            {"name": "identity_link", "description": "Link a platform identity to an actor", "inputSchema": {"type": "object", "properties": {"platform": {"type": "string"}, "platform_id": {"type": "string"}, "actor": {"type": "string"}}, "required": ["platform", "platform_id", "actor"]}},
            {"name": "identity_resolve", "description": "Resolve a platform ID to an actor", "inputSchema": {"type": "object", "properties": {"platform": {"type": "string"}, "platform_id": {"type": "string"}}, "required": ["platform", "platform_id"]}},
            {"name": "identity_list", "description": "List linked identities", "inputSchema": {"type": "object", "properties": {"actor": {"type": "string"}}}},
            {"name": "identity_unlink", "description": "Unlink a platform identity", "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}},
            {"name": "recall", "description": "Recall memory for a session", "inputSchema": {"type": "object", "properties": {"identity": {"type": "string"}, "peer": {"type": "string"}, "peer_platform": {"type": "string"}, "peer_platform_id": {"type": "string"}, "query": {"type": "string"}, "budget": {"type": "integer"}}, "required": ["identity"]}},
            {"name": "journal_create", "description": "Create a journal entry", "inputSchema": {"type": "object", "properties": {"author": {"type": "string"}, "body": {"type": "string"}, "tags": {"type": "array", "items": {"type": "string"}}}, "required": ["author", "body"]}},
            {"name": "tasks_list", "description": "List open tasks", "inputSchema": {"type": "object", "properties": {"project": {"type": "string"}, "status": {"type": "string"}}}},
            {"name": "profile_get", "description": "Get an actor's profile card", "inputSchema": {"type": "object", "properties": {"actor": {"type": "string"}}, "required": ["actor"]}},
            {"name": "profile_update", "description": "Update an actor's profile card", "inputSchema": {"type": "object", "properties": {"actor": {"type": "string"}, "display_name": {"type": "string"}, "sections": {"type": "object"}}, "required": ["actor"]}},
            {"name": "inbox_list", "description": "List inbox items", "inputSchema": {"type": "object", "properties": {"actor": {"type": "string"}, "unread_only": {"type": "boolean"}}, "required": ["actor"]}},
            {"name": "search", "description": "Search the journal", "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}, "limit": {"type": "integer"}}, "required": ["q"]}},
        ]
    })
}

async fn mcp_tools_call(store: &Store, params: &serde_json::Value) -> serde_json::Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    match name {
        "identity_link" => {
            let platform = args["platform"].as_str().unwrap_or("");
            let platform_id = args["platform_id"].as_str().unwrap_or("");
            let actor = args["actor"].as_str().unwrap_or("");
            match store.identities_create(NewIdentity { platform: platform.to_string(), platform_id: platform_id.to_string(), actor: actor.to_string() }, "mcp").await {
                Ok(i) => json!({"linked": true, "identity": i}),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "identity_resolve" => {
            let platform = args["platform"].as_str().unwrap_or("");
            let platform_id = args["platform_id"].as_str().unwrap_or("");
            match store.identities_resolve(platform, platform_id).await {
                Ok(actor) => json!({"actor": actor}),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "identity_list" => {
            let actor = args["actor"].as_str();
            let list = if let Some(a) = actor {
                store.identities_for_actor(a).await
            } else {
                store.identities_list().await
            };
            match list {
                Ok(items) => json!({"count": items.len(), "identities": items}),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "identity_unlink" => {
            let id = args["id"].as_str().unwrap_or("");
            match store.identities_remove(id, "mcp").await {
                Ok(removed) => json!({"removed": removed}),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "recall" => {
            let identity = args["identity"].as_str().unwrap_or("anon");
            let peer = args["peer"].as_str();
            let peer_platform = args["peer_platform"].as_str();
            let peer_platform_id = args["peer_platform_id"].as_str();
            let query = args["query"].as_str();
            let budget = args["budget"].as_u64().map(|b| b as usize);
            match store.recall(identity, peer, peer_platform, peer_platform_id, query, budget).await {
                Ok(r) => json!({"brief": r.brief, "tasks": r.tasks.len(), "inbox": r.inbox.len(), "journal": r.journal.len()}),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "journal_create" => {
            let author = args["author"].as_str().unwrap_or("anon").to_string();
            let body = args["body"].as_str().unwrap_or("").to_string();
            let tags: Vec<String> = args["tags"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect()).unwrap_or_default();
            match store.journal_create(NewJournalEntry { author, body, tags }).await {
                Ok(e) => json!(e),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "tasks_list" => {
            let project = args["project"].as_str().map(|s| s.to_string());
            let status = args["status"].as_str().and_then(|s| match s { "todo" => Some(TaskStatus::Todo), "doing" => Some(TaskStatus::Doing), "blocked" => Some(TaskStatus::Blocked), "done" => Some(TaskStatus::Done), _ => None });
            match store.tasks_list(project.as_deref(), status).await {
                Ok(list) => json!(list),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "profile_get" => {
            let actor = args["actor"].as_str().unwrap_or("");
            match store.profile_get(actor).await {
                Ok(Some(p)) => json!(p),
                Ok(None) => json!({"error": "not found"}),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "profile_update" => {
            let actor = args["actor"].as_str().unwrap_or("");
            let patch = ProfilePatch {
                display_name: args["display_name"].as_str().map(|s| s.to_string()),
                ..Default::default()
            };
            match store.profile_update(actor, patch, "mcp").await {
                Ok(p) => json!(p),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "inbox_list" => {
            let actor = args["actor"].as_str().unwrap_or("anon");
            let unread_only = args["unread_only"].as_bool().unwrap_or(false);
            match store.inbox_list(actor, unread_only).await {
                Ok(list) => json!(list),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        "search" => {
            let q = args["q"].as_str().unwrap_or("");
            let limit = args["limit"].as_i64().unwrap_or(20);
            match store.search(q, limit).await {
                Ok(hits) => json!(hits),
                Err(e) => json!({"error": e.to_string()}),
            }
        }
        _ => json!({"error": format!("unknown tool: {}", name)}),
    }
}

// ---- users (admin) ----

#[derive(Deserialize)]
struct UserCreateBody {
    name: String,
    email: String,
    password: String,
    role: Option<String>,
}

async fn users_list(State(s): State<AppState>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    let actor = resolve_actor_from_headers(&s.store, &headers).await.unwrap_or_else(|| "anon".to_string());
    if !is_admin(&s.store, &actor).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden"})));
    }
    match s.store.users_list().await {
        Ok(list) => (StatusCode::OK, Json(list)),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn users_create(State(s): State<AppState>, headers: axum::http::HeaderMap, Json(body): Json<UserCreateBody>) -> impl IntoResponse {
    let actor = resolve_actor_from_headers(&s.store, &headers).await.unwrap_or_else(|| "anon".to_string());
    if !is_admin(&s.store, &actor).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden"})));
    }
    let role = if body.role.as_deref() == Some("admin") { UserRole::Admin } else { UserRole::Member };
    match s.store.users_create(&body.name, &body.email, &body.password, role, None, &actor).await {
        Ok(user) => (StatusCode::CREATED, Json(json!({"user": user}))),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({"error": e.to_string()}))),
    }
}

async fn is_admin(store: &Store, actor: &str) -> bool {
    store.users_list().await.ok().map(|users| users.iter().any(|u| u.actor == actor && u.role == UserRole::Admin)).unwrap_or(false)
}

// ---- tokens (admin) ----

async fn tokens_list(State(s): State<AppState>, headers: axum::http::HeaderMap) -> impl IntoResponse {
    let actor = resolve_actor_from_headers(&s.store, &headers).await.unwrap_or_else(|| "anon".to_string());
    if !is_admin(&s.store, &actor).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden"})));
    }
    match s.store.tokens_list().await {
        Ok(list) => (StatusCode::OK, Json(list)),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e.to_string()}))),
    }
}

async fn tokens_create(State(s): State<AppState>, headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>) -> impl IntoResponse {
    let actor = resolve_actor_from_headers(&s.store, &headers).await.unwrap_or_else(|| "anon".to_string());
    if !is_admin(&s.store, &actor).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden"})));
    }
    (StatusCode::NOT_IMPLEMENTED, Json(json!({"error": "not yet implemented"})))
}

async fn tokens_remove(State(s): State<AppState>, headers: axum::http::HeaderMap, Path(id): Path<String>) -> impl IntoResponse {
    let actor = resolve_actor_from_headers(&s.store, &headers).await.unwrap_or_else(|| "anon".to_string());
    if !is_admin(&s.store, &actor).await {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "forbidden"})));
    }
    (StatusCode::NOT_IMPLEMENTED, Json(json!({"error": "not yet implemented"})))
}
