// Cross-org isolation, through the real HTTP surface with real sessions.
//
// This file is the security model. If it passes, a member of org A cannot read
// or write org B's rows; if it fails, nothing else in the tree matters. It
// drives the actual router (middleware included) rather than unit-testing a
// helper, because the thing being tested is the composition — auth resolves a
// credential, the credential pins one org, and Postgres enforces it.
//
// Requires a Postgres whose DATABASE_URL role can CREATE ROLE (the API serves
// as an unprivileged role provisioned at boot).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use hive_core::acting::{self, ActingScope};
use hive_core::store::users::NewUser;
use hive_shared::{ActorKind, UserRole};
use serde_json::{json, Value};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use hive_api::store::Store;

async fn test_app() -> (Router, Store) {
    std::env::set_var("HIVE_EMBED", "hash");
    // STRICT: no fallback scope, so an unscoped task reads nothing exactly as
    // the binary behaves. A test on the permissive pool would prove nothing.
    let pool = hive_api::db::test_pool_strict().await;
    let store = Store::new(pool);
    (hive_api::routes::router(store.clone()), store)
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value, axum::http::HeaderMap) {
    let res = app.clone().oneshot(req).await.expect("request");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json, headers)
}

fn post(path: &str, cookie: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::from(body.to_string())).expect("request")
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).expect("request")
}

