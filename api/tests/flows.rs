// The flow registry over the full router, and the retired hosted-agent
// surface staying dark by default (docs/FLOWS.md D9). This binary must NOT
// set HIVE_AGENTS_ENABLED — the default-off behavior is the thing under test.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn test_app() -> (Router, hive_api::store::Store, hive_api::db::TestDb) {
    std::env::set_var("HIVE_EMBED", "hash");
    let test_db = hive_api::db::test_pool().await;
    let store = hive_api::store::Store::new(test_db.pool.clone());
    (hive_api::routes::router(store.clone()), store, test_db)
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
        serde_json::from_slice(&bytes)
            .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
    };
    (status, body)
}

fn get(path: &str, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::get(path);
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::empty()).unwrap()
}

fn json_req(method: &str, path: &str, body: Value, cookie: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(c) = cookie {
        b = b.header(header::COOKIE, c);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// Run onboarding, return the admin session cookie.
async fn onboard(app: &Router) -> String {
    let res = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/api/onboarding",
            json!({
                "instanceName": "Test Hive",
                "adminName": "Nate",
                "adminEmail": "nate@example.com",
                "password": "hunter22-strong"
            }),
            None,
        ))
        .await
        .expect("onboarding");
    assert_eq!(res.status(), StatusCode::CREATED);
    let set_cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap();
    set_cookie.split(';').next().unwrap().to_string()
}

/// Create a member and log them in; returns their cookie.
async fn member(app: &Router, admin_cookie: &str, name: &str, email: &str) -> String {
    let (status, body) = send(
        app,
        json_req(
            "POST",
            "/api/users",
            json!({"name": name, "email": email, "password": "member-secret-1"}),
            Some(admin_cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create member: {body}");
    let res = app
        .clone()
        .oneshot(json_req(
            "POST",
            "/api/auth/login",
            json!({"email": email, "password": "member-secret-1"}),
            None,
        ))
        .await
        .expect("login");
    assert_eq!(res.status(), StatusCode::OK);
    let set_cookie = res
        .headers()
        .get(header::SET_COOKIE)
        .expect("session cookie")
        .to_str()
        .unwrap();
    set_cookie.split(';').next().unwrap().to_string()
}

/// Registry REST: self-seeding list, detail, admin enable/disable, runs.
#[tokio::test]
async fn flow_registry_rest_surface() {
    let (app, _store, _test_db) = test_app().await;
    let admin = onboard(&app).await;
    let viewer = member(&app, &admin, "Maggie", "maggie@example.com").await;

    // The list self-seeds the builtin wire flow; members can read it.
    let (status, body) = send(&app, get("/api/flows", Some(&viewer))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let flows = body.as_array().expect("array");
    assert_eq!(flows.len(), 1);
    assert_eq!(flows[0]["slug"], "wire");
    assert_eq!(flows[0]["kind"], "builtin");
    assert_eq!(flows[0]["manifest"]["operations"][0]["name"], "recent");

    let (status, body) = send(&app, get("/api/flows/wire", Some(&viewer))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);

    // Enable/disable is admin: the member is refused, the admin flips it.
    let (status, _) = send(
        &app,
        json_req(
            "PATCH",
            "/api/flows/wire",
            json!({"enabled": false}),
            Some(&viewer),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = send(
        &app,
        json_req(
            "PATCH",
            "/api/flows/wire",
            json!({"enabled": false}),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);

    // Runs history: empty but well-formed for a fresh flow.
    let (status, body) = send(&app, get("/api/flows/wire/runs?limit=10", Some(&viewer))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));

    // Unknown flow is a plain 404.
    let (status, _) = send(&app, get("/api/flows/nope", Some(&viewer))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The hosted-agent surface is dark by default (docs/FLOWS.md D9): every
/// workspaces/conversations/credentials route answers the standard 404, the
/// SPA reads the gate from authConfig, and the flows surface is unaffected.
#[tokio::test]
async fn agent_surface_is_dark_by_default() {
    let (app, _store, _test_db) = test_app().await;
    let admin = onboard(&app).await;

    for path in [
        "/api/workspaces",
        "/api/workspaces/ws_x",
        "/api/cc-credentials",
        "/api/conversations/pending",
    ] {
        let (status, body) = send(&app, get(path, Some(&admin))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
        assert_eq!(body, json!({"error": "not found"}), "{path}");
    }
    let (status, body) = send(
        &app,
        json_req(
            "POST",
            "/api/workspaces",
            json!({"title": "should not spawn"}),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "spawn gated: {body}");
    let (status, body) = send(
        &app,
        json_req(
            "POST",
            "/api/conversations",
            json!({"external_id": "s1"}),
            Some(&admin),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "capture REST gated: {body}");

    // The SPA reads the gate from the public auth config.
    let (status, body) = send(&app, get("/api/auth/config", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["agentsEnabled"], false, "{body}");

    // The flows surface (the replacement integration story) still serves.
    let (status, body) = send(&app, get("/api/flows", Some(&admin))).await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
