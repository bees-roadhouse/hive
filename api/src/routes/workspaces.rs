// Hosted Claude Code workspaces — HTTP surface. Per-user CRUD + transcript, the
// runner's ingest path, and the INTERNAL runtime-auth that hands decrypted owner
// credentials to the runner. Auth + per-user scoping come from AuthCtx (middleware).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Extension, Router};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hive_shared::NewJournalEntry;
use rand::RngCore;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::error::{err, forbidden, not_found, ApiResult};
use crate::middleware::AuthCtx;
use crate::store::cc_credentials::NewCcCredential;
use crate::store::workspaces::{LinkedEntity, NewCcMessage, NewCcSession};
use crate::store::{now_iso, Store};

pub fn router() -> Router<Store> {
    Router::new()
        .route("/api/workspaces", get(list).post(create))
        .route("/api/workspaces/{id}", get(get_one))
        .route(
            "/api/workspaces/{id}/messages",
            get(transcript).post(ingest),
        )
        .route("/api/workspaces/{id}/input", post(send_input))
        .route("/api/workspaces/{id}/archive", post(archive))
        .route("/api/workspaces/{id}/status", post(set_status))
        .route("/api/workspaces/{id}/runtime-auth", get(runtime_auth))
        .route(
            "/api/runtime-oauth/{runtime}/start",
            get(runtime_oauth_start),
        )
        .route(
            "/api/runtime-oauth/{runtime}/callback",
            get(runtime_oauth_callback),
        )
        .route("/api/cc-credentials", get(creds_list).post(creds_put))
        .route("/api/cc-credentials/{id}", delete(creds_delete))
}

/// Require a non-anon principal (the /api gate already enforces auth; defensive).
// The Err carries a full axum Response by design (callers `return Ok(r)` it); that
// trips result_large_err, which is not meaningful for a two-call-site guard.
#[allow(clippy::result_large_err)]
fn require_actor(ctx: &AuthCtx) -> Result<String, Response> {
    match ctx.actor.as_deref() {
        Some(a) if a != "anon" => Ok(a.to_string()),
        _ => Err(err(StatusCode::UNAUTHORIZED, "authentication required")),
    }
}

#[derive(Deserialize)]
struct ListQuery {
    limit: Option<i64>,
}

async fn list(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Query(q): Query<ListQuery>,
) -> ApiResult {
    let limit = q.limit.unwrap_or(50).clamp(1, 500);
    Ok(Json(s.workspace_list(&ctx.visibility(), limit).await?).into_response())
}

async fn create(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(input): Json<NewCcSession>,
) -> ApiResult {
    if let Err(r) = require_actor(&ctx) {
        return Ok(r);
    }
    let owner = ctx.namespace_user().to_string();
    let created_by = ctx.actor().to_string();
    let mut input = input;
    let prompt = input.prompt.clone();
    let linked_entities = input.linked_entities.clone().unwrap_or_default();
    let project_link = if let Some(project_name) = input
        .project
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        let project = s.projects_ensure(project_name).await?;
        input.project = Some(project.name.clone());
        Some(project.id)
    } else {
        None
    };
    let session = s.workspace_create(&owner, &created_by, input).await?;
    let mut prelinked = Vec::new();
    let has_project_link = project_link.is_some();
    if let Some(project_id) = project_link {
        s.links_create(
            "conversation",
            &session.id,
            "project",
            &project_id,
            "grouped_in",
        )
        .await?;
        prelinked.push(("project".to_string(), project_id));
    }
    link_conversation_entities(
        &s,
        &session.id,
        linked_entities,
        prelinked,
        has_project_link,
    )
    .await?;
    // Persist the kickoff prompt as the first transcript message so the runner can
    // pick it up and the UI shows it immediately.
    if let Some(text) = prompt.filter(|t| !t.trim().is_empty()) {
        s.workspace_append_message(&session.id, input_message(&text))
            .await?;
        append_workspace_journal(
            &s,
            &created_by,
            &owner,
            &session.id,
            &session.runtime,
            &text,
        )
        .await?;
    }
    Ok((StatusCode::CREATED, Json(session)).into_response())
}

