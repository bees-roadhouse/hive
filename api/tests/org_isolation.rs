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

async fn test_app() -> (Router, Store, hive_api::db::TestDb) {
    std::env::set_var("HIVE_EMBED", "hash");
    // STRICT: no fallback scope, so an unscoped task reads nothing exactly as
    // the binary behaves. A test on the permissive pool would prove nothing.
    let test_db = hive_api::db::test_pool_strict().await;
    let store = Store::new(test_db.pool.clone());
    (hive_api::routes::router(store.clone()), store, test_db)
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
    let (app, store, _test_db) = test_app().await;
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
    let (app, store, _test_db) = test_app().await;
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
    let (app, store, _test_db) = test_app().await;
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
    let test_db = hive_core::db::test_pool_single_conn().await;
    let store = Store::new(test_db.pool.clone());
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
    let test_db = hive_core::db::test_pool_strict().await;
    let store = Store::new(test_db.pool.clone());
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
    let test_db = hive_core::db::test_pool_strict().await;
    let pool = test_db.pool.clone();
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

/// Tables that are legitimately NOT org-scoped. Every one is either the
/// tenancy plane — read to answer "which org is this credential acting in",
/// which cannot itself require an acting org without being circular — or
/// instance bookkeeping that holds no tenant content.
///
/// This list is the whole forcing function of the sweep below. A new table is
/// a hole unless someone puts it here ON PURPOSE, with a reason, in review.
const NOT_ORG_SCOPED: &[(&str, &str)] = &[
    ("_sqlx_migrations", "sqlx's own migration ledger"),
    (
        "orgs",
        "the tenancy root; you cannot scope the table that defines scopes",
    ),
    (
        "memberships",
        "decides which orgs a user may pin; read before one is pinned",
    ),
    (
        "user_identities",
        "(issuer, subject) → user, resolved at login",
    ),
    // A user account is global and `memberships` scopes it: a credential
    // resolves through `users` BEFORE any org exists (email → user at login,
    // actor → user for a bearer token's granting human). See the long note on
    // the table in core/src/db.rs. What IS scoped is every list of them —
    // `users_list` joins memberships on the acting org.
    ("users", "global login accounts; membership is the scope"),
    (
        "sessions",
        "a credential that PINS an org; resolved before one exists",
    ),
    (
        "api_tokens",
        "same, and `tokens_list`/`tokens_remove` carry the predicate by hand",
    ),
    (
        "oauth_auth_codes",
        "redeemed by an unauthenticated endpoint; carries the org forward",
    ),
    (
        "oauth_clients",
        "RFC 7591 registration is unauthenticated, so there is no org to record",
    ),
    (
        "config",
        "instance settings (name, version, onboarding); read before login",
    ),
    ("worker_status", "one row, the worker heartbeat"),
];

/// EVERY table is org-scoped unless it is on the list above.
///
/// The sweep this replaces asked the question backwards: it found tables that
/// HAVE `org_id` but lack a policy, so a table with NEITHER was invisible to
/// it. `users`, `blobs` and `runtime_oauth_states` all passed it green while
/// leaking, and `blobs` was losing another org's attachment bytes at the time.
/// A count of scoped tables did not catch it either, because the count only
/// moves when a SCOPED table changes.
///
/// Inverted, the default is "scoped": a new table fails until someone either
/// scopes it or writes down why it does not need to be.
#[tokio::test]
async fn every_table_is_org_scoped_unless_deliberately_exempt() {
    let test_db = hive_core::db::test_pool_strict().await;
    let pool = test_db.pool.clone();
    let admin = hive_core::db::test_admin_pool(&pool).await;
    let exempt: Vec<String> = NOT_ORG_SCOPED.iter().map(|(t, _)| t.to_string()).collect();

    // `org_id`, RLS, FORCE, and a policy with BOTH halves. USING alone filters
    // reads while leaving writes into another org legal; WITH CHECK alone
    // leaves them readable. The gap is named in the failure so the message is
    // actionable rather than just a table list.
    let gaps: Vec<String> = hive_core::pgq::query_scalar::<String>(
        "SELECT c.relname::text || ' [' || concat_ws('; ', \
           CASE WHEN NOT EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid \
                                 AND a.attname = 'org_id' AND NOT a.attisdropped) \
                THEN 'no org_id column' END, \
           CASE WHEN NOT c.relrowsecurity THEN 'row security not enabled' END, \
           CASE WHEN NOT c.relforcerowsecurity THEN 'not FORCEd (the owner bypasses it)' END, \
           CASE WHEN NOT EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid \
                                 AND p.polqual IS NOT NULL AND p.polwithcheck IS NOT NULL) \
                THEN 'no policy carrying both USING and WITH CHECK' END) || ']' \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = current_schema() AND c.relkind = 'r' \
           AND NOT (c.relname::text = ANY(?)) \
           AND NOT (c.relrowsecurity AND c.relforcerowsecurity \
                    AND EXISTS (SELECT 1 FROM pg_attribute a WHERE a.attrelid = c.oid \
                                AND a.attname = 'org_id' AND NOT a.attisdropped) \
                    AND EXISTS (SELECT 1 FROM pg_policy p WHERE p.polrelid = c.oid \
                                AND p.polqual IS NOT NULL AND p.polwithcheck IS NOT NULL)) \
         ORDER BY 1",
    )
    .bind(&exempt)
    .fetch_all(&admin)
    .await
    .expect("policy sweep");
    assert!(
        gaps.is_empty(),
        "these tables are neither org-scoped nor on the deliberate exemption list \
         in api/tests/org_isolation.rs: {gaps:#?}"
    );

    // And the list stays honest in the other direction: an exemption for a
    // table that no longer exists is a stale note that would silently cover a
    // future table of the same name.
    let missing: Vec<String> = hive_core::pgq::query_scalar::<String>(
        "SELECT x.name FROM unnest(?::text[]) AS x(name) \
         WHERE to_regclass(x.name) IS NULL",
    )
    .bind(&exempt)
    .fetch_all(&admin)
    .await
    .expect("exemption sweep");
    assert!(
        missing.is_empty(),
        "exempted tables that do not exist — drop them from NOT_ORG_SCOPED: {missing:?}"
    );
}

/// `GET /api/users` used to answer with every tenant's people: id, email,
/// actor slug and role, to any admin, because `users_list` had no org
/// predicate at all. A user row is global on purpose — a credential resolves
/// through it before an acting org exists — so the join to `memberships` IS
/// the access control, and this is the assertion that it is there.
///
/// The same query backs `/api/auth/me` and the bearer-token admin check in
/// routes/admin.rs, so a leak here is not only a listing.
#[tokio::test]
async fn the_user_list_does_not_cross_orgs() {
    let (app, store, _test_db) = test_app().await;
    let alice = onboard(&app).await;
    let (_, bob) = second_org(&store, &app).await;

    let (status, seen, _) = send(&app, get("/api/users", Some(&alice))).await;
    assert_eq!(status, StatusCode::OK, "alice lists users: {seen}");
    let actors: Vec<String> = seen
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|u| u["actor"].as_str().map(String::from))
        .collect();
    assert!(actors.contains(&"alice".to_string()), "alice sees herself");
    assert!(
        !actors.contains(&"bob".to_string()),
        "alice must not see beta's people: {seen}"
    );
    let emails: Vec<String> = seen
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|u| u["email"].as_str().map(String::from))
        .collect();
    assert!(
        !emails.iter().any(|e| e.contains("bob")),
        "and not their email addresses either: {emails:?}"
    );

    // Symmetric, and `me` reads the same list — a session in beta must still
    // resolve to its own user through it.
    let (status, seen, _) = send(&app, get("/api/users", Some(&bob))).await;
    assert_eq!(status, StatusCode::OK, "bob lists users: {seen}");
    let actors: Vec<String> = seen
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|u| u["actor"].as_str().map(String::from))
        .collect();
    assert_eq!(actors, vec!["bob".to_string()], "beta sees beta only");
    let (status, me, _) = send(&app, get("/api/auth/me", Some(&bob))).await;
    assert_eq!(status, StatusCode::OK, "me: {me}");
    assert_eq!(me["user"]["actor"], "bob", "me still resolves: {me}");
}

