// End-to-end parity smoke: drives the full router (middleware included) over
// a temp database the way a browser + CLI would — onboarding, sessions, journal
// emergence, ACL, search, tokens, admin lifecycle, import idempotency.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, StatusCode};
use axum::routing::{get as route_get, post as route_post};
use axum::Json;
use axum::Router;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;
use tower::ServiceExt;

static ENV_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

async fn env_guard() -> tokio::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn reset_auth_env() {
    for key in [
        "HIVE_LOCAL_AUTH_ENABLED",
        "HIVE_OIDC_ENABLED",
        "HIVE_OAUTH_ALLOW_NEVER_EXPIRES",
        "OIDC_ISSUER",
        "OIDC_CLIENT_ID",
        "OIDC_CLIENT_SECRET",
        "OIDC_REDIRECT_URI",
        "OIDC_ALLOWED_DOMAINS",
    ] {
        std::env::remove_var(key);
    }
}

async fn test_app() -> (Router, hive_api::store::Store) {
    // Hash embedder: deterministic + offline (set before any embed call; the
    // provider choice is latched once per process).
    std::env::set_var("HIVE_EMBED", "hash");
    // Isolated Postgres schema per test (uses DATABASE_URL / local dev default).
    let pool = hive_api::db::test_pool().await;
    let store = hive_api::store::Store::new(pool);
    (hive_api::routes::router(store.clone()), store)
}

async fn backfill_embeddings(store: &hive_api::store::Store) {
    for it in store.embeddable_items().await.expect("embeddable items") {
        let vec = hive_embed::embed(&it.embed_text);
        hive_api::pgq::query(
            "INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(ref_kind, ref_id) DO UPDATE SET \
               model=excluded.model, dim=excluded.dim, vec=excluded.vec, \
               hash=excluded.hash, created_at=excluded.created_at",
        )
        .bind(&it.kind)
        .bind(&it.id)
        .bind(hive_embed::embed_model())
        .bind(vec.len() as i64)
        .bind(hive_embed::to_blob(&vec))
        .bind(&it.hash)
        .bind(hive_api::store::now_iso())
        .execute(store.db())
        .await
        .expect("embedding upsert");
    }
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Value, axum::http::HeaderMap) {
    let res = app.clone().oneshot(req).await.expect("request");
    let status = res.status();
    let headers = res.headers().clone();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, body, headers)
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).unwrap()
}