async fn get_one(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    match s.workspace_get(&ctx.visibility(), &id).await? {
        Some(ws) => Ok(Json(ws).into_response()),
        None => Ok(not_found()),
    }
}

#[derive(Deserialize)]
struct TranscriptQuery {
    after: Option<i64>,
    limit: Option<i64>,
}

async fn transcript(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Query(q): Query<TranscriptQuery>,
) -> ApiResult {
    if s.workspace_get(&ctx.visibility(), &id).await?.is_none() {
        return Ok(not_found());
    }
    let msgs = s
        .workspace_transcript(
            &id,
            q.after.unwrap_or(0),
            q.limit.unwrap_or(1000).clamp(1, 5000),
        )
        .await?;
    Ok(Json(msgs).into_response())
}

/// Runner → API: append a transcript message. Allowed for the owner or an admin
/// (the runner authenticates as a service/admin principal).
async fn ingest(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(input): Json<NewCcMessage>,
) -> ApiResult {
    let Some(ws) = s.workspace_get_internal(&id).await? else {
        return Ok(not_found());
    };
    if !ctx.is_admin() && ctx.namespace_user() != ws.owner {
        return Ok(forbidden());
    }
    let msg = s.workspace_append_message(&id, input.clone()).await?;
    if let Some(body) = journal_body_for_message(&input) {
        append_workspace_journal(
            &s,
            message_author(&input, &ws.runtime),
            &ws.owner,
            &id,
            &ws.runtime,
            &body,
        )
        .await?;
    }
    Ok((StatusCode::CREATED, Json(msg)).into_response())
}

#[derive(Deserialize)]
struct InputBody {
    text: String,
}

/// Human → session: record the input as a transcript message and signal the runner.
async fn send_input(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(body): Json<InputBody>,
) -> ApiResult {
    if s.workspace_get(&ctx.visibility(), &id).await?.is_none() {
        return Ok(not_found());
    }
    let msg = s
        .workspace_append_message(&id, input_message(&body.text))
        .await?;
    let Some(ws) = s.workspace_get_internal(&id).await? else {
        return Ok(not_found());
    };
    append_workspace_journal(&s, ctx.actor(), &ws.owner, &id, &ws.runtime, &body.text).await?;
    s.emit(
        "workspace.input",
        ctx.actor(),
        json!({"session_id": id, "seq": msg.seq, "text": body.text}),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(msg)).into_response())
}

async fn archive(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if s.workspace_get(&ctx.visibility(), &id).await?.is_none() {
        return Ok(not_found());
    }
    s.workspace_archive(&id).await?;
    Ok(Json(json!({"ok": true})).into_response())
}

#[derive(Deserialize)]
struct StatusBody {
    status: String,
}

/// Runner/owner sets a session's lifecycle status (running/idle/completed/failed/…).
async fn set_status(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
    Json(body): Json<StatusBody>,
) -> ApiResult {
    let Some(ws) = s.workspace_get_internal(&id).await? else {
        return Ok(not_found());
    };
    if !ctx.is_admin() && ctx.namespace_user() != ws.owner {
        return Ok(forbidden());
    }
    s.workspace_set_status(&id, &body.status).await?;
    Ok(Json(json!({"ok": true, "status": body.status})).into_response())
}

/// INTERNAL: hand the decrypted owner credential to the runner. Admin/service only.
async fn runtime_auth(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if !ctx.is_admin() {
        return Ok(forbidden());
    }
    let Some(ws) = s.workspace_get_internal(&id).await? else {
        return Ok(not_found());
    };
    match s
        .cc_cred_decrypt_for_runtime(&ws.owner, &ws.runtime)
        .await?
    {
        Some((kind, runtime, provider, secret)) => Ok(Json(json!({
            "owner": ws.owner,
            "runtime": runtime,
            "provider": provider,
            "model": ws.model,
            "kind": kind,
            "secret": secret,
            "workdir": ws.workdir,
        }))
        .into_response()),
        None => {
            let msg = format!("owner has no {} credential saved", ws.runtime);
            Ok(err(StatusCode::FAILED_DEPENDENCY, &msg))
        }
    }
}

// ---- hosted runtime OAuth callback flow (Codex / Claude Code) ----