/// One org's admin listed AND revoked another org's agent token. `api_tokens`
/// carries `org_id` and is deliberately policy-free (the auth middleware
/// resolves a bearer BEFORE an acting org exists, so a policy would make the
/// credential unreadable by the lookup that discovers its org) — which makes
/// the predicate on every OTHER query the only thing standing there.
#[tokio::test]
async fn one_orgs_admin_cannot_see_or_revoke_anothers_tokens() {
    let (app, store, _test_db) = test_app().await;
    let alice = onboard(&app).await;
    let (beta, bob) = second_org(&store, &app).await;

    let a_token = mint_token(&app, &alice, "alice").await;
    let b_token = mint_token(&app, &bob, "bob").await;

    // The list shows this org's tokens and no others.
    let (status, list, _) = send(&app, get("/api/tokens", Some(&alice))).await;
    assert_eq!(status, StatusCode::OK, "list tokens: {list}");
    let ids: Vec<String> = list
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|t| t["id"].as_str().map(String::from))
        .collect();
    assert!(ids.contains(&a_token.1), "alice sees her own token");
    assert!(
        !ids.contains(&b_token.1),
        "alice must not see beta's token: {list}"
    );

    // And revoking by id across the boundary is a 404, not a deletion. This
    // is the destructive half: a cross-tenant denial of service that needed
    // nothing but a guessed id.
    let (status, _, _) = send(
        &app,
        Request::delete(format!("/api/tokens/{}", b_token.1))
            .header(header::COOKIE, alice.clone())
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "alice revoking beta's token");
    let (status, _, _) = send(&app, bearer("/api/journal?limit=1", &b_token.0)).await;
    assert_eq!(status, StatusCode::OK, "beta's token still works");

    // Second half, same fixture: a token must not outlive its granting
    // human's membership. Sessions re-check this on every resolve; tokens did
    // not, so a removed admin's agent kept acting in the org indefinitely.
    let admin = hive_core::db::test_admin_pool(store.db()).await;
    hive_core::pgq::query("DELETE FROM memberships WHERE org_id = ?")
        .bind(beta)
        .execute(&admin)
        .await
        .expect("revoke membership");
    let (status, _, _) = send(&app, bearer("/api/journal?limit=1", &b_token.0)).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a token whose human left the org stops acting in it"
    );
}

