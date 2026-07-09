// Conversation capture end-to-end: SessionEnd ingest onto cc_sessions
// (origin='captured'), replace-vs-append transcript semantics, the
// pending → reflected reflection loop, namespace scoping, the runner-claim
// invariant, the MCP capture tools, and the journal mail-scope guard
// (downgrade-not-refuse). Drives the full router (middleware included) over an
// isolated Postgres schema, parity_smoke.rs style.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn test_app() -> (Router, hive_api::store::Store) {
    // Hash embedder: deterministic + offline (latched once per process).
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

/// Create a member user and log them in; returns their session cookie.
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
    assert_eq!(status, StatusCode::CREATED, "user create: {body}");
    let (status, body, headers) = send(
        app,
        post_json(
            "/api/auth/login",
            json!({"email": email, "password": "member-secret-1"}),
            None,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login: {body}");
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

async fn capture(app: &Router, cookie: &str, external_id: &str, title: &str) -> String {
    let (status, body, _) = send(
        app,
        post_json(
            "/api/conversations",
            json!({"external_id": external_id, "title": title}),
            Some(cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "capture upsert: {body}");
    body["id"].as_str().expect("conversation id").to_string()
}

fn turn(role: &str, text: &str) -> Value {
    json!({"role": role, "content": {"text": text}})
}

#[tokio::test]
async fn capture_upsert_is_idempotent_by_runtime_and_external_id() {
    let (app, _store) = test_app().await;
    let cookie = onboard(&app).await;

    let id = capture(&app, &cookie, "sess-uuid-1", "local hack").await;

    // Same (runtime, external_id) → the same row (idempotent re-ingest).
    let (status, body, _) = send(
        &app,
        post_json(
            "/api/conversations",
            json!({"runtime": "claude_code", "external_id": "sess-uuid-1", "summary": "so far"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "re-upsert: {body}");
    assert_eq!(body["id"], id, "same capture key must upsert to one row");

    // A different runtime is a different capture key.
    let (_, body, _) = send(
        &app,
        post_json(
            "/api/conversations",
            json!({"runtime": "codex", "external_id": "sess-uuid-1"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_ne!(body["id"], id, "runtime is part of the capture key");

    // The re-upsert refreshed the summary but kept the original title.
    let (status, view, _) = send(
        &app,
        get(&format!("/api/conversations/{id}"), Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["title"], "local hack");
    assert_eq!(view["summary"], "so far");
    assert_eq!(view["origin"], "captured");

    // external_id is required.
    let (status, _, _) = send(
        &app,
        post_json(
            "/api/conversations",
            json!({"external_id": "  "}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn replace_swaps_the_full_transcript() {
    let (app, _store) = test_app().await;
    let cookie = onboard(&app).await;
    let id = capture(&app, &cookie, "sess-replace", "").await;

    // First SessionEnd: two turns.
    let (status, body, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/messages"),
            json!({"messages": [turn("user", "kick off"), turn("assistant", "on it")], "replace": true}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "messages: {body}");
    assert_eq!(body["appended"], 2);

    // The session resumes and SessionEnd re-fires with the FULL transcript.
    // replace=true must swap, not append — append-only would leave 6 turns.
    let (_, body, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/messages"),
            json!({"messages": [
                turn("user", "kick off"),
                turn("assistant", "on it"),
                turn("user", "and also this"),
                turn("assistant", "done")
            ], "replace": true}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(body["appended"], 4);

    let (status, view, _) = send(
        &app,
        get(&format!("/api/conversations/{id}"), Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let msgs = view["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 4, "replace must not duplicate turns: {view}");
    let seqs: Vec<i64> = msgs.iter().map(|m| m["seq"].as_i64().unwrap()).collect();
    assert_eq!(seqs, vec![1, 2, 3, 4], "seq restarts on replace");
    // Content is flattened to plain text for the reflector.
    assert_eq!(msgs[0]["content"], "kick off");
    assert_eq!(msgs[3]["content"], "done");

    // A plain append continues the seq instead of restarting.
    let (_, body, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/messages"),
            json!({"messages": [turn("user", "one more")]}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(body["appended"], 1);
    let (_, view, _) = send(
        &app,
        get(&format!("/api/conversations/{id}"), Some(&cookie)),
    )
    .await;
    let msgs = view["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 5);
    assert_eq!(msgs[4]["seq"], 5);
    assert_eq!(msgs[4]["content"], "one more");
}

#[tokio::test]
async fn runner_cannot_claim_captured_sessions() {
    let (app, _store) = test_app().await;
    let cookie = onboard(&app).await;

    // A hosted workspace is what the runner claims: status 'provisioning'.
    let (status, hosted, _) = send(
        &app,
        post_json("/api/workspaces", json!({"title": "hosted"}), Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(hosted["status"], "provisioning");

    let captured_id = capture(&app, &cookie, "sess-claim", "captured").await;

    // The runner polls /api/workspaces and drives every 'provisioning' row
    // (packages/runner tick()). A captured session must never appear there
    // as provisioning.
    let (status, list, _) = send(&app, get("/api/workspaces", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    let sessions = list.as_array().expect("workspace list");
    let captured = sessions
        .iter()
        .find(|s| s["id"] == captured_id.as_str())
        .expect("captured session is listed");
    assert_eq!(
        captured["status"], "captured",
        "captured sessions must not be claimable: {captured}"
    );
    assert!(
        !sessions
            .iter()
            .any(|s| s["id"] == captured_id.as_str() && s["status"] == "provisioning"),
        "the runner claim filter must never match a captured session"
    );
}

#[tokio::test]
async fn pending_reflected_flow_and_message_requeue() {
    let (app, _store) = test_app().await;
    let cookie = onboard(&app).await;
    let id = capture(&app, &cookie, "sess-reflect", "to reflect").await;
    let (_, _, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/messages"),
            json!({"messages": [turn("user", "remember this")], "replace": true}),
            Some(&cookie),
        ),
    )
    .await;

    // Freshly captured → queued for reflection.
    let (status, pending, _) = send(&app, get("/api/conversations/pending", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        pending.as_array().unwrap().iter().any(|c| c["id"] == id),
        "captured conversation starts pending: {pending}"
    );

    // Reflect: stamps the cursor + stores the rolling summary.
    let (status, reflected, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/reflected"),
            json!({"summary": "user asked hive to remember a thing"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "reflected: {reflected}");
    assert_eq!(reflected["summary"], "user asked hive to remember a thing");
    assert!(reflected["reflected_at"].is_string());
    let (_, pending, _) = send(&app, get("/api/conversations/pending", Some(&cookie))).await;
    assert!(
        !pending.as_array().unwrap().iter().any(|c| c["id"] == id),
        "reflected conversation leaves the queue"
    );

    // A later transcript write clears the cursor → re-queued for reflection.
    let (_, _, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/messages"),
            json!({"messages": [turn("user", "more happened")]}),
            Some(&cookie),
        ),
    )
    .await;
    let (_, pending, _) = send(&app, get("/api/conversations/pending", Some(&cookie))).await;
    assert!(
        pending.as_array().unwrap().iter().any(|c| c["id"] == id),
        "a transcript write re-queues reflection: {pending}"
    );
    // Reflecting without a summary keeps the previous rolling summary.
    let (_, reflected, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/reflected"),
            json!({}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(reflected["summary"], "user asked hive to remember a thing");
}

#[tokio::test]
async fn conversations_are_namespace_scoped() {
    let (app, _store) = test_app().await;
    let admin = onboard(&app).await;
    let maggie = member(&app, &admin, "Maggie", "maggie@example.com").await;
    let bob = member(&app, &admin, "Bob", "bob@example.com").await;

    let mid = capture(&app, &maggie, "sess-maggie", "maggie private").await;
    let (_, _, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{mid}/messages"),
            json!({"messages": [turn("user", "maggie only")], "replace": true}),
            Some(&maggie),
        ),
    )
    .await;

    // Owner sees her own conversation and queue.
    let (status, view, _) = send(
        &app,
        get(&format!("/api/conversations/{mid}"), Some(&maggie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["owner"], "maggie");
    let (_, pending, _) = send(&app, get("/api/conversations/pending", Some(&maggie))).await;
    assert!(pending.as_array().unwrap().iter().any(|c| c["id"] == mid));

    // Another member: hidden as 404 on get, absent from pending, forbidden on writes.
    let (status, _, _) = send(&app, get(&format!("/api/conversations/{mid}"), Some(&bob))).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "foreign get hides as 404");
    let (_, pending, _) = send(&app, get("/api/conversations/pending", Some(&bob))).await;
    assert!(
        !pending.as_array().unwrap().iter().any(|c| c["id"] == mid),
        "pending is namespace-scoped"
    );
    let (status, _, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{mid}/messages"),
            json!({"messages": [turn("user", "intrude")]}),
            Some(&bob),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "foreign message write");
    let (status, _, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{mid}/reflected"),
            json!({"summary": "hijack"}),
            Some(&bob),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "foreign reflect");

    // A capture key is not a bearer credential: re-upserting someone else's
    // (runtime, external_id) is forbidden, not a silent join.
    let (status, body, _) = send(
        &app,
        post_json(
            "/api/conversations",
            json!({"external_id": "sess-maggie", "title": "bob steals"}),
            Some(&bob),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "foreign capture key: {body}");

    // Admin sees all: get, pending, and reflect across namespaces.
    let (status, view, _) = send(
        &app,
        get(&format!("/api/conversations/{mid}"), Some(&admin)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["messages"][0]["content"], "maggie only");
    let (_, pending, _) = send(&app, get("/api/conversations/pending", Some(&admin))).await;
    assert!(
        pending.as_array().unwrap().iter().any(|c| c["id"] == mid),
        "admin pending spans namespaces"
    );
}

#[tokio::test]
async fn mcp_capture_tools_flow() {
    let (app, _store) = test_app().await;
    let cookie = onboard(&app).await;
    // The Hive plugin's shape: token actor = pia (AI), namespace = nate.
    let (status, minted, _) = send(
        &app,
        post_json(
            "/api/tokens",
            json!({"actor": "pia", "label": "plugin", "neverExpires": true}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let token = minted["token"].as_str().unwrap().to_string();

    let call = |name: &'static str, arguments: Value| {
        let app = app.clone();
        let token = token.clone();
        async move {
            let (status, body, _) = send(
                &app,
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
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "mcp {name}: {body}");
            let text = body["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("mcp {name} content: {body}"))
                .to_string();
            serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
        }
    };

    // conversation_log = upsert + transcript in one call.
    let logged = call(
        "conversation_log",
        json!({
            "external_id": "mcp-sess-1",
            "title": "pia local session",
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": {"text": "hi"}}
            ]
        }),
    )
    .await;
    let id = logged["id"].as_str().expect("id").to_string();
    assert_eq!(logged["appended"], 2);

    // Idempotent re-log with replace swaps the transcript on the same row.
    let relogged = call(
        "conversation_log",
        json!({
            "external_id": "mcp-sess-1",
            "replace": true,
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "bye"}
            ]
        }),
    )
    .await;
    assert_eq!(relogged["id"], id.as_str(), "same capture key, same row");
    assert_eq!(relogged["appended"], 3);

    // The transcript reads back flattened (bare-string content included).
    let view = call("conversation_get", json!({"id": id})).await;
    let msgs = view["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 3, "replace over MCP: {view}");
    assert_eq!(msgs[1]["content"], "hi");
    assert_eq!(view["owner"], "nate", "owner is the token's namespace");

    // pending → reflected drains the queue, viewer-gated to the namespace.
    let pending = call("conversation_list_pending", json!({})).await;
    assert!(pending.as_array().unwrap().iter().any(|c| c["id"] == id));
    let reflected = call(
        "conversation_mark_reflected",
        json!({"id": id, "summary": "pia said hello and left"}),
    )
    .await;
    assert_eq!(reflected["summary"], "pia said hello and left");
    assert!(reflected["reflected_at"].is_string());
    let pending = call("conversation_list_pending", json!({})).await;
    assert!(!pending.as_array().unwrap().iter().any(|c| c["id"] == id));
}

#[tokio::test]
async fn journal_guard_downgrades_ai_mail_summaries() {
    use hive_api::store::Store;
    use hive_shared::{ActorKind, NewJournalEntry};

    std::env::set_var("HIVE_EMBED", "hash");
    let pool = hive_api::db::test_pool().await;
    let store = Store::new(pool);

    store
        .people_upsert("nate", "Nate", ActorKind::Human, None)
        .await
        .unwrap();
    store
        .people_upsert("owned-ai", "Owned AI", ActorKind::Ai, Some("nate"))
        .await
        .unwrap();
    store
        .people_upsert("orphan-ai", "Orphan AI", ActorKind::Ai, None)
        .await
        .unwrap();

    let entry = |body: &str| NewJournalEntry {
        author: None,
        body: body.to_string(),
        tags: None,
        anchors: None,
    };
    let mail_body = "Summarized the thread in [mail:msg_abc123]: invoice due Friday.";

    // AI-authored GLOBAL entry citing mail → downgraded to the owner's scope,
    // visibly tagged (downgrade-not-refuse; never a silent rewrite).
    let v = store
        .journal_append(entry(mail_body), Some("owned-ai"), None)
        .await
        .unwrap();
    assert_eq!(
        v.entry.user_scope.as_deref(),
        Some("nate"),
        "mail-derived AI memory lands owner-scoped"
    );
    assert!(
        v.entry.tags.iter().any(|t| t == "scoped-by-policy"),
        "the downgrade is tagged, not silent: {:?}",
        v.entry.tags
    );

    // The same entry by a human stays global and untagged.
    let v = store
        .journal_append(entry(mail_body), Some("nate"), None)
        .await
        .unwrap();
    assert_eq!(v.entry.user_scope, None, "human global mail cite passes");
    assert!(!v.entry.tags.iter().any(|t| t == "scoped-by-policy"));

    // An AI with no owner has no scope to land in → rejected.
    let err = store
        .journal_append(entry(mail_body), Some("orphan-ai"), None)
        .await
        .expect_err("unowned AI mail cite must be refused");
    assert!(
        err.to_string().contains("owner scope"),
        "unexpected error: {err}"
    );

    // Guard only fires on GLOBAL mail cites: an already-scoped AI write keeps
    // its scope untagged, and a global AI write without [mail: stays global.
    let v = store
        .journal_append(entry(mail_body), Some("owned-ai"), Some("nate"))
        .await
        .unwrap();
    assert_eq!(v.entry.user_scope.as_deref(), Some("nate"));
    assert!(!v.entry.tags.iter().any(|t| t == "scoped-by-policy"));
    let v = store
        .journal_append(entry("Plain global note, no mail."), Some("owned-ai"), None)
        .await
        .unwrap();
    assert_eq!(v.entry.user_scope, None);
    assert!(!v.entry.tags.iter().any(|t| t == "scoped-by-policy"));
}