#[derive(Clone)]
struct RuntimeOAuthConfig {
    runtime: String,
    provider: Option<String>,
    client_id: String,
    client_secret: Option<String>,
    auth_url: String,
    token_url: String,
    redirect_uri: String,
    scopes: String,
}

#[derive(Deserialize)]
struct RuntimeOAuthStartQuery {
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct RuntimeOAuthCallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(sqlx::FromRow)]
struct RuntimeOAuthStateRow {
    owner: String,
    runtime: String,
    provider: Option<String>,
    code_verifier: String,
    return_to: String,
}

fn runtime_env_prefix(runtime: &str) -> Option<&'static str> {
    match runtime {
        "codex" => Some("HIVE_CODEX_OAUTH"),
        "claude_code" => Some("HIVE_CLAUDE_CODE_OAUTH"),
        _ => None,
    }
}

fn runtime_label(runtime: &str) -> &'static str {
    match runtime {
        "codex" => "Codex subscription",
        "claude_code" => "Claude Code subscription",
        _ => "Runtime subscription",
    }
}

fn runtime_oauth_config(runtime: &str, issuer: &str) -> Result<RuntimeOAuthConfig, String> {
    let runtime = crate::store::workspaces::normalize_runtime(Some(runtime));
    let Some(prefix) = runtime_env_prefix(&runtime) else {
        return Err("unsupported runtime".to_string());
    };
    let get = |suffix: &str| {
        std::env::var(format!("{prefix}_{suffix}"))
            .ok()
            .filter(|v| !v.trim().is_empty())
    };
    let client_id =
        get("CLIENT_ID").ok_or_else(|| format!("{prefix}_CLIENT_ID is not configured"))?;
    let auth_url = get("AUTH_URL").ok_or_else(|| format!("{prefix}_AUTH_URL is not configured"))?;
    let token_url =
        get("TOKEN_URL").ok_or_else(|| format!("{prefix}_TOKEN_URL is not configured"))?;
    let redirect_uri = get("REDIRECT_URI")
        .unwrap_or_else(|| format!("{issuer}/api/runtime-oauth/{runtime}/callback"));
    Ok(RuntimeOAuthConfig {
        runtime,
        provider: get("PROVIDER"),
        client_id,
        client_secret: get("CLIENT_SECRET"),
        auth_url,
        token_url,
        redirect_uri,
        scopes: get("SCOPES").unwrap_or_else(|| "openid profile offline_access".to_string()),
    })
}

fn random_urlsafe(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    rand::rngs::OsRng.fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn safe_return_path(v: Option<String>) -> String {
    let raw = v.unwrap_or_else(|| "/settings".to_string());
    if raw.starts_with('/') && !raw.starts_with("//") && !raw.contains('\n') && !raw.contains('\r')
    {
        raw
    } else {
        "/settings".to_string()
    }
}

fn runtime_query_string(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn with_status(path: &str, status: &str) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}runtime_oauth={}", urlencoding::encode(status))
}