/// Cross-org DATA DESTRUCTION, and the sharpest of the three: `blobs` carried
/// no `org_id`, dedup on the content hash was GLOBAL, and three call sites
/// asked an RLS-FILTERED `mail_attachments` whether a blob was orphaned and
/// then DELETEd from an UNFILTERED `blobs`. The other org's attachments were
/// invisible to the question, so their bytes read as unreferenced and went —
/// and `ON DELETE SET NULL` cut the surviving pointer, because Postgres runs
/// foreign key actions with row security bypassed and no policy could have
/// stopped it.
#[tokio::test]
async fn a_blob_sweep_in_one_org_cannot_destroy_anothers_attachment_bytes() {
    let test_db = hive_core::db::test_pool_strict().await;
    let store = Store::new(test_db.pool.clone());
    let alpha = store.orgs_default().await.expect("default org").id;
    let beta = store.orgs_create("beta", "Beta").await.expect("org").id;

    // The SAME bytes in both orgs: exactly what global dedup collapsed into
    // one shared row, and aged past the GC's 24h grace in both.
    const HASH: &str = "a-hash-both-orgs-happen-to-hold";
    for (org, who) in [(alpha, "alice"), (beta, "bob")] {
        acting::scope(ActingScope::new(org, who.to_string(), true), async {
            seed_attachment(&store, who, HASH).await;
        })
        .await;
    }

    // Alpha orphans ITS copy and sweeps.
    acting::scope(ActingScope::new(alpha, "alice".to_string(), true), async {
        hive_core::pgq::query("DELETE FROM mail_attachments")
            .execute(store.db())
            .await
            .expect("orphan alpha's attachment");
        let swept = store.mail_blobs_gc().await.expect("gc");
        assert_eq!(swept, 1, "alpha sweeps exactly its own orphaned copy");
    })
    .await;

    // Beta still has its pointer AND its bytes.
    assert_attachment_intact(&store, beta, HASH, "after alpha's blob GC").await;

    // Same question of the actor purge, which swept "every blob nothing
    // points at" with no restriction to the purged actor at all.
    acting::scope(ActingScope::new(alpha, "alice".to_string(), true), async {
        store.actors_remove("alice").await.expect("purge alice");
    })
    .await;
    assert_attachment_intact(&store, beta, HASH, "after alpha purged an actor").await;

    // And beta can still SERVE the bytes, which is the thing a reader would
    // notice: a nulled `blob_hash` renders as an attachment that lost its file.
    let admin = hive_core::db::test_admin_pool(store.db()).await;
    let rows: i64 = hive_core::pgq::query_scalar::<i64>(
        "SELECT count(*) FROM blobs WHERE hash = ? AND org_id = ?",
    )
    .bind(HASH)
    .bind(beta)
    .fetch_one(&admin)
    .await
    .expect("blob probe");
    assert_eq!(rows, 1, "beta's copy of the bytes is a row of its own");
}

