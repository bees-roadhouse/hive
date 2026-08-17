// The privilege-escalation chain, walked end to end through the real router.
//
// A plain member used to be able to become an admin and take over another
// account, in four moves that were each individually defensible:
//
//   1. PATCH their own `people` row to `{name: <an admin>, kind: ai, owner:
//      self}` — the gate was evaluated against the row BEFORE the patch, and
//      `people_ais_owned_by` is a string compare with no foreign key.
//   2. Grant themselves an OAuth token for that identity. Consent only asks
//      "do you own this AI", and after (1) they did.
//   3. Present the token to an admin route. Admin was decided by matching
//      `api_tokens.actor` — free text, chosen at mint — against `users_list()`,
//      which has no org filter.
//   4. `POST /api/actors/<victim>/merge`, which reassigns every row owned by
//      one actor to another in a single transaction.
//
// The first three tests are one link each. They are separate on purpose: a
// chain that breaks in two places should be seen to break in two places, and a
// single test asserting only the final 403 would still pass if the earlier
// links silently came back. The fourth is not part of the chain — it is the
// same credential's other integrity property, that an auth code is single-use
// even when two redemptions race.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use hive_core::acting::{self, ActingScope};
use hive_core::store::users::NewUser;
use hive_shared::{ActorKind, UserRole};
use serde_json::{json, Value};
use tower::ServiceExt;
use uuid::Uuid;

use hive_api::store::Store;

async fn test_app() -> (Router, Store, hive_api::db::TestDb) {
    std::env::set_var("HIVE_EMBED", "hash");
    // STRICT: no fallback scope, so the test sees what the binary sees.
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

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).expect("request")
}

fn post(path: &str, cookie: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::from(body.to_string())).expect("request")
}

fn patch(path: &str, cookie: &str, body: Value) -> Request<Body> {
    Request::patch(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::COOKIE, cookie)
        .body(Body::from(body.to_string()))
        .expect("request")
}

fn bearer_get(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request")
}

fn bearer_post(path: &str, token: &str, body: Value) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
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