fn bearer(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

fn session_cookie(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(header::SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .expect("session cookie")
        .to_string()
}

/// Onboard org A (the default org) and return its admin's session cookie.
async fn onboard(app: &Router) -> String {
    let (status, _, headers) = send(
        app,
        post(
            "/api/onboarding",
            None,
            json!({
                "instanceName": "Hive",
                "adminName": "alice",
                "adminEmail": "alice@example.com",
                "password": "correct-horse",
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "onboarding");
    session_cookie(&headers)
}

/// Provision a second org with its own member, the way an operator would.
/// There is deliberately no HTTP route that does this: creating an org is not
/// something a session inside another org may do.
async fn second_org(store: &Store, app: &Router) -> (Uuid, String) {
    let org = store.orgs_create("beta", "Beta").await.expect("create org");
    acting::scope(ActingScope::new(org.id, "bob".to_string(), true), async {
        store
            .users_create(
                NewUser {
                    name: "bob".to_string(),
                    email: "bob@example.com".to_string(),
                    password: "correct-horse".to_string(),
                    role: Some(UserRole::Admin),
                    actor: Some("bob".to_string()),
                    kind: Some(ActorKind::Human),
                },
                "operator",
            )
            .await
            .expect("create user");
    })
    .await;

    let (status, body, headers) = send(
        app,
        post(
            "/api/auth/login",
            None,
            json!({"email": "bob@example.com", "password": "correct-horse"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login as bob: {body}");
    assert_eq!(body["org"]["slug"], "beta", "bob's session acts in beta");
    (org.id, session_cookie(&headers))
}

async fn append_entry(app: &Router, cookie: &str, body: &str) -> String {
    let (status, entry, _) = send(
        app,
        post("/api/journal", Some(cookie), json!({"body": body})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "append: {entry}");
    entry["id"].as_str().expect("entry id").to_string()
}

async fn journal_ids(app: &Router, cookie: &str) -> Vec<String> {
    let (status, list, _) = send(app, get("/api/journal?limit=100", Some(cookie))).await;
    assert_eq!(status, StatusCode::OK, "list: {list}");
    list.as_array()
        .expect("array")
        .iter()
        .filter_map(|e| e["id"].as_str().map(String::from))
        .collect()
}

/// THE test. Everything else in this file is a negative control on it.
#[tokio::test]
async fn a_member_of_one_org_cannot_read_or_insert_into_another() {
    let (app, store) = test_app().await;
    let alice = onboard(&app).await;
    let (beta, bob) = second_org(&store, &app).await;

    let a_entry = append_entry(&app, &alice, "alice writes in the default org").await;
    let b_entry = append_entry(&app, &bob, "bob writes in beta").await;

    // Neither list contains the other org's entry.
    let a_sees = journal_ids(&app, &alice).await;
    let b_sees = journal_ids(&app, &bob).await;
    assert!(a_sees.contains(&a_entry), "alice sees her own entry");
    assert!(b_sees.contains(&b_entry), "bob sees his own entry");
    assert!(
        !a_sees.contains(&b_entry),
        "alice must not see beta's entry"
    );
    assert!(
        !b_sees.contains(&a_entry),
        "bob must not see default's entry"
    );

    // Fetching the other org's entry by its exact id is a 404, not a 403:
    // a row in another org does not exist as far as this session is concerned.
    let (status, _, _) = send(&app, get(&format!("/api/journal/{b_entry}"), Some(&alice))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "alice reading beta's entry");
    let (status, _, _) = send(&app, get(&format!("/api/journal/{a_entry}"), Some(&bob))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "bob reading default's entry");

    // And the write side: bob's insert landed in beta, not in the default org.
    // Checked from OUTSIDE any policy (owner role) so the assertion is about
    // the stored row rather than about what a scoped read happens to show.
    let admin = hive_core::db::test_admin_pool(store.db()).await;
    assert_eq!(org_of(&admin, &b_entry).await, beta, "bob's row is in beta");
    assert_ne!(org_of(&admin, &a_entry).await, beta, "alice's row is not");
}

/// A body that names an org is a body with an ignored field. `org_id` is
/// stamped by a column DEFAULT from the acting scope, so there is no argument
/// to poison — and the policy's WITH CHECK would reject it anyway.
#[tokio::test]
async fn a_forged_org_id_in_the_request_body_is_ignored() {
    let (app, store) = test_app().await;
    let alice = onboard(&app).await;
    let (beta, bob) = second_org(&store, &app).await;
    let default_org = store.orgs_default().await.expect("default org").id;

    let (status, entry, _) = send(
        &app,
        post(
            "/api/journal",
            Some(&bob),
            json!({
                "body": "bob tries to write into the default org",
                "org_id": default_org.to_string(),
                "orgId": default_org.to_string(),
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "append: {entry}");
    let id = entry["id"].as_str().expect("entry id").to_string();

    let admin = hive_core::db::test_admin_pool(store.db()).await;
    assert_eq!(
        org_of(&admin, &id).await,
        beta,
        "the forged org_id must not steer the write"
    );
    assert!(
        !journal_ids(&app, &alice).await.contains(&id),
        "and alice must not see it"
    );
}

/// A bearer token is pinned to the org it was minted in. Presenting it against
/// another org's resource is not a different answer, it is the same answer:
/// that row is not there.
#[tokio::test]
async fn a_token_minted_in_one_org_cannot_reach_another() {
    let (app, store) = test_app().await;
    let alice = onboard(&app).await;
    let (_, bob) = second_org(&store, &app).await;

    let b_entry = append_entry(&app, &bob, "beta-only prose").await;
    let a_entry = append_entry(&app, &alice, "default-only prose").await;

    let (status, minted, _) = send(
        &app,
        post(
            "/api/tokens",
            Some(&alice),
            json!({"actor": "alice", "label": "cli"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mint token: {minted}");
    let token = minted["token"].as_str().expect("token").to_string();

    // The token reaches its own org …
    let (status, _, _) = send(&app, bearer(&format!("/api/journal/{a_entry}"), &token)).await;
    assert_eq!(status, StatusCode::OK, "token reads its own org");
    // … and nothing else. There is no header, parameter, or body field that
    // changes this, because the org came off the token row.
    let (status, _, _) = send(&app, bearer(&format!("/api/journal/{b_entry}"), &token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "token reaching beta");
}

/// The pooled-connection question, answered on a pool of exactly ONE
/// connection: the same physical socket serves org A and then org B, so if the
/// session variable leaked, this test would see it.
#[tokio::test]
async fn one_connection_serving_two_orgs_in_sequence_leaks_nothing() {
    let pool = hive_core::db::test_pool_single_conn().await;
    let store = Store::new(pool);
    let alpha = store.orgs_default().await.expect("default org").id;
    let beta = store.orgs_create("beta", "Beta").await.expect("org").id;

    let alpha_id = acting::scope(ActingScope::new(alpha, "alice".to_string(), true), async {
        insert_entry(&store, "alpha prose").await
    })
    .await;
    let beta_id = acting::scope(ActingScope::new(beta, "bob".to_string(), true), async {
        insert_entry(&store, "beta prose").await
    })
    .await;

    for _ in 0..8 {
        let seen = acting::scope(ActingScope::new(beta, "bob".to_string(), true), async {
            visible_ids(&store).await
        })
        .await;
        assert!(seen.contains(&beta_id), "beta sees its own row");
        assert!(!seen.contains(&alpha_id), "beta must never see alpha's row");

        let seen = acting::scope(ActingScope::new(alpha, "alice".to_string(), true), async {
            visible_ids(&store).await
        })
        .await;
        assert!(seen.contains(&alpha_id), "alpha sees its own row");
        assert!(!seen.contains(&beta_id), "alpha must never see beta's row");
    }
}

/// No acting scope is deny, not bypass. A task that never opened one reads
/// nothing and cannot write: `org_id` has no value to default to.
#[tokio::test]
async fn an_unscoped_task_reads_nothing_and_writes_nothing() {
    let pool = hive_core::db::test_pool_strict().await;
    let store = Store::new(pool);
    let alpha = store.orgs_default().await.expect("default org").id;

    let id = acting::scope(ActingScope::new(alpha, "alice".to_string(), true), async {
        insert_entry(&store, "scoped prose").await
    })
    .await;

    assert!(
        !visible_ids(&store).await.contains(&id),
        "an unscoped read must return nothing"
    );
    let write = hive_core::pgq::query(
        "INSERT INTO journal (id, author, body, tags, mentions, created_at) \
         VALUES (?, 'nobody', 'unscoped', '[]', '[]', ?)",
    )
    .bind("jrnl_unscoped01")
    .bind(hive_core::store::now_iso())
    .execute(store.db())
    .await;
    assert!(write.is_err(), "an unscoped write must fail, not go global");
}

/// The API must not be the table owner or a superuser. `FORCE ROW LEVEL
/// SECURITY` covers the owner, but BYPASSRLS and superuser defeat everything,
/// so boot refuses them — this is that refusal, asserted.
#[tokio::test]
async fn the_serving_role_is_neither_superuser_nor_bypassrls() {
    let pool = hive_core::db::test_pool_strict().await;
    let privileged: bool = hive_core::pgq::query_scalar::<bool>(
        "SELECT COALESCE((SELECT bool_or(rolsuper OR rolbypassrls) FROM pg_roles \
                          WHERE rolname = current_user), true)",
    )
    .fetch_one(&pool)
    .await
    .expect("role probe");
    assert!(!privileged, "the serving role outranks its own policies");

    let owner: Option<String> = hive_core::pgq::query_scalar::<String>(
        "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE oid = 'journal'::regclass",
    )
    .fetch_optional(&pool)
    .await
    .expect("owner probe");
    let current: String = hive_core::pgq::query_scalar::<String>("SELECT current_user::text")
        .fetch_one(&pool)
        .await
        .expect("current user");
    assert_ne!(owner.as_deref(), Some(current.as_str()), "API owns journal");
}

/// Every content table carries the column AND the policy. A table added
/// without them is a hole, and this is the sweep that finds it.
///
/// Four tables carry `org_id` without a policy, deliberately: they are the
/// tenancy plane, read before an acting org exists. `sessions`, `api_tokens`
/// and `oauth_auth_codes` carry the org so a credential can PIN one;
/// `memberships` is what decides which orgs a user may pin. None holds content.
#[tokio::test]
async fn every_content_table_is_scoped_and_forced() {
    const TENANCY_PLANE: &[&str] = &["sessions", "api_tokens", "oauth_auth_codes", "memberships"];
    let pool = hive_core::db::test_pool_strict().await;
    let admin = hive_core::db::test_admin_pool(&pool).await;
    let gaps: Vec<String> = hive_core::pgq::query_scalar::<String>(
        "SELECT c.relname::text FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind = 'r' \
           AND EXISTS (SELECT 1 FROM pg_attribute a \
                       WHERE a.attrelid = c.oid AND a.attname = 'org_id' AND NOT a.attisdropped) \
           AND NOT (c.relrowsecurity AND c.relforcerowsecurity \
                    AND EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid))",
    )
    .fetch_all(&admin)
    .await
    .expect("policy sweep");
    let gaps: Vec<String> = gaps
        .into_iter()
        .filter(|t| !TENANCY_PLANE.contains(&t.as_str()))
        .collect();
    assert!(gaps.is_empty(), "org_id without a forced policy: {gaps:?}");

    // And the reverse sweep: a content table that lost `org_id` would drop out
    // of the list above entirely, so count the ones that have it.
    let scoped: i64 = hive_core::pgq::query_scalar::<i64>(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind = 'r' \
           AND c.relrowsecurity AND c.relforcerowsecurity \
           AND EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid)",
    )
    .fetch_one(&admin)
    .await
    .expect("scoped count");
    // 30 at the orgs workstream, +1 for `artifacts` (the file-artifacts
    // workstream, integrated separately). Bump this deliberately when a content
    // table is ADDED; a drop means one silently lost `org_id`, which is the
    // whole reason this counts rather than trusting the gap sweep above.
    assert_eq!(scoped, 31, "content tables under a forced org policy");
}

// ---- helpers that talk to the store directly ----

async fn insert_entry(store: &Store, body: &str) -> String {
    let view = store
        .journal_append(
            hive_shared::NewJournalEntry {
                author: Some("tester".to_string()),
                body: body.to_string(),
                tags: None,
                anchors: None,
            },
            Some("tester"),
            None,
        )
        .await
        .expect("append");
    view.entry.id
}

async fn visible_ids(store: &Store) -> Vec<String> {
    hive_core::pgq::query_scalar::<String>("SELECT id FROM journal")
        .fetch_all(store.db())
        .await
        .expect("select")
}

async fn org_of(admin: &PgPool, entry_id: &str) -> Uuid {
    hive_core::pgq::query_scalar::<Uuid>("SELECT org_id FROM journal WHERE id = ?")
        .bind(entry_id)
        .fetch_one(admin)
        .await
        .expect("org_id")
}
