// End-to-end parity smoke: drives the full router (middleware included) over
// a temp database the way a browser + CLI would — onboarding, sessions, journal
// emergence, ACL, search, tokens, admin lifecycle, import idempotency.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn test_app() -> (Router, ()) {
    // Hash embedder: deterministic + offline (set before any embed call; the
    // provider choice is latched once per process).
    std::env::set_var("HIVE_EMBED", "hash");
    // Isolated Postgres schema per test (uses DATABASE_URL / local dev default).
    let pool = hive_api::db::test_pool().await;
    let store = hive_api::store::Store::new(pool);
    (hive_api::routes::router(store), ())
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
    let (app, _dir) = test_app().await;

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
async fn viewer_acl_scopes_journal() {
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
async fn conversations_upsert_append_scope_and_reflection() {
    use hive_api::middleware::Visibility;
    use hive_api::store::conversations::ConversationUpsert;
    use hive_api::store::Store;
    use hive_shared::NewConversationMessage;

    std::env::set_var("HIVE_EMBED", "hash");
    let pool = hive_api::db::test_pool().await;
    let store = Store::new(pool);

    let upsert = |actor: &str, ext: &str| ConversationUpsert {
        app: "claude-code".into(),
        instance: Some("roadhouse".into()),
        name: Some("kickoff".into()),
        actor: actor.into(),
        external_id: Some(ext.into()),
    };

    // Upsert idempotency by (app, external_id): same key returns the same id.
    let id1 = store
        .conversations_upsert(upsert("pia", "sess-1"), Some("nate"))
        .await
        .unwrap();
    let id2 = store
        .conversations_upsert(upsert("pia", "sess-1"), Some("nate"))
        .await
        .unwrap();
    assert_eq!(id1, id2, "same (app, external_id) must upsert to one row");

    // Message append ordering: seq continues monotonically across calls.
    let appended = store
        .conversation_append_messages(
            &id1,
            &[
                NewConversationMessage {
                    role: "user".into(),
                    content: "hello".into(),
                },
                NewConversationMessage {
                    role: "assistant".into(),
                    content: "hi".into(),
                },
            ],
        )
        .await
        .unwrap();
    assert_eq!(appended, 2);
    store
        .conversation_append_messages(
            &id1,
            &[NewConversationMessage {
                role: "user".into(),
                content: "again".into(),
            }],
        )
        .await
        .unwrap();
    let view = store
        .conversation_get(&id1, &Visibility::All)
        .await
        .unwrap()
        .expect("conversation present");
    let seqs: Vec<i64> = view.messages.iter().map(|m| m.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2], "seq is monotonic across append calls");
    assert_eq!(view.messages[2].content, "again");
    assert!(view.conversation.last_message_at.is_some());

    // A second conversation owned by maggie's namespace, plus a global one.
    let id_maggie = store
        .conversations_upsert(upsert("apis", "sess-2"), Some("maggie"))
        .await
        .unwrap();
    let id_global = store
        .conversations_upsert(upsert("system", "sess-3"), None)
        .await
        .unwrap();

    // Namespace scoping: nate sees his own + global, not maggie's.
    let nate_vis = Visibility::Namespace("nate".into());
    let nate_list = store.conversations_list(&nate_vis, 50, 0).await.unwrap();
    let nate_ids: Vec<&str> = nate_list.iter().map(|c| c.id.as_str()).collect();
    assert!(nate_ids.contains(&id1.as_str()), "nate sees own");
    assert!(nate_ids.contains(&id_global.as_str()), "nate sees global");
    assert!(
        !nate_ids.contains(&id_maggie.as_str()),
        "nate must not see maggie's namespace"
    );
    // get() hides maggie's conversation from nate as a 404 (None).
    assert!(store
        .conversation_get(&id_maggie, &nate_vis)
        .await
        .unwrap()
        .is_none());
    // Admin (All) sees every conversation.
    let all = store
        .conversations_list(&Visibility::All, 50, 0)
        .await
        .unwrap();
    assert_eq!(all.len(), 3, "admin sees all three");

    // pending → reflected transition, namespace-scoped.
    let pending = store.conversations_pending(&nate_vis, 50).await.unwrap();
    let pending_ids: Vec<&str> = pending.iter().map(|c| c.id.as_str()).collect();
    assert!(pending_ids.contains(&id1.as_str()), "id1 starts pending");
    assert!(
        !pending_ids.contains(&id_maggie.as_str()),
        "pending is namespace-scoped"
    );
    let reflected = store
        .conversation_mark_reflected(&id1, "rolling summary")
        .await
        .unwrap()
        .expect("marked");
    assert!(reflected.reflected_at.is_some());
    assert_eq!(reflected.summary, "rolling summary");
    let pending_after = store.conversations_pending(&nate_vis, 50).await.unwrap();
    assert!(
        !pending_after.iter().any(|c| c.id == id1),
        "reflected conversation leaves the pending queue"
    );

    // Rename updates the friendly name.
    let renamed = store
        .conversation_rename(&id1, "renamed session")
        .await
        .unwrap()
        .expect("renamed");
    assert_eq!(renamed.name, "renamed session");
}

#[tokio::test]
async fn conversations_route_flow() {
    let (app, _dir) = test_app().await;
    let cookie = onboard(&app).await;

    // Upsert via the REST route returns an id.
    let (status, body, _) = send(
        &app,
        post_json(
            "/api/conversations",
            json!({"app": "claude-code", "external_id": "route-sess-1", "name": "rust work"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "upsert: {body}");
    let id = body["id"].as_str().expect("conversation id").to_string();

    // Idempotent re-upsert hits the same row.
    let (_, body2, _) = send(
        &app,
        post_json(
            "/api/conversations",
            json!({"app": "claude-code", "external_id": "route-sess-1"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(body2["id"], id, "same external_id is idempotent");

    // Append turns.
    let (status, appended, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/messages"),
            json!({"messages": [
                {"role": "user", "content": "kick it off"},
                {"role": "assistant", "content": "on it"}
            ]}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "append: {appended}");
    assert_eq!(appended["appended"], 2);

    // Transcript get returns the conversation + ordered messages.
    let (status, view, _) = send(
        &app,
        get(&format!("/api/conversations/{id}"), Some(&cookie)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(view["messages"].as_array().map(Vec::len), Some(2));
    assert_eq!(view["messages"][0]["seq"], 0);

    // pending lists it (admin sees all).
    let (status, pending, _) = send(&app, get("/api/conversations/pending", Some(&cookie))).await;
    assert_eq!(status, StatusCode::OK);
    assert!(pending
        .as_array()
        .map(|a| a.iter().any(|c| c["id"] == id))
        .unwrap_or(false));

    // Mark reflected, then it drops out of pending.
    let (status, reflected, _) = send(
        &app,
        post_json(
            &format!("/api/conversations/{id}/reflected"),
            json!({"summary": "did the rust work"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(reflected["summary"], "did the rust work");
    assert!(reflected["reflected_at"].is_string());
    let (_, pending2, _) = send(&app, get("/api/conversations/pending", Some(&cookie))).await;
    assert!(!pending2
        .as_array()
        .map(|a| a.iter().any(|c| c["id"] == id))
        .unwrap_or(false));

    // Rename via PATCH.
    let (status, renamed, _) = send(
        &app,
        patch_json(
            &format!("/api/conversations/{id}"),
            json!({"name": "renamed"}),
            Some(&cookie),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renamed["name"], "renamed");
}

#[tokio::test]
async fn spa_paths_are_not_gated() {
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