async fn runtime_oauth_start(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    headers: axum::http::HeaderMap,
    Path(runtime): Path<String>,
    Query(q): Query<RuntimeOAuthStartQuery>,
) -> ApiResult {
    if let Err(r) = require_actor(&ctx) {
        return Ok(r);
    }
    let issuer = crate::middleware::issuer_for(&s, &headers).await;
    let cfg = match runtime_oauth_config(&runtime, &issuer) {
        Ok(cfg) => cfg,
        Err(msg) => return Ok(err(StatusCode::NOT_IMPLEMENTED, &msg)),
    };
    let state = random_urlsafe(32);
    let verifier = random_urlsafe(64);
    let challenge = pkce_challenge(&verifier);
    let return_to = safe_return_path(q.return_to);
    crate::pgq::query(
        "INSERT INTO runtime_oauth_states (state, owner, runtime, provider, code_verifier, return_to, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&state)
    .bind(ctx.namespace_user())
    .bind(&cfg.runtime)
    .bind(&cfg.provider)
    .bind(&verifier)
    .bind(&return_to)
    .bind(now_iso())
    .execute(s.db())
    .await?;

    let authorize_url = format!(
        "{}?{}",
        cfg.auth_url,
        runtime_query_string(&[
            ("response_type", "code"),
            ("client_id", &cfg.client_id),
            ("redirect_uri", &cfg.redirect_uri),
            ("scope", &cfg.scopes),
            ("state", &state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ])
    );
    Ok(Redirect::temporary(&authorize_url).into_response())
}

async fn runtime_oauth_callback(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    headers: axum::http::HeaderMap,
    Path(runtime): Path<String>,
    Query(q): Query<RuntimeOAuthCallbackQuery>,
) -> ApiResult {
    if let Err(r) = require_actor(&ctx) {
        return Ok(r);
    }
    let state = q.state.unwrap_or_default();
    if state.is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "missing state"));
    }
    let row = crate::pgq::query_as::<RuntimeOAuthStateRow>(
        "DELETE FROM runtime_oauth_states WHERE state = ? RETURNING owner, runtime, provider, code_verifier, return_to",
    )
    .bind(&state)
    .fetch_optional(s.db())
    .await?;
    let Some(row) = row else {
        return Ok(err(StatusCode::BAD_REQUEST, "invalid_state"));
    };
    let requested_runtime = crate::store::workspaces::normalize_runtime(Some(&runtime));
    if row.runtime != requested_runtime || row.owner != ctx.namespace_user() {
        return Ok(err(StatusCode::FORBIDDEN, "state_owner_mismatch"));
    }
    if q.error.is_some() {
        return Ok(Redirect::temporary(&with_status(&row.return_to, "denied")).into_response());
    }
    let code = q.code.unwrap_or_default();
    if code.is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "missing code"));
    }
    let issuer = crate::middleware::issuer_for(&s, &headers).await;
    let cfg = match runtime_oauth_config(&row.runtime, &issuer) {
        Ok(cfg) => cfg,
        Err(msg) => return Ok(err(StatusCode::NOT_IMPLEMENTED, &msg)),
    };
    let token = exchange_runtime_code(&cfg, &code, &row.code_verifier).await?;
    let secret = token
        .get("refresh_token")
        .or_else(|| token.get("access_token"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if secret.is_empty() {
        return Ok(err(
            StatusCode::BAD_GATEWAY,
            "oauth token response did not include a token",
        ));
    }
    let provider = row.provider.or(cfg.provider);
    s.cc_cred_put(
        &row.owner,
        NewCcCredential {
            kind: "oauth_token".to_string(),
            runtime: Some(row.runtime.clone()),
            provider,
            label: Some(runtime_label(&row.runtime).to_string()),
            secret,
        },
    )
    .await?;
    Ok(Redirect::temporary(&with_status(&row.return_to, "connected")).into_response())
}

async fn exchange_runtime_code(
    cfg: &RuntimeOAuthConfig,
    code: &str,
    verifier: &str,
) -> anyhow::Result<Value> {
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", cfg.redirect_uri.as_str()),
        ("client_id", cfg.client_id.as_str()),
        ("code_verifier", verifier),
    ];
    if let Some(secret) = cfg.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let res = reqwest::Client::new()
        .post(&cfg.token_url)
        .form(&form)
        .send()
        .await?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_else(|_| json!({}));
    if !status.is_success() {
        anyhow::bail!(
            "runtime oauth token exchange failed: {} {}",
            status.as_u16(),
            body
        );
    }
    Ok(body)
}

// ---- per-user credentials (redacted; secret never returned) ----

async fn creds_list(State(s): State<Store>, Extension(ctx): Extension<AuthCtx>) -> ApiResult {
    Ok(Json(s.cc_cred_list(ctx.namespace_user()).await?).into_response())
}

async fn creds_put(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Json(input): Json<NewCcCredential>,
) -> ApiResult {
    if let Err(r) = require_actor(&ctx) {
        return Ok(r);
    }
    if input.secret.trim().is_empty() || input.kind.trim().is_empty() {
        return Ok(err(StatusCode::BAD_REQUEST, "kind and secret are required"));
    }
    let owner = ctx.namespace_user().to_string();
    Ok((
        StatusCode::CREATED,
        Json(s.cc_cred_put(&owner, input).await?),
    )
        .into_response())
}