fn post_json(path: &str, body: Value, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::post(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn post_json_bearer(path: &str, body: Value, token: &str) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn post_form(path: &str, pairs: &[(&str, &str)]) -> Request<Body> {
    let body = pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

struct OAuthGrantFixture<'a> {
    cookie: &'a str,
    client_id: &'a str,
    redirect_uri: &'a str,
    challenge: &'a str,
    verifier: &'a str,
    csrf: &'a str,
}

async fn grant_oauth_token(app: &Router, fixture: &OAuthGrantFixture<'_>, ttl: i64) -> Value {
    let (status, grant, _) = send(
        app,
        post_json(
            "/oauth/authorize/grant",
            json!({
                "client_id": fixture.client_id,
                "redirect_uri": fixture.redirect_uri,
                "code_challenge": fixture.challenge,
                "state": "abc",
                "scope": "mcp",
                "ai_actor": "pia",
                "csrf": fixture.csrf,
                "token_ttl_secs": ttl,
            }),
            Some(fixture.cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "grant: {grant}");
    let redirect = grant["redirect"].as_str().unwrap();
    let url = reqwest::Url::parse(redirect).unwrap();
    let code = url
        .query_pairs()
        .find(|(k, _)| k == "code")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    assert_eq!(
        url.query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .as_deref(),
        Some("abc")
    );
    let (status, token, _) = send(
        app,
        post_form(
            "/oauth/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("code_verifier", fixture.verifier),
                ("redirect_uri", fixture.redirect_uri),
                ("client_id", fixture.client_id),
            ],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "token: {token}");
    token
}

fn patch_json(path: &str, body: Value, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::patch(path).header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn bearer(path: &str, token: &str) -> Request<Body> {
    Request::get(path)
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

const OIDC_TEST_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQClUE7f4AGyfdwv
ycn1RBfPrCm8fTM+5m5DCgXeTi3b+ne7RpvlxHMfHVyyNMOLuE0TvXtR7HdO5tiI
XE5OBsKVHWUIE8KGTK6cM1jrcXsTuLoZYRUicGoxOzTUAQh6Ys+/i87K+1IgdIAK
OynROZvGkHdwqKxMoMmKANda9J+kuAtUJTLFYDcq9XjwOaJP5Z1BhYfTJ4dmF/Mu
EFqQBqKntpUq9b3syOuGWKAYgtD+kcAe0XQ6InibQrqi0e+COiQfKiD3dKudPM2R
FbhOa7u9e1aA+9ne0AO2nhQBJ2C9HuJg8ZAX5xskoaQxMoNn9msm5yc8l/y4HACF
Zio4L6M7AgMBAAECggEAF3/G9oP9OcYyWoiwsLCxQdATTrvtYO+YlOcD1on+ctqz
0mdDGfJG+xFNb/eYJHBaZIf207pta0XdWeTlLKpBVrkK9473g+e6mnGiHjXPbQpB
SgJG4tJgBgeIhupurhcFuRDCoJABKKPm341xcFBkGGHI2LbhZzMj8v4TntZPKzbT
Wq+UBHDbLuuHzbanH+qPQSIEiuGcVDR7L9eGRkKfAcgWoEgHtaSdmdUbNrJKvsYm
fTXGbVfpWDYW6Jk87mse1zEv+RvUf8/n/wrGYatiXwtgJzdImvAHn/XSa4mTbixi
Taj+rWR1Pxy6xDsRsicmY1pBf38gMpf+JbZKwyV5AQKBgQDoPv/SzRSqMKMprINz
0ISWK36X3hNqSx4qSzgCKzF8x9J1iMCEykL0V/ym2IK7HOz3vzqXdDW5E/Ng0Pem
8UrC8F5GniKMr/WwDbE2dDAo/Jy5F5VIcQfjmqpwqUqpW7VChkBjvlipJkyQouV3
nib5VZcIwY70vf/5EWpPIsr6WwKBgQC2OMn1WTga/6IJGjR0cew1Clc93WMOi9b1
EH8kruSck02GLWbL8JyFBLxX5SqVk8v9Jt04XAJUBmQmTdYsqAQ6wfnugsPzvR9U
gFlBpdzusDlR05jqZtbk3Qni1rVIsxZLDQ3TQBRA3yvOK8N8M4dSwHpN6s4fj+xN
3SnmCS+QoQKBgQCFHQ7OCSOOBICQc0OIzvwfgmB1tSCVrOZmQWShwZYEuhdDrJUD
x1Ym7INwMeqESqj7uwxfIIlmQiwd0sgPVH+QSesPOLX+wx/jv4VR+7ha1acSY5T5
x2dJKi4EktOrTFgRABfJ06DHmp8Jy4QQUoJuKIN/zkkcuAYOANBY+U0zvwKBgQCD
cJk1EeMnjmeqGy3lJNvWMpxVcqDmODaY1QpxQnqC+rn75Dn3N5sfVBgrapF6DX8i
HuuJoMzJIUcSXij0U0mhvJP02HxSD4RO5rn7YZHo1lKyVGhEBGRT96EO8AMZ6pxV
DJiBXgJ9/LzTXbwHlf+x0EcodwuxtpYkYDi9xrh5oQKBgQDPbF4qmQtk1TVrGvWD
QhOhxtRTzj9dTbjN2bfuSrG57XUzb2qOTBAbh7Iyaw2/NCvhLyRHUf1mJFcLScjq
1qb7yle/Uh5YYvL9/LqITKyK39OKWlWRTT4quasSc7vJ4a95dO7QZu3HMOZN477y
TcOrPdBrkCcLEQuH2iTbia2C+g==
-----END PRIVATE KEY-----"#;

const OIDC_TEST_JWK_N: &str = "pVBO3-ABsn3cL8nJ9UQXz6wpvH0zPuZuQwoF3k4t2_p3u0ab5cRzHx1csjTDi7hNE717Uex3TubYiFxOTgbClR1lCBPChkyunDNY63F7E7i6GWEVInBqMTs01AEIemLPv4vOyvtSIHSACjsp0TmbxpB3cKisTKDJigDXWvSfpLgLVCUyxWA3KvV48DmiT-WdQYWH0yeHZhfzLhBakAaip7aVKvW97MjrhligGILQ_pHAHtF0OiJ4m0K6otHvgjokHyog93SrnTzNkRW4Tmu7vXtWgPvZ3tADtp4UASdgvR7iYPGQF-cbJKGkMTKDZ_ZrJucnPJf8uBwAhWYqOC-jOw";

#[derive(Serialize)]
struct FakeOidcClaims<'a> {
    iss: &'a str,
    aud: &'a str,
    exp: i64,
    nonce: &'a str,
    email: &'a str,
    name: &'a str,
}

async fn fake_oidc_provider() -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            route_get(|State(issuer): State<String>| async move {
                Json(json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "jwks_uri": format!("{issuer}/jwks"),
                }))
            }),
        )
        .route(
            "/jwks",
            route_get(|| async {
                Json(json!({
                    "keys": [{
                        "kty": "RSA",
                        "kid": "hive-test",
                        "use": "sig",
                        "alg": "RS256",
                        "n": OIDC_TEST_JWK_N,
                        "e": "AQAB",
                    }]
                }))
            }),
        )
        .route(
            "/token",
            route_post(
                |State(issuer): State<String>,
                 axum::Form(form): axum::Form<HashMap<String, String>>| async move {
                    let code = form.get("code").map(String::as_str).unwrap_or("");
                    let mut header = Header::new(Algorithm::RS256);
                    header.kid = Some("hive-test".to_string());
                    let claims = FakeOidcClaims {
                        iss: &issuer,
                        aud: "hive-client",
                        exp: chrono::Utc::now().timestamp() + 600,
                        nonce: code,
                        email: "oidc-user@example.com",
                        name: "OIDC User",
                    };
                    let id_token = jsonwebtoken::encode(
                        &header,
                        &claims,
                        &EncodingKey::from_rsa_pem(OIDC_TEST_PRIVATE_KEY.as_bytes()).unwrap(),
                    )
                    .unwrap();
                    Json(json!({
                        "id_token": id_token,
                        "access_token": "fake-access-token",
                        "token_type": "Bearer",
                    }))
                },
            ),
        )
        .with_state(issuer.clone());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    issuer
}

/// Run onboarding, return the admin session cookie ("hive_session=...").
async fn onboard(app: &Router) -> String {
    let (status, body, headers) = send(
        app,
        post_json(
            "/api/onboarding",
            json!({
                "instanceName": "Test Hive",
                "adminName": "Nate",
                "adminEmail": "nate@example.com",
                "password": "hunter22-strong"
            }),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "onboarding: {body}");
    assert_eq!(body["user"]["role"], "admin");
    assert_eq!(body["user"]["actor"], "nate");
    let set_cookie = headers
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap();
    set_cookie.split(';').next().unwrap().to_string()
}

#[tokio::test]
async fn onboarding_gate_then_full_flow() {
    let _guard = env_guard().await;
    reset_auth_env();
    let (app, store) = test_app().await;

    // Fresh DB: healthz is public, everything else is locked behind onboarding.
    let (status, body, _) = send(&app, get("/api/healthz", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    let (status, body, _) = send(&app, get("/api/tasks", None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "onboarding_required");
    let (_, body, _) = send(&app, get("/api/onboarding/status", None)).await;
    assert_eq!(body["completed"], false);

    let cookie = onboard(&app).await;

    // Onboarding completes once; second attempt conflicts.
    let (status, _, _) =
        send(&app, post_json("/api/onboarding", json!({"instanceName":"x","adminName":"y","adminEmail":"z@example.com","password":"12345678"}), None)).await;
    assert_eq!(status, StatusCode::CONFLICT);

    // Unauthenticated API calls 401; session cookie works.
    let (status, body, _) = send(&app, get("/api/tasks", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "unauthenticated");
    let (status, body, _) = send(&app, get("/api/auth/me", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["principal"], "session");
    assert_eq!(body["user"]["actor"], "nate");

    // Login round-trip with the scrypt hash written at onboarding.
    let (status, body, headers) = send(
        &app,
        post_json(
            "/api/auth/login",
            json!({"email": "nate@example.com", "password": "hunter22-strong"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login: {body}");
    assert!(headers.get(header::SET_COOKIE).is_some());
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/auth/login",
            json!({"email": "nate@example.com", "password": "wrong-password"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Journal append: mentions fan the inbox, anchors emerge a task.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/people",
            json!({"name": "pia", "kind": "ai"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let entry_body = "Kickoff with @pia. We must ship the rust rewrite this week.";
    let task_start = entry_body.find("We must").unwrap() as i64;
    let (status, entry, _) = send(
        &app,
        post_json(
            "/api/journal",
            json!({
                "body": entry_body,
                "tags": ["rewrite"],
                "anchors": [{
                    "start": task_start,
                    "end": entry_body.len(),
                    "kind": "task",
                    "fields": {"title": "Ship the rust rewrite", "assignees": ["pia"], "priority": "high"}
                }]
            }),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "journal: {entry}");
    assert_eq!(
        entry["author"], "nate",
        "author comes from the session, not the body"
    );
    assert_eq!(entry["mentions"], json!(["pia"]));
    assert_eq!(entry["anchors"].as_array().map(Vec::len), Some(1));
    let task_id = entry["anchors"][0]["ref_id"]
        .as_str()
        .expect("anchored task id")
        .to_string();

    // The anchored task exists with the anchor fields applied.
    let (status, task, _) = send(&app, get(&format!("/api/tasks/{task_id}"), Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["title"], "Ship the rust rewrite");
    assert_eq!(task["priority"], "high");
    assert_eq!(task["assignees"], json!(["pia"]));
    assert_eq!(task["origin_entry_id"], entry["id"]);

    // Mention + assignment landed in pia's inbox.
    let (_, inbox, _) = send(&app, get("/api/inbox/pia", Some(&cookie))).await;
    assert!(
        inbox.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "pia inbox should have entries: {inbox}"
    );

    // Task workflow PATCH re-indexes and bumps updated_at.
    let (status, task, _) = send(
        &app,
        patch_json(
            &format!("/api/tasks/{task_id}"),
            json!({"status": "doing"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(task["status"], "doing");

    // FTS search finds the entry; semantic mode works on the hash provider.
    let (status, hits, _) = send(&app, get("/api/search?q=rewrite", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        hits.as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "fts hits: {hits}"
    );
    let (status, hits, _) = send(
        &app,
        get("/api/search?q=rust+rewrite&mode=standard", Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "semantic: {hits}");

    // Recall exposes the structured Node/shared shape: journal hit metadata
    // nests under `hit`, while author/created_at sit beside it.
    backfill_embeddings(&store).await;
    let (status, recall, _) = send(
        &app,
        post_json(
            "/api/recall",
            json!({"identity": "pia", "peer": "nate", "query": "Kickoff"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "recall: {recall}");
    assert!(
        recall["brief"]
            .as_str()
            .unwrap_or_default()
            .contains("Recall for pia"),
        "recall brief should be ready to inject: {recall}"
    );
    let recalled = recall["data"]["journal"]
        .as_array()
        .expect("recall journal array");
    assert!(!recalled.is_empty(), "recall journal hits: {recall}");
    assert_eq!(
        recalled[0]["kind"],
        Value::Null,
        "hit should not be flattened"
    );
    assert_eq!(recalled[0]["hit"]["kind"], "journal");
    assert!(recalled[0]["hit"]["title"]
        .as_str()
        .unwrap_or_default()
        .contains("Kickoff"));

    // Dashboard composes.
    let (status, dash, _) = send(&app, get("/api/dashboard", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(dash["entries"], 1);
    assert_eq!(dash["tasks"]["doing"], 1);

    // API tokens: mint (admin), then the bearer resolves to its actor.
    let (status, minted, _) = send(
        &app,
        post_json(
            "/api/tokens",
            json!({"actor": "pia", "label": "test", "expiresInDays": 30}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "token mint: {minted}");
    let plaintext = minted["token"].as_str().unwrap();
    assert!(plaintext.starts_with("hive_pat_"));
    let (status, me, _) = send(&app, bearer("/api/auth/me", plaintext)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["principal"], "token");
    assert_eq!(me["user"], Value::Null, "pia has no login account");

    let (status, minted, _) = send(
        &app,
        post_json(
            "/api/tokens",
            json!({"actor": "pia", "label": "never", "neverExpires": true}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "never token mint: {minted}");
    assert_eq!(minted["record"]["expires_at"], Value::Null);
    let plaintext = minted["token"].as_str().unwrap();
    let (status, me, _) = send(&app, bearer("/api/auth/me", plaintext)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["principal"], "token");

    // Bad bearer is rejected by the gate.
    let (status, _, _) = send(&app, bearer("/api/tasks", "hive_pat_not-a-real-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // MCP without auth: 401 + www-authenticate pointing at resource metadata.
    let (status, _, headers) = send(
        &app,
        Request::post("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"jsonrpc":"2.0","method":"tools/list","id":1}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let www = headers
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap_or_default().to_string())
        .unwrap_or_default();
    assert!(
        www.contains("oauth-protected-resource"),
        "www-authenticate should advertise resource metadata, got: {www:?}"
    );

    // Actor merge dryRun reports counts without mutating.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/people",
            json!({"name": "cera", "kind": "ai"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, preview, _) = send(
        &app,
        post_json(
            "/api/actors/pia/merge?dryRun=1",
            json!({"into": "cera"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "merge preview: {preview}");
    assert_eq!(preview["dryRun"], true);
    let (_, people, _) = send(&app, get("/api/people/pia", Some(&cookie))).await;
    assert_eq!(people["slug"], "pia", "dryRun must not delete the actor");

    // Bulk import is idempotent: second run skips everything.
    let payload = json!({
        "journal": [{"id": "jrn_legacy000001", "author": "cera", "body": "legacy entry", "tags": [], "created_at": "2025-01-01T00:00:00.000Z"}],
        "projects": [{"id": "proj_legacy0001", "name": "Legacy Project", "slug": "legacy-project", "created_at": "2025-01-01T00:00:00.000Z"}]
    });
    let (status, first, _) = send(
        &app,
        post_json("/api/import", payload.clone(), Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "import: {first}");
    assert_eq!(first["journal"]["inserted"], 1);
    let (_, second, _) = send(&app, post_json("/api/import", payload, Some(&cookie))).await;
    assert_eq!(second["journal"]["inserted"], 0);
    assert_eq!(second["journal"]["skipped"], 1);

    // OAuth discovery is public.
    let (status, disco, _) = send(&app, get("/.well-known/oauth-authorization-server", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(disco["authorization_endpoint"]
        .as_str()
        .unwrap_or_default()
        .ends_with("/authorize"));

    // Logout clears the session.
    let (status, _, _) = send(
        &app,
        post_json("/api/auth/logout", json!({}), Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _, _) = send(&app, get("/api/tasks", Some(&cookie))).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "session must be dead after logout"
    );
}

#[tokio::test]
async fn oidc_callback_provisions_allowed_user_and_sets_session() {
    let _guard = env_guard().await;
    reset_auth_env();
    let issuer = fake_oidc_provider().await;
    std::env::set_var("HIVE_OIDC_ENABLED", "true");
    std::env::set_var("OIDC_ISSUER", &issuer);
    std::env::set_var("OIDC_CLIENT_ID", "hive-client");
    std::env::set_var("OIDC_CLIENT_SECRET", "fake-secret");
    std::env::set_var(
        "OIDC_REDIRECT_URI",
        "http://localhost/api/auth/oidc/callback",
    );
    std::env::set_var("OIDC_ALLOWED_DOMAINS", "example.com");

    let (app, _dir) = test_app().await;
    let _admin_cookie = onboard(&app).await;

    let (status, body, _) = send(&app, get("/api/auth/config", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["oidc"], true);
    assert_eq!(body["localAuth"], true);

    let (status, _, headers) =
        send(&app, get("/api/auth/oidc/start?return_to=%2Faccount", None)).await;
    assert_eq!(status, StatusCode::FOUND);
    let location = headers
        .get(header::LOCATION)
        .expect("oidc start redirect")
        .to_str()
        .unwrap();
    let auth_url = reqwest::Url::parse(location).unwrap();
    assert!(auth_url
        .as_str()
        .starts_with(&format!("{issuer}/authorize")));
    let state = auth_url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("state param");
    let nonce = auth_url
        .query_pairs()
        .find(|(k, _)| k == "nonce")
        .map(|(_, v)| v.into_owned())
        .expect("nonce param");
    let cookie = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string())
        .collect::<Vec<_>>()
        .join("; ");
    assert!(cookie.contains("hive_oidc_state="));
    assert!(cookie.contains("hive_oidc_nonce="));
    assert!(cookie.contains("hive_oidc_return="));

    let callback = format!("/api/auth/oidc/callback?code={nonce}&state={state}");
    let (status, _, headers) = send(&app, get(&callback, Some(&cookie))).await;
    assert_eq!(status, StatusCode::FOUND);
    assert_eq!(
        headers
            .get(header::LOCATION)
            .expect("return redirect")
            .to_str()
            .unwrap(),
        "/account"
    );
    let set_cookies = headers
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let session_cookie = set_cookies
        .iter()
        .find_map(|v| {
            v.strip_prefix("hive_session=")
                .map(|_| v.split(';').next().unwrap())
        })
        .expect("session cookie")
        .to_string();
    assert!(set_cookies
        .iter()
        .any(|v| v.starts_with("hive_oidc_state=;")));
    assert!(set_cookies
        .iter()
        .any(|v| v.starts_with("hive_oidc_nonce=;")));
    assert!(set_cookies
        .iter()
        .any(|v| v.starts_with("hive_oidc_return=;")));

    let (status, me, _) = send(&app, get("/api/auth/me", Some(&session_cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["principal"], "session");
    assert_eq!(me["user"]["email"], "oidc-user@example.com");
    assert_eq!(me["user"]["name"], "OIDC User");
    assert_eq!(me["user"]["role"], "member");
}

#[tokio::test]
async fn local_auth_can_be_disabled_globally() {
    let _guard = env_guard().await;
    reset_auth_env();
    std::env::set_var("HIVE_LOCAL_AUTH_ENABLED", "false");

    let (app, _store) = test_app().await;
    let cookie = onboard(&app).await;

    let (status, body, _) = send(&app, get("/api/auth/config", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["localAuth"], false);

    let (status, login, _) = send(
        &app,
        post_json(
            "/api/auth/login",
            json!({"email": "nate@example.com", "password": "hunter22-strong"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "login disabled: {login}");
    assert_eq!(login["error"], "local_auth_disabled");

    let (status, me, _) = send(&app, get("/api/auth/me", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["principal"], "session");
}

#[tokio::test]
async fn oauth_mcp_flow_issues_long_and_never_tokens() {
    let _guard = env_guard().await;
    reset_auth_env();
    let (app, _store) = test_app().await;
    let cookie = onboard(&app).await;

    let (status, _, _) = send(
        &app,
        post_json(
            "/api/people",
            json!({"name": "pia", "kind": "ai"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, pia, _) = send(
        &app,
        patch_json("/api/people/pia", json!({"owner": "nate"}), Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner patch: {pia}");
    assert_eq!(pia["owner"], "nate");

    let (status, client, _) = send(
        &app,
        post_json(
            "/oauth/register",
            json!({
                "client_name": "Claude MCP",
                "redirect_uris": ["http://localhost:31337/callback"]
            }),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register: {client}");
    let client_id = client["client_id"].as_str().unwrap();
    let redirect_uri = "http://localhost:31337/callback";
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";

    let authorize_path = format!(
        "/authorize?response_type=code&client_id={}&redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&scope=mcp&state=abc",
        urlencoding::encode(client_id),
        urlencoding::encode(redirect_uri),
    );
    let (status, _, headers) = send(&app, get(&authorize_path, None)).await;
    assert_eq!(status, StatusCode::FOUND);
    let location = headers.get(header::LOCATION).unwrap().to_str().unwrap();
    assert!(
        location.starts_with("/consent?"),
        "authorize should hand off to consent: {location}"
    );

    let (status, ctx, _) = send(
        &app,
        get(
            &format!(
                "/oauth/authorize/context?client_id={}",
                urlencoding::encode(client_id)
            ),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "context: {ctx}");
    assert_eq!(ctx["allow_never_expires"], true);
    assert!(ctx["identities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["slug"] == "pia"));
    let csrf = ctx["csrf"].as_str().unwrap();
    let grant_fixture = OAuthGrantFixture {
        cookie: &cookie,
        client_id,
        redirect_uri,
        challenge,
        verifier,
        csrf,
    };

    let long_ttl = 90 * 24 * 60 * 60;
    let long_token = grant_oauth_token(&app, &grant_fixture, long_ttl).await;
    assert_eq!(long_token["expires_in"], long_ttl);
    assert_eq!(long_token["scope"], "mcp");

    let never_token = grant_oauth_token(&app, &grant_fixture, 0).await;
    assert_eq!(never_token["expires_in"], Value::Null);
    let access_token = never_token["access_token"].as_str().unwrap();
    assert!(access_token.starts_with("hive_pat_"));

    let (status, mcp, _) = send(
        &app,
        Request::post("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {access_token}"))
            .body(Body::from(
                json!({"jsonrpc":"2.0","method":"tools/list","id":1}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "mcp: {mcp}");
    assert!(mcp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|t| t["name"] == "recall"));

    let (status, entry, _) = send(
        &app,
        post_json_bearer(
            "/api/journal",
            json!({"body": "Pia writes through an OAuth MCP token."}),
            access_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "oauth journal write: {entry}");
    assert_eq!(entry["author"], "pia");
    assert_eq!(entry["user_scope"], "nate");
}

#[tokio::test]
async fn viewer_acl_scopes_journal() {
    let _guard = env_guard().await;
    reset_auth_env();
    let (app, _dir) = test_app().await;
    let cookie = onboard(&app).await;

    // Two more humans: maggie gets a login; bob is just a person.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/users",
            json!({"name": "Maggie", "email": "maggie@example.com", "password": "maggie-secret-1"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, _, headers) = send(
        &app,
        post_json(
            "/api/auth/login",
            json!({"email": "maggie@example.com", "password": "maggie-secret-1"}),
            None,
        ),
    )
    .await;
    let maggie_cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // Nate writes a private entry and one mentioning maggie.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/journal",
            json!({"body": "private nate-only note"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/journal",
            json!({"body": "hey @maggie the garden plan is ready"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Maggie's viewer-scoped journal: sees the mention, not the private note.
    let (status, visible, _) = send(
        &app,
        get("/api/journal?viewer=maggie", Some(&maggie_cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let bodies: Vec<String> = visible
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        bodies.iter().any(|b| b.contains("garden plan")),
        "mention visible: {bodies:?}"
    );
    assert!(
        !bodies.iter().any(|b| b.contains("private nate-only")),
        "private hidden: {bodies:?}"
    );

    // An explicit journal-scope share opens nate's whole journal to maggie.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/shares",
            json!({"scope": "journal", "ref": "nate", "viewer": "maggie"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, visible, _) = send(
        &app,
        get("/api/journal?viewer=maggie", Some(&maggie_cookie)),
    )
    .await;
    let bodies: Vec<String> = visible
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        bodies.iter().any(|b| b.contains("private nate-only")),
        "shared journal visible: {bodies:?}"
    );
}

#[tokio::test]
async fn semantic_scope_runs_before_truncation_and_recall_filters_kinds_in_search() {
    let _guard = env_guard().await;
    reset_auth_env();
    let (app, store) = test_app().await;
    let cookie = onboard(&app).await;

    // maggie: a second, non-admin login.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/users",
            json!({"name": "Maggie", "email": "maggie@example.com", "password": "maggie-secret-1"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, _, headers) = send(
        &app,
        post_json(
            "/api/auth/login",
            json!({"email": "maggie@example.com", "password": "maggie-secret-1"}),
            None,
        ),
    )
    .await;
    let maggie_cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // maggie's own note, then nate floods the vector space with three private
    // entries that match the query exactly (strictly higher cosine than hers).
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/journal",
            json!({"body": "alpha hive inspection notes from the west garden"}),
            Some(&maggie_cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    for _ in 0..3 {
        let (status, _, _) = send(
            &app,
            post_json(
                "/api/journal",
                json!({"body": "alpha hive inspection notes"}),
                Some(&cookie),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    backfill_embeddings(&store).await;

    // limit=2: before the fix the two top slots were nate's private entries,
    // scoped away only AFTER the cut — maggie got an empty result back.
    let (status, hits, _) = send(
        &app,
        get(
            "/api/search?q=alpha%20hive%20inspection%20notes&mode=semantic&limit=2",
            Some(&maggie_cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let titles: Vec<String> = hits
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["title"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        titles.iter().any(|t| t.starts_with("maggie:")),
        "viewer-visible hit starved out of the pool: {titles:?}"
    );
    assert!(
        !titles.iter().any(|t| t.starts_with("nate:")),
        "cross-namespace leak: {titles:?}"
    );

    // Ten tasks that outscore every journal entry for this query used to fill
    // semantic_search's 8-hit pool before recall's journal post-filter ran,
    // emptying the brief (DIRECTION.md D9). The kinds filter now runs inside
    // the search, so the pool is journal-only from the start.
    for _ in 0..10 {
        store
            .tasks_create(
                hive_api::store::tasks::TaskCreate {
                    title: "queen brood frame audit notes".to_string(),
                    body: "queen brood frame audit notes".to_string(),
                    assignees: vec!["nate".to_string()],
                    ..Default::default()
                },
                "nate",
            )
            .await
            .expect("task create");
    }
    backfill_embeddings(&store).await;
    let recall = store
        .recall(
            "nate",
            hive_api::store::recall::RecallOptions {
                query: Some("queen brood frame audit notes".to_string()),
                ..Default::default()
            },
        )
        .await
        .expect("recall");
    assert!(
        !recall.data.journal.is_empty(),
        "task noise crowded journal out of the recall brief"
    );
    assert!(recall
        .data
        .journal
        .iter()
        .all(|h| h.hit.kind == hive_shared::EntityKind::Journal));

    // A top-scoring embeddings row of a kind this build doesn't know (written
    // by a newer binary) must not hold result slots on the UNSCOPED path
    // either: admission drops it before the cut, so the admin still gets the
    // best parseable hit instead of an empty result.
    let alien = hive_embed::embed_query("alpha hive inspection notes");
    hive_api::pgq::query(
        "INSERT INTO embeddings (ref_kind, ref_id, model, dim, vec, hash, created_at) \
         VALUES ('document', 'doc_alien', ?, ?, ?, 'alien', ?)",
    )
    .bind(hive_embed::embed_model())
    .bind(alien.len() as i64)
    .bind(hive_embed::to_blob(&alien))
    .bind(hive_api::store::now_iso())
    .execute(store.db())
    .await
    .expect("alien embedding row");
    let (status, hits, _) = send(
        &app,
        get(
            "/api/search?q=alpha%20hive%20inspection%20notes&mode=semantic&limit=1",
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        !hits.as_array().unwrap().is_empty(),
        "unknown-kind row starved the unscoped result: {hits}"
    );
}

#[tokio::test]
async fn ai_token_memory_is_namespaced_and_mention_shared() {
    let _guard = env_guard().await;
    reset_auth_env();
    let (app, _dir) = test_app().await;
    let cookie = onboard(&app).await;

    // Maggie has her own user namespace.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/users",
            json!({"name": "Maggie", "email": "maggie@example.com", "password": "maggie-secret-1"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, _, headers) = send(
        &app,
        post_json(
            "/api/auth/login",
            json!({"email": "maggie@example.com", "password": "maggie-secret-1"}),
            None,
        ),
    )
    .await;
    let maggie_cookie = headers
        .get(header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    // Admin-mint the kind of token the Hive plugins use: actor=pia, namespace=Nate.
    let (status, minted, _) = send(
        &app,
        post_json(
            "/api/tokens",
            json!({"actor": "pia", "label": "plugin token", "neverExpires": true}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "token mint: {minted}");
    let pia_token = minted["token"].as_str().unwrap();

    let (status, _, _) = send(&app, bearer("/api/users", pia_token)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "delegated AI token must not inherit admin authority"
    );

    let (status, profile, _) = send(
        &app,
        post_json_bearer(
            "/api/profile/nate",
            json!({"sections": {"bio": "Pia should not be able to overwrite Nate."}}),
            pia_token,
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "delegated AI token must not edit another actor's profile: {profile}"
    );

    let (status, profile, _) = send(
        &app,
        post_json_bearer(
            "/api/profile/pia",
            json!({"sections": {"bio": "Pia can maintain her own identity card."}}),
            pia_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "self profile update: {profile}");
    assert_eq!(
        profile["body"]["sections"]["bio"],
        "Pia can maintain her own identity card."
    );

    let (_, mcp_profile, _) = send(
        &app,
        Request::post("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {pia_token}"))
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "method": "tools/call",
                    "params": {
                        "name": "profile_update",
                        "arguments": {
                            "actor": "nate",
                            "sections": {"bio": "blocked"}
                        }
                    },
                    "id": 1
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        mcp_profile["result"]["isError"], true,
        "MCP profile update: {mcp_profile}"
    );

    let (status, _, _) = send(
        &app,
        post_json(
            "/api/journal",
            json!({"body": "Maggie writes a private Maggie-only note."}),
            Some(&maggie_cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // The token writes as Pia but stores the entry in Nate's memory namespace.
    let (status, entry, _) = send(
        &app,
        post_json_bearer(
            "/api/journal",
            json!({"body": "Pia remembers a private Nate-only plugin setup detail."}),
            pia_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "pia journal write: {entry}");
    assert_eq!(entry["author"], "pia");
    assert_eq!(entry["user_scope"], "nate");

    // A subsequent plugin read with the same token sees its own prior memory.
    let (status, visible, _) = send(&app, bearer("/api/journal", pia_token)).await;
    assert_eq!(status, StatusCode::OK);
    let pia_bodies: Vec<String> = visible
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        pia_bodies.iter().any(|b| b.contains("private Nate-only")),
        "AI token should see its own namespace memory: {pia_bodies:?}"
    );
    assert!(
        !pia_bodies.iter().any(|b| b.contains("private Maggie-only")),
        "AI token must not see another user's unmentioned private memory: {pia_bodies:?}"
    );

    let (status, recall, _) = send(
        &app,
        post_json_bearer(
            "/api/recall",
            json!({"identity": "pia", "query": "private Nate-only plugin setup"}),
            pia_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "own recall should work: {recall}");

    let (status, recall, _) = send(
        &app,
        post_json_bearer(
            "/api/recall",
            json!({"identity": "apis", "query": "private"}),
            pia_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "cross-AI recall: {recall}");
    assert_eq!(recall["error"], "not_your_identity");

    let (_, mcp_recall, _) = send(
        &app,
        Request::post("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::AUTHORIZATION, format!("Bearer {pia_token}"))
            .body(Body::from(
                json!({
                    "jsonrpc": "2.0",
                    "method": "tools/call",
                    "params": {
                        "name": "recall",
                        "arguments": {"identity": "apis", "query": "private"}
                    },
                    "id": 1
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(
        mcp_recall["result"]["isError"], true,
        "MCP recall: {mcp_recall}"
    );
    assert!(
        mcp_recall["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("not_your_identity"),
        "MCP recall denial should be explicit: {mcp_recall}"
    );

    // Maggie cannot see that private Nate/Pia memory.
    let (_, visible, _) = send(&app, get("/api/journal", Some(&maggie_cookie))).await;
    let maggie_bodies: Vec<String> = visible
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        !maggie_bodies
            .iter()
            .any(|b| b.contains("private Nate-only")),
        "unmentioned private memory must stay hidden: {maggie_bodies:?}"
    );

    // A mention shares a single Nate/Pia entry into Maggie's visible journal.
    let (status, _, _) = send(
        &app,
        post_json_bearer(
            "/api/journal",
            json!({"body": "Pia notes that @maggie should see the pantry migration context."}),
            pia_token,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, visible, _) = send(&app, get("/api/journal", Some(&maggie_cookie))).await;
    let maggie_bodies: Vec<String> = visible
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        maggie_bodies.iter().any(|b| b.contains("pantry migration")),
        "mention should share the entry to Maggie: {maggie_bodies:?}"
    );

    // Maggie mentioning Nate becomes visible to Nate's AI token too.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/journal",
            json!({"body": "Maggie writes a garden update for @nate."}),
            Some(&maggie_cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, visible, _) = send(&app, bearer("/api/journal", pia_token)).await;
    let pia_bodies: Vec<String> = visible
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(
        pia_bodies.iter().any(|b| b.contains("garden update")),
        "Nate-granted AI token should see entries shared with Nate: {pia_bodies:?}"
    );
}

#[tokio::test]
async fn spa_paths_are_not_gated() {
    let _guard = env_guard().await;
    reset_auth_env();
    let (app, _dir) = test_app().await;
    // Without onboarding, non-API paths must not 401/403 (the SPA has to load
    // so the wizard can run). Dist isn't present in tests → plain 404.
    let (status, _, _) = send(&app, get("/login", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // Unknown API path stays JSON-shaped (after onboarding it would be 404;
    // before onboarding the gate answers 403).
    let (status, body, _) = send(&app, get("/api/definitely-not-a-route", None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "onboarding_required");
}