/// Onboard the instance. The first admin's actor is `alice`, in the default
/// org — the name every escalation below tries to borrow.
async fn onboard(app: &Router) -> String {
    let (status, body, headers) = send(
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
    assert_eq!(status, StatusCode::CREATED, "onboarding: {body}");
    assert_eq!(body["user"]["role"], "admin");
    session_cookie(&headers)
}

/// A second org holding one PLAIN MEMBER, provisioned the way an operator
/// would. Returns (org id, bob's session cookie).
async fn beta_with_member(store: &Store, app: &Router) -> (Uuid, String) {
    let org = store.orgs_create("beta", "Beta").await.expect("create org");
    acting::scope(ActingScope::new(org.id, "bob".to_string(), true), async {
        store
            .users_create(
                NewUser {
                    name: "bob".to_string(),
                    email: "bob@example.com".to_string(),
                    password: "correct-horse".to_string(),
                    role: Some(UserRole::Member),
                    actor: Some("bob".to_string()),
                    kind: Some(ActorKind::Human),
                },
                "operator",
            )
            .await
            .expect("create user");
    })
    .await;
    (org.id, login(app, "bob@example.com", None).await)
}

async fn login(app: &Router, email: &str, org: Option<&str>) -> String {
    let mut body = json!({"email": email, "password": "correct-horse"});
    if let Some(org) = org {
        body["org"] = json!(org);
    }
    let (status, out, headers) = send(app, post("/api/auth/login", None, body)).await;
    assert_eq!(status, StatusCode::OK, "login {email}: {out}");
    session_cookie(&headers)
}

const REDIRECT_URI: &str = "http://localhost:31337/callback";
const PKCE_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const PKCE_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

/// Walk the consent half of the OAuth flow as `cookie`'s human and return
/// (client_id, auth code) for `ai_actor`. This is the member's own door — no
/// admin involved anywhere in it.
async fn oauth_code_for(app: &Router, cookie: &str, ai_actor: &str) -> (String, String) {
    let redirect_uri = REDIRECT_URI;
    let challenge = PKCE_CHALLENGE;
    let (status, client, _) = send(
        app,
        post(
            "/oauth/register",
            None,
            json!({"client_name": "MCP client", "redirect_uris": [redirect_uri]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register: {client}");
    let client_id = client["client_id"].as_str().expect("client_id").to_string();

    let (status, ctx, _) = send(
        app,
        get(
            &format!(
                "/oauth/authorize/context?client_id={}",
                urlencoding::encode(&client_id)
            ),
            Some(cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "consent context: {ctx}");
    let csrf = ctx["csrf"].as_str().expect("csrf").to_string();

    let (status, grant, _) = send(
        app,
        post(
            "/oauth/authorize/grant",
            Some(cookie),
            json!({
                "client_id": client_id,
                "redirect_uri": redirect_uri,
                "code_challenge": challenge,
                "state": "abc",
                "scope": "mcp",
                "ai_actor": ai_actor,
                "csrf": csrf,
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "consent grant: {grant}");
    let code = reqwest::Url::parse(grant["redirect"].as_str().expect("redirect"))
        .expect("redirect url")
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .expect("code");
    (client_id, code)
}

/// Exchange an auth code at the token endpoint.
async fn redeem(app: &Router, client_id: &str, code: &str) -> (StatusCode, Value) {
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", PKCE_VERIFIER),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", client_id),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
    .collect::<Vec<_>>()
    .join("&");
    let (status, body, _) = send(
        app,
        Request::post("/oauth/token")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(form))
            .expect("request"),
    )
    .await;
    (status, body)
}

async fn oauth_token_for(app: &Router, cookie: &str, ai_actor: &str) -> String {
    let (client_id, code) = oauth_code_for(app, cookie, ai_actor).await;
    let (status, token) = redeem(app, &client_id, &code).await;
    assert_eq!(status, StatusCode::OK, "token: {token}");
    token["access_token"].as_str().expect("token").to_string()
}

async fn mcp_call(app: &Router, token: &str, name: &str, arguments: Value) -> String {
    let (status, body, _) = send(
        app,
        Request::post("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "method": "tools/call",
                    "params": {"name": name, "arguments": arguments},
                    "id": 1
                })
                .to_string(),
            ))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mcp {name}: {body}");
    body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// Link 1. A delegate does not get to rewrite the delegation gate. `kind` and
/// `owner` decide who may grant a token for an identity, so they are not
/// self-editable, and the rename that carried the takeover is refused because
/// the gate is asked of the PATCHED row too.
#[tokio::test]
async fn a_member_cannot_rewrite_their_own_delegation_gate() {
    let (app, store, _test_db) = test_app().await;
    let _alice = onboard(&app).await;
    let (_beta, bob) = beta_with_member(&store, &app).await;

    // The whole move, in one PATCH of bob's own row.
    let (status, body, _) = send(
        &app,
        patch(
            "/api/people/bob",
            &bob,
            json!({"name": "alice", "kind": "ai", "owner": "bob"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "the whole move: {body}");

    // And each half of it on its own.
    for attempt in [
        json!({"kind": "ai"}),
        json!({"owner": "bob"}),
        json!({"name": "alice"}),
        json!({"name": "bobby"}),
    ] {
        let (status, body, _) = send(&app, patch("/api/people/bob", &bob, attempt.clone())).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{attempt}: {body}");
    }

    // The row is exactly as it was, and bob owns no AI to grant.
    let (status, person, _) = send(&app, get("/api/people/bob", Some(&bob))).await;
    assert_eq!(status, StatusCode::OK, "read back: {person}");
    assert_eq!(person["slug"], "bob");
    assert_eq!(person["kind"], "human");
    assert_eq!(person["owner"], Value::Null);

    // What a member may still do to their own card: bio and role. Those are
    // the fields the identity screen actually writes.
    let (status, person, _) = send(
        &app,
        patch("/api/people/bob", &bob, json!({"bio": "runs the smoker"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "bio edit: {person}");
    assert_eq!(person["bio"], "runs the smoker");
}

/// Link 3, isolated from link 1: even GIVEN an AI identity named after an
/// admin — here handed to bob by operator fiat, so link 1 is not the reason
/// this fails — the token it mints is not an admin token. Admin is a property
/// of the granting human's membership, never of the name on the credential.
#[tokio::test]
async fn a_token_named_after_an_admin_is_not_an_admin() {
    let (app, store, _test_db) = test_app().await;
    let _alice_cookie = onboard(&app).await;
    let (beta, bob) = beta_with_member(&store, &app).await;

    // An AI called `alice`, owned by bob, inside beta. The slug collides with
    // the default org's ADMIN — legally, because `people` is unique per org.
    acting::scope(ActingScope::new(beta, "bob".to_string(), true), async {
        store
            .people_upsert("alice", "Alice", ActorKind::Ai, Some("bob"))
            .await
            .expect("ai identity");
        store
            .people_upsert("victim", "Victim", ActorKind::Human, None)
            .await
            .expect("victim");
    })
    .await;

    let token = oauth_token_for(&app, &bob, "alice").await;

    // The token works — it is a real credential, just not an admin one.
    let (status, entry, _) = send(
        &app,
        bearer_post("/api/journal", &token, json!({"body": "an ordinary write"})),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "ordinary write: {entry}");
    assert_eq!(entry["author"], "alice");

    // Every admin door, closed. `actors_merge` is the takeover: it reassigns
    // every row owned by one actor to another in one transaction.
    for req in [
        bearer_post("/api/actors/victim/merge", &token, json!({"into": "alice"})),
        bearer_post(
            "/api/actors/victim/merge?dryRun=1",
            &token,
            json!({"into": "alice"}),
        ),
        Request::delete("/api/actors/victim")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::empty())
            .expect("request"),
        bearer_post("/api/import", &token, json!({"journal": []})),
        bearer_post("/api/sources/poll", &token, json!({})),
        bearer_post(
            "/api/entity-types",
            &token,
            json!({"name": "Plant", "fields": []}),
        ),
        bearer_get("/api/users", &token),
        bearer_get("/api/tokens", &token),
        bearer_get("/api/oauth/clients", &token),
    ] {
        let path = req.uri().to_string();
        let (status, body, _) = send(&app, req).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
    }

    // The MCP door is the same door. Its refusal is error-shaped OK content.
    for (tool, args) in [
        ("actor_merge", json!({"from": "victim", "into": "alice"})),
        ("actor_delete", json!({"slug": "victim"})),
        (
            "entity_type_create",
            json!({"name": "Plant", "fields": [{"label": "Species", "field_type": "text"}]}),
        ),
    ] {
        let text = mcp_call(&app, &token, tool, args).await;
        assert!(text.contains("forbidden"), "mcp {tool}: {text}");
    }

    // And the victim survived: merge did not run behind the 403.
    let (status, person, _) = send(&app, get("/api/people/victim", Some(&bob))).await;
    assert_eq!(status, StatusCode::OK, "victim still there: {person}");
}

/// The root cause the other two hang off. `users.role` is one global column;
/// authorization reads `memberships.role` for the ORG BEING ACTED IN. An
/// operator adding a global admin to a second org as a plain member must not
/// be handing them admin there.
#[tokio::test]
async fn a_global_admin_is_only_admin_where_their_membership_says_so() {
    let (app, store, _test_db) = test_app().await;
    let default_session = onboard(&app).await;
    let beta = store.orgs_create("beta", "Beta").await.expect("create org");

    let alice = store
        .users_by_email("alice@example.com")
        .await
        .expect("lookup")
        .expect("alice")
        .0;
    assert_eq!(alice.role, UserRole::Admin, "users.role is still admin");
    store
        .memberships_add(&alice.id, beta.id, "member")
        .await
        .expect("add to beta as a member");

    // In her own org she is an admin, as before.
    let (status, users, _) = send(&app, get("/api/users", Some(&default_session))).await;
    assert_eq!(status, StatusCode::OK, "admin in default: {users}");

    // In beta she is a member, and the account-wide `role` column does not
    // travel with her.
    let beta_session = login(&app, "alice@example.com", Some("beta")).await;
    for path in ["/api/users", "/api/tokens", "/api/oauth/clients"] {
        let (status, body, _) = send(&app, get(path, Some(&beta_session))).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{path} in beta: {body}");
    }
    let (status, body, _) = send(
        &app,
        post("/api/import", Some(&beta_session), json!({"journal": []})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "import in beta: {body}");

    // `/api/auth/me` reports the role the session actually holds, so the SPA
    // does not offer a nav the API will refuse.
    let (status, me, _) = send(&app, get("/api/auth/me", Some(&beta_session))).await;
    assert_eq!(status, StatusCode::OK, "me: {me}");
    assert_eq!(me["user"]["role"], "member", "acting-org role: {me}");
    let (_, me, _) = send(&app, get("/api/auth/me", Some(&default_session))).await;
    assert_eq!(me["user"]["role"], "admin", "still admin at home: {me}");
}

/// An auth code is single-use, and "single" has to survive a race. The
/// redemption read had no `FOR UPDATE`, so under READ COMMITTED two concurrent
/// `/oauth/token` posts both saw `used_at IS NULL`, both minted a token, and
/// the compromise branch that revokes on reuse never fired.
///
/// The assertion is deterministic once the row is locked: exactly one
/// redemption succeeds, and because every loser treats the reuse as a
/// compromise, the client is left holding no token at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_redemptions_of_one_code_mint_at_most_one_token() {
    let (app, store, _test_db) = test_app().await;
    let alice = onboard(&app).await;
    let default_org = store.orgs_default().await.expect("default org").id;
    acting::scope(
        ActingScope::new(default_org, "alice".to_string(), true),
        async {
            store
                .people_upsert("pia", "Pia", ActorKind::Ai, Some("alice"))
                .await
                .expect("ai identity");
        },
    )
    .await;

    let (client_id, code) = oauth_code_for(&app, &alice, "pia").await;
    let results = futures::future::join_all((0..4).map(|_| redeem(&app, &client_id, &code))).await;

    let minted = results
        .iter()
        .filter(|(status, _)| *status == StatusCode::OK)
        .count();
    assert_eq!(minted, 1, "one code, one token: {results:?}");
    for (status, body) in results.iter().filter(|(s, _)| *s != StatusCode::OK) {
        assert_eq!(*status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "invalid_grant");
    }

    // Reuse is treated as a compromise, so the one token that WAS minted is
    // revoked along with it: the client ends up with nothing rather than with a
    // credential someone else also holds.
    let tokens = store.tokens_list().await.expect("tokens");
    assert!(
        !tokens
            .iter()
            .any(|t| t.client_id.as_deref() == Some(client_id.as_str())),
        "replay must revoke the client's tokens"
    );
}
