// Hosted-conversation lifecycle over the full router: DELETE /api/workspaces/{id}
// is owner-or-admin gated and cascades the transcript + `conversation` graph
// links while journal mirror entries (history) survive.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn test_app() -> (Router, hive_api::store::Store) {
    // Hash embedder: deterministic + offline (set before any embed call; the
    // provider choice is latched once per process).
    std::env::set_var("HIVE_EMBED", "hash");
    // Isolated Postgres schema per test (uses DATABASE_URL / local dev default).
    let pool = hive_api::db::test_pool().await;
    let store = hive_api::store::Store::new(pool);
    (hive_api::routes::router(store.clone()), store)
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

fn delete_req(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::delete(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).unwrap()
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
    let set_cookie = headers
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap();
    set_cookie.split(';').next().unwrap().to_string()
}

/// Create a member user via the admin and log them in; returns their cookie.
async fn member(app: &Router, admin_cookie: &str, name: &str, email: &str) -> String {
    let (status, body, _) = send(
        app,
        post_json(
            "/api/users",
            json!({"name": name, "email": email, "password": "member-secret-1"}),
            Some(admin_cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create {name}: {body}");
    let (status, body, headers) = send(
        app,
        post_json(
            "/api/auth/login",
            json!({"email": email, "password": "member-secret-1"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login {name}: {body}");
    headers
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn create_conversation(app: &Router, cookie: &str, title: &str) -> String {
    let (status, ws, _) = send(
        app,
        post_json(
            "/api/workspaces",
            json!({
                "title": title,
                "prompt": "kick off the retention docs",
                "project": "retrofit",
                "linked_entities": [{"kind": "task", "id": "tsk_ghost", "rel": "related"}],
            }),
            Some(cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create workspace: {ws}");
    ws["id"].as_str().expect("workspace id").to_string()
}

async fn count(store: &hive_api::store::Store, sql: &str, bind: &str) -> i64 {
    hive_api::pgq::query_scalar::<i64>(sql)
        .bind(bind)
        .fetch_one(store.db())
        .await
        .expect("count query")
}

#[tokio::test]
async fn delete_cascades_transcript_and_links_but_keeps_journal_mirrors() {
    let (app, store) = test_app().await;
    let admin = onboard(&app).await;
    let maggie = member(&app, &admin, "Maggie", "maggie@example.com").await;
    let ws = create_conversation(&app, &maggie, "sweep me").await;

    // Kickoff prompt landed: one transcript row, two conversation links
    // (project + linked task), and a journal mirror entry.
    let (status, msgs, _) = send(
        &app,
        get(&format!("/api/workspaces/{ws}/messages"), Some(&maggie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(msgs.as_array().map(Vec::len), Some(1), "kickoff: {msgs}");
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM links WHERE source_kind = 'conversation' AND source_id = ?",
            &ws,
        )
        .await,
        2
    );
    let mirror_pattern = format!("%[workspace:{ws}]%");
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM journal WHERE body LIKE ?",
            &mirror_pattern,
        )
        .await,
        1
    );

    let (status, body, _) = send(
        &app,
        delete_req(&format!("/api/workspaces/{ws}"), Some(&maggie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "owner delete: {body}");
    assert_eq!(body["ok"], true);

    let (status, _, _) = send(&app, get(&format!("/api/workspaces/{ws}"), Some(&maggie))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "session row gone");
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM cc_messages WHERE session_id = ?",
            &ws,
        )
        .await,
        0,
        "transcript cascades"
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM links WHERE source_kind = 'conversation' AND source_id = ?",
            &ws,
        )
        .await
            + count(
                &store,
                "SELECT COUNT(*) FROM links WHERE target_kind = 'conversation' AND target_id = ?",
                &ws,
            )
            .await,
        0,
        "conversation links cascade (both directions)"
    );
    assert_eq!(
        count(
            &store,
            "SELECT COUNT(*) FROM journal WHERE body LIKE ?",
            &mirror_pattern,
        )
        .await,
        1,
        "journal mirror is history and stays"
    );

    // Idempotence-ish: the row is gone now.
    let (status, _, _) = send(
        &app,
        delete_req(&format!("/api/workspaces/{ws}"), Some(&maggie)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "second delete is a 404");
}

#[tokio::test]
async fn delete_is_owner_or_admin_gated() {
    let (app, _store) = test_app().await;
    let admin = onboard(&app).await;
    let maggie = member(&app, &admin, "Maggie", "maggie@example.com").await;
    let bob = member(&app, &admin, "Bob", "bob@example.com").await;
    let ws = create_conversation(&app, &maggie, "not yours, bob").await;

    // A non-owner member cannot delete it.
    let (status, _, _) = send(
        &app,
        delete_req(&format!("/api/workspaces/{ws}"), Some(&bob)),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = send(&app, get(&format!("/api/workspaces/{ws}"), Some(&maggie))).await;
    assert_eq!(status, StatusCode::OK, "still there after forbidden delete");

    // An admin can.
    let (status, body, _) = send(
        &app,
        delete_req(&format!("/api/workspaces/{ws}"), Some(&admin)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "admin delete: {body}");
    let (status, _, _) = send(&app, get(&format!("/api/workspaces/{ws}"), Some(&maggie))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Unknown ids 404.
    let (status, _, _) = send(
        &app,
        delete_req("/api/workspaces/ccs_missing", Some(&admin)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