async fn creds_delete(
    State(s): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    Path(id): Path<String>,
) -> ApiResult {
    if s.cc_cred_delete(ctx.namespace_user(), &id).await? {
        Ok(StatusCode::NO_CONTENT.into_response())
    } else {
        Ok(not_found())
    }
}

fn content_str<'a>(content: &'a Value, key: &str) -> Option<&'a str> {
    content
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn journal_body_for_message(input: &NewCcMessage) -> Option<String> {
    match (input.role.as_str(), input.kind.as_str()) {
        ("assistant", "text") | ("assistant", "thinking") => {
            content_str(&input.content, "text").map(str::to_string)
        }
        ("system", "error") => {
            content_str(&input.content, "error").map(|s| format!("Runtime error: {s}"))
        }
        ("system", "result") => content_str(&input.content, "result")
            .or_else(|| content_str(&input.content, "note"))
            .map(str::to_string),
        ("user", "input") => content_str(&input.content, "text").map(str::to_string),
        _ => None,
    }
}

fn message_author<'a>(input: &'a NewCcMessage, runtime: &'a str) -> &'a str {
    match input.role.as_str() {
        "user" => "human",
        "assistant" | "system" => runtime,
        _ => runtime,
    }
}

async fn link_conversation_entities(
    s: &Store,
    conversation_id: &str,
    entities: Vec<LinkedEntity>,
    prelinked: Vec<(String, String)>,
    skip_project_entities: bool,
) -> anyhow::Result<()> {
    let mut seen: std::collections::HashSet<(String, String)> = prelinked.into_iter().collect();
    for entity in entities {
        let kind = entity.kind.trim();
        let id = entity.id.trim();
        if kind.is_empty() || id.is_empty() {
            continue;
        }
        if skip_project_entities && kind == "project" {
            continue;
        }
        if !seen.insert((kind.to_string(), id.to_string())) {
            continue;
        }
        let rel = entity.rel.as_deref().unwrap_or("related").trim();
        s.links_create(
            "conversation",
            conversation_id,
            kind,
            id,
            if rel.is_empty() { "related" } else { rel },
        )
        .await?;
    }
    Ok(())
}

async fn append_workspace_journal(
    s: &Store,
    author: &str,
    owner: &str,
    session_id: &str,
    runtime: &str,
    body: &str,
) -> anyhow::Result<()> {
    let body = body.trim();
    if body.is_empty() {
        return Ok(());
    }
    let tagged = format!("[workspace:{session_id}] {body}");
    s.journal_append(
        NewJournalEntry {
            author: Some(author.to_string()),
            body: tagged,
            tags: Some(vec![
                "workspace".to_string(),
                runtime.to_string(),
                session_id.to_string(),
            ]),
            anchors: None,
        },
        Some(author),
        Some(owner),
    )
    .await?;
    Ok(())
}

fn input_message(text: &str) -> NewCcMessage {
    NewCcMessage {
        role: "user".to_string(),
        kind: "input".to_string(),
        content: json!({ "text": text }),
        raw: json!({}),
        tokens_in: None,
        tokens_out: None,
        claude_session_id: None,
    }
}

#[cfg(test)]
mod runtime_oauth_tests {
    use super::*;

    #[test]
    fn runtime_oauth_only_supports_subscription_runtimes() {
        assert_eq!(runtime_env_prefix("codex"), Some("HIVE_CODEX_OAUTH"));
        assert_eq!(
            runtime_env_prefix("claude_code"),
            Some("HIVE_CLAUDE_CODE_OAUTH")
        );
        assert_eq!(runtime_env_prefix("opencode"), None);
    }

    #[test]
    fn safe_return_path_blocks_open_redirects() {
        assert_eq!(safe_return_path(Some("/settings".into())), "/settings");
        assert_eq!(
            safe_return_path(Some("/settings?tab=agents".into())),
            "/settings?tab=agents"
        );
        assert_eq!(
            safe_return_path(Some("https://evil.example".into())),
            "/settings"
        );
        assert_eq!(safe_return_path(Some("//evil.example".into())), "/settings");
    }

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }
}
