// Identity-artifacts: store idempotency + the sync endpoint + owner/admin
// management gating. The sync endpoint is keyed on the authenticated AI actor
// (the token's actor), NOT the per-user memory namespace; disabled artifacts
// are excluded from the sync payload.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use hive_api::store::users::NewUser;
use hive_shared::{ActorKind, UserRole};
use serde_json::Value;
use tower::ServiceExt;

async fn store() -> hive_api::store::Store {
    std::env::set_var("HIVE_EMBED", "hash");
    let pool = hive_api::db::test_pool().await;
    hive_api::store::Store::new(pool)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.clone().oneshot(req).await.expect("request");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}

fn bearer_get(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn bearer_post(path: &str, token: &str, body: Value) -> Request<Body> {
    Request::post(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

// ---- store layer ----

#[tokio::test]
async fn upsert_is_idempotent_by_actor_kind_name() {
    let s = store().await;
    let a = s
        .artifacts_upsert("pia", "skill", "journal", "v1", "first", true)
        .await
        .unwrap();
    let b = s
        .artifacts_upsert("pia", "skill", "journal", "v2", "second", false)
        .await
        .unwrap();

    // Same logical row: id + created_at preserved, content/flags refreshed.
    assert_eq!(a.id, b.id, "upsert must reuse the (actor,kind,name) row");
    assert_eq!(a.created_at, b.created_at);
    assert_eq!(b.content, "v2");
    assert_eq!(b.description, "second");
    assert!(!b.enabled);

    // Exactly one row for that key.
    assert_eq!(s.artifacts_list("pia").await.unwrap().len(), 1);
}

#[tokio::test]
async fn for_actor_excludes_disabled_and_other_actors() {
    let s = store().await;
    s.artifacts_upsert("pia", "skill", "on", "x", "", true)
        .await
        .unwrap();
    s.artifacts_upsert("pia", "agent", "off", "x", "", false)
        .await
        .unwrap();
    s.artifacts_upsert("apis", "skill", "other", "x", "", true)
        .await
        .unwrap();

    let synced = s.artifacts_for_actor("pia").await.unwrap();
    assert_eq!(synced.len(), 1, "only pia's ENABLED artifacts");
    assert_eq!(synced[0].name, "on");

    // Management listing still sees the disabled one.
    assert_eq!(s.artifacts_list("pia").await.unwrap().len(), 2);
}

// ---- HTTP: sync endpoint + management gating ----

/// Onboard an admin and return the store + router. The admin actor is "nate".
async fn app_with_admin() -> (hive_api::store::Store, Router) {
    let s = store().await;
    s.onboarding_complete("Test Hive", "Nate", "nate@example.com", "hunter22-strong")
        .await
        .unwrap();
    let router = hive_api::routes::router(s.clone());
    (s, router)
}

/// Mint an OAuth identity token for `actor` granted by `granter`, so the request
/// authenticates AS `actor` with `granter`'s namespace (and role).
async fn identity_token(s: &hive_api::store::Store, actor: &str, granter: &str) -> String {
    let (token, _) = s
        .tokens_create_oauth(actor, "claude-code", granter, "hive", Some(3600))
        .await
        .unwrap();
    token
}

#[tokio::test]
async fn sync_returns_only_token_actors_enabled_artifacts() {
    let (s, app) = app_with_admin().await;
    // pia is an AI owned by maggie (a non-admin member).
    s.people_upsert("maggie", "Maggie", ActorKind::Human, None)
        .await
        .unwrap();
    s.users_create(
        NewUser {
            name: "Maggie".into(),
            email: "maggie@example.com".into(),
            password: "hunter22-strong".into(),
            role: Some(UserRole::Member),
            actor: Some("maggie".into()),
            kind: Some(ActorKind::Human),
        },
        "test",
    )
    .await
    .unwrap();
    s.people_upsert("pia", "Pia", ActorKind::Ai, Some("maggie"))
        .await
        .unwrap();

    s.artifacts_upsert("pia", "skill", "journal", "body", "j", true)
        .await
        .unwrap();
    s.artifacts_upsert("pia", "agent", "scout", "body", "s", false)
        .await
        .unwrap();
    s.artifacts_upsert("apis", "skill", "elsewhere", "body", "", true)
        .await
        .unwrap();

    let token = identity_token(&s, "pia", "maggie").await;
    let (status, body) = send(&app, bearer_get("/api/identity/artifacts", &token)).await;
    assert_eq!(status, StatusCode::OK);
    let arr = body.as_array().unwrap();
    assert_eq!(arr.len(), 1, "only pia's ENABLED artifact: {body}");
    assert_eq!(arr[0]["name"], "journal");
    assert_eq!(arr[0]["kind"], "skill");
    // camelCase wire shape (matches the file fields the plugin reads).
    assert!(arr[0]["createdAt"].is_string());
}

#[tokio::test]
async fn owner_can_manage_but_non_owner_non_admin_gets_403() {
    let (s, app) = app_with_admin().await;
    // Owner: maggie (member) owns pia. Stranger: bob (member) owns nothing here.
    for (slug, email) in [("maggie", "maggie@example.com"), ("bob", "bob@example.com")] {
        s.people_upsert(slug, slug, ActorKind::Human, None)
            .await
            .unwrap();
        s.users_create(
            NewUser {
                name: slug.into(),
                email: email.into(),
                password: "hunter22-strong".into(),
                role: Some(UserRole::Member),
                actor: Some(slug.into()),
                kind: Some(ActorKind::Human),
            },
            "test",
        )
        .await
        .unwrap();
    }
    s.people_upsert("pia", "Pia", ActorKind::Ai, Some("maggie"))
        .await
        .unwrap();
    s.people_upsert("apis", "Apis", ActorKind::Ai, Some("bob"))
        .await
        .unwrap();

    // pia's owner (maggie) can upsert pia's artifacts.
    let maggie_tok = identity_token(&s, "pia", "maggie").await;
    let (status, _) = send(
        &app,
        bearer_post(
            "/api/actors/pia/artifacts",
            &maggie_tok,
            serde_json::json!({"kind": "skill", "name": "journal", "content": "body"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "owner may manage");

    // bob acts as apis (his own AI); he is neither admin nor pia's owner → 403.
    let bob_tok = identity_token(&s, "apis", "bob").await;
    let (status, _) = send(
        &app,
        bearer_post(
            "/api/actors/pia/artifacts",
            &bob_tok,
            serde_json::json!({"kind": "skill", "name": "sneaky", "content": "body"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-owner non-admin blocked");

    let (status, _) = send(&app, bearer_get("/api/actors/pia/artifacts", &bob_tok)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "list is gated too");

    // Admin (nate, via a PAT — created_by=nate → admin namespace) can manage anyone.
    let (admin_tok, _) = s
        .tokens_create("pia", "admin-pat", Some(7), "nate")
        .await
        .unwrap();
    let (status, body) = send(&app, bearer_get("/api/actors/pia/artifacts", &admin_tok)).await;
    assert_eq!(status, StatusCode::OK, "admin may list: {body}");
}