// ---- helpers that talk to the store directly ----

/// Mint a bearer token through the real route; returns (plaintext, id).
async fn mint_token(app: &Router, cookie: &str, actor: &str) -> (String, String) {
    let (status, minted, _) = send(
        app,
        post(
            "/api/tokens",
            Some(cookie),
            json!({"actor": actor, "label": "cli"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "mint token: {minted}");
    (
        minted["token"].as_str().expect("token").to_string(),
        minted["record"]["id"].as_str().expect("id").to_string(),
    )
}

/// A mail account, one message, one attachment, and the bytes it points at —
/// all in the caller's acting org, all aged past the GC grace window.
async fn seed_attachment(store: &Store, owner: &str, hash: &str) {
    let old = "2020-01-01T00:00:00.000Z";
    store
        .people_ensure(owner, hive_shared::ActorKind::Human)
        .await
        .expect("person");
    hive_core::pgq::query(
        "INSERT INTO mail_accounts (id, owner, address, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(format!("acct-{owner}"))
    .bind(owner)
    .bind(format!("{owner}@example.com"))
    .bind(old)
    .bind(old)
    .execute(store.db())
    .await
    .expect("account");
    hive_core::pgq::query(
        "INSERT INTO mail_messages (id, account_id, jmap_id, jmap_thread_id, received_at, \
         user_scope, created_at, updated_at) VALUES (?, ?, 'j1', 't1', ?, ?, ?, ?)",
    )
    .bind(format!("msg-{owner}"))
    .bind(format!("acct-{owner}"))
    .bind(old)
    .bind(owner)
    .bind(old)
    .bind(old)
    .execute(store.db())
    .await
    .expect("message");
    hive_core::pgq::query(
        "INSERT INTO blobs (hash, size, mime, data, created_at) \
         VALUES (?, 4, 'text/plain', ?, ?)",
    )
    .bind(hash)
    .bind(owner.as_bytes().to_vec())
    .bind(old)
    .execute(store.db())
    .await
    .expect("blob");
    hive_core::pgq::query(
        "INSERT INTO mail_attachments (id, message_id, blob_hash, jmap_blob_id, created_at) \
         VALUES (?, ?, ?, 'b1', ?)",
    )
    .bind(format!("att-{owner}"))
    .bind(format!("msg-{owner}"))
    .bind(hash)
    .bind(old)
    .execute(store.db())
    .await
    .expect("attachment");
}

/// The pointer still points, and the bytes are still behind it.
async fn assert_attachment_intact(store: &Store, org: Uuid, hash: &str, when: &str) {
    acting::scope(ActingScope::new(org, "bob".to_string(), true), async {
        let (pointer, data): (Option<String>, Option<Vec<u8>>) = hive_core::pgq::query_as(
            "SELECT t.blob_hash, b.data FROM mail_attachments t \
             LEFT JOIN blobs b ON b.org_id = t.org_id AND b.hash = t.blob_hash \
             WHERE t.id = 'att-bob'",
        )
        .fetch_one(store.db())
        .await
        .expect("beta's attachment row");
        assert_eq!(
            pointer.as_deref(),
            Some(hash),
            "beta's attachment lost its blob pointer {when}"
        );
        assert_eq!(
            data.as_deref(),
            Some(&b"bob"[..]),
            "beta's attachment bytes were destroyed {when}"
        );
    })
    .await;
}

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
