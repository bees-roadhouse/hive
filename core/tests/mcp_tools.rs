// The MCP tool layer over hive-core (core/src/mcp.rs) — transport-free:
// tools/list is a plain Value, tools/call dispatch runs against a Store and a
// LocalCtx. Ports the surviving MCP assertions from the retired api tests
// (custom_entities_over_mcp, the journal author pin, the artifacts sync
// shape); the auth/gating halves died with their subjects in PR 1.3.

mod common;

use std::sync::OnceLock;

use hive_core::mcp::{self, LocalCtx};
use hive_core::store::Store;
use serde_json::{json, Map, Value};

fn hash_setup() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| std::env::set_var("HIVE_EMBED", "hash"));
    assert_eq!(hive_embed::embed_dim(), 256, "hash provider must be active");
}

async fn test_store() -> Store {
    hash_setup();
    common::test_store().await
}

fn ctx(actor: &str) -> LocalCtx {
    LocalCtx {
        actor: actor.to_string(),
    }
}

fn args(v: Value) -> Map<String, Value> {
    v.as_object().expect("args object").clone()
}

/// Parse the JSON a successful tool call rendered into its content block.
fn content_json(result: &Value) -> Value {
    assert!(result.get("isError").is_none(), "tool errored: {result}");
    serde_json::from_str(result["content"][0]["text"].as_str().expect("text block"))
        .expect("content json")
}

#[test]
fn tools_list_matches_the_teardown() {
    let tools = mcp::tools_list().as_array().expect("tool array");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().expect("tool name"))
        .collect();

    // Every kept tool is present.
    for want in [
        "journal_append",
        "journal_list",
        "journal_get",
        "identity_update",
        "tasks_list",
        "task_set_status",
        "decisions_list",
        "events_list",
        "inbox_list",
        "inbox_mark_read",
        "search",
        "mail_search",
        "mail_thread_get",
        "mail_accounts_list",
        "dashboard",
        "embeddings_status",
        "semantic_search",
        "profile_get",
        "profile_update",
        "recall",
        "sources_list",
        "sources_add",
        "sources_update",
        "sources_remove",
        "outbox_list",
        "worker_status",
        "people_list",
        "topics_list",
        "projects_list",
        "phases_list",
        "actor_delete",
        "actor_merge",
        "identity_link",
        "identity_resolve",
        "identity_list",
        "identity_unlink",
        "entity_types_list",
        "entity_type_create",
        "entity_type_update",
        "entities_list",
        "entity_get",
        "entity_create",
        "entity_update",
        "entity_delete",
        "artifacts_list",
        "artifacts_get",
        "identity_artifacts_sync",
    ] {
        assert!(names.contains(&want), "missing tool {want}");
    }

    // The hosted-era surface is gone: workspaces, shares, conversations,
    // token management, OAuth.
    for gone in [
        "workspace_list",
        "workspace_get",
        "workspace_transcript",
        "share_entry",
        "conversation_log",
        "conversation_list_pending",
        "conversation_get",
        "conversation_mark_reflected",
    ] {
        assert!(!names.contains(&gone), "tool {gone} must be deleted");
    }

    // Schema stability: every tool still carries an object inputSchema, and
    // kept schemas keep their required keys.
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
    }
    let journal_append = tools
        .iter()
        .find(|t| t["name"] == "journal_append")
        .unwrap();
    assert_eq!(journal_append["inputSchema"]["required"], json!(["body"]));
    let recall = tools.iter().find(|t| t["name"] == "recall").unwrap();
    assert_eq!(recall["inputSchema"]["required"], json!(["identity"]));
}

#[tokio::test]
async fn journal_append_pins_authorship_to_the_ctx_actor() {
    let store = test_store().await;

    // A supplied `author` argument is overridden by the acting identity, and
    // the write stamps the actor as the namespace owner.
    let result = mcp::call_tool(
        &store,
        &ctx("pia"),
        "journal_append",
        &args(json!({"body": "Pia notes the [topic: garden] plan.", "author": "mallory"})),
    )
    .await;
    let entry = content_json(&result);
    assert_eq!(entry["author"], "pia", "authorship pinned to ctx.actor");
    assert_eq!(
        entry["user_scope"], "pia",
        "the acting identity is the stamped owner"
    );

    // journal_list / journal_get read it back unscoped.
    let listed =
        content_json(&mcp::call_tool(&store, &ctx("pia"), "journal_list", &args(json!({}))).await);
    assert_eq!(listed.as_array().map(Vec::len), Some(1));
    let got = content_json(
        &mcp::call_tool(
            &store,
            &ctx("pia"),
            "journal_get",
            &args(json!({"id": entry["id"]})),
        )
        .await,
    );
    assert_eq!(got["id"], entry["id"]);
}

#[tokio::test]
async fn custom_entities_over_mcp() {
    let store = test_store().await;

    // Define a type (no admin gate — single user has full access).
    let created = mcp::call_tool(
        &store,
        &ctx("nate"),
        "entity_type_create",
        &args(json!({"name": "Plant", "fields": [
            {"label": "Species", "field_type": "text"},
            {"label": "Watered", "field_type": "date"}
        ]})),
    )
    .await;
    let view = content_json(&created);
    assert_eq!(view["slug"], "plant");

    let inst = mcp::call_tool(
        &store,
        &ctx("pia"),
        "entity_create",
        &args(json!({"type": "plant", "title": "Monstera", "fields": {"species": "M. deliciosa", "watered": "2026-07-01"}})),
    )
    .await;
    let monstera = content_json(&inst);
    assert_eq!(monstera["type"], "plant");

    // Validation failures render the structured issue list as tool errors.
    let bad = mcp::call_tool(
        &store,
        &ctx("pia"),
        "entity_create",
        &args(json!({"type": "plant", "title": "X", "fields": {"watered": "yesterday"}})),
    )
    .await;
    assert_eq!(bad["isError"], true);
    assert!(bad["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("bad_date"));

    let listed = content_json(
        &mcp::call_tool(
            &store,
            &ctx("pia"),
            "entities_list",
            &args(json!({"type": "plant"})),
        )
        .await,
    );
    assert_eq!(listed.as_array().unwrap().len(), 1);

    let updated = content_json(
        &mcp::call_tool(
            &store,
            &ctx("pia"),
            "entity_update",
            &args(json!({"id": monstera["id"], "fields": {"watered": null}})),
        )
        .await,
    );
    assert!(updated["fields"].get("watered").is_none());

    let deleted = content_json(
        &mcp::call_tool(
            &store,
            &ctx("pia"),
            "entity_delete",
            &args(json!({"id": monstera["id"]})),
        )
        .await,
    );
    assert_eq!(deleted["deleted"], true);
}

#[tokio::test]
async fn zod_style_validation_and_unknown_tools() {
    let store = test_store().await;

    // Missing required arg renders the SDK's -32602 shape.
    let missing = mcp::call_tool(&store, &ctx("nate"), "journal_get", &Map::new()).await;
    assert_eq!(missing["isError"], true);
    let text = missing["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("MCP error -32602: Input validation error"),
        "{text}"
    );
    assert!(text.contains("\"code\": \"invalid_type\""), "{text}");

    // Unknown tool is the SDK's not-found error.
    let unknown = mcp::call_tool(&store, &ctx("nate"), "workspace_list", &Map::new()).await;
    assert_eq!(unknown["isError"], true);
    assert!(unknown["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("MCP error -32602: Tool workspace_list not found"));
}

#[tokio::test]
async fn inbox_dashboard_and_status_tools_run_unscoped() {
    let store = test_store().await;

    // Seed one notification via a mention.
    mcp::call_tool(
        &store,
        &ctx("nate"),
        "journal_append",
        &args(json!({"body": "ping @pia about the smoker"})),
    )
    .await;

    let inbox = content_json(
        &mcp::call_tool(
            &store,
            &ctx("nate"),
            "inbox_list",
            &args(json!({"recipient": "pia"})),
        )
        .await,
    );
    let items = inbox.as_array().unwrap();
    assert_eq!(items.len(), 1, "any actor's inbox is readable: {inbox}");

    let marked = content_json(
        &mcp::call_tool(
            &store,
            &ctx("nate"),
            "inbox_mark_read",
            &args(json!({"id": items[0]["id"]})),
        )
        .await,
    );
    assert_eq!(marked["marked"], true);
    // A missing id answers the same soft shape.
    let missing = content_json(
        &mcp::call_tool(
            &store,
            &ctx("nate"),
            "inbox_mark_read",
            &args(json!({"id": "inb_missing"})),
        )
        .await,
    );
    assert_eq!(missing["marked"], false);

    // dashboard + embeddings_status answer without an admin gate.
    let dash = content_json(&mcp::call_tool(&store, &ctx("pia"), "dashboard", &Map::new()).await);
    assert_eq!(dash["entries"], 1);
    let status =
        content_json(&mcp::call_tool(&store, &ctx("pia"), "embeddings_status", &Map::new()).await);
    assert!(status["embeddable"].as_i64().unwrap() >= 1, "{status}");

    // recall takes an explicit actor identity, no ownership gate.
    let recall = content_json(
        &mcp::call_tool(
            &store,
            &ctx("nate"),
            "recall",
            &args(json!({"identity": "pia", "query": "smoker"})),
        )
        .await,
    );
    assert!(recall["brief"].as_str().unwrap().contains("Recall for pia"));
}

#[tokio::test]
async fn artifacts_tools_and_identity_sync() {
    let store = test_store().await;
    store
        .artifacts_upsert("pia", "skill", "journal", "body-v1", "j", true)
        .await
        .unwrap();
    store
        .artifacts_upsert("pia", "agent", "scout", "body", "s", false)
        .await
        .unwrap();
    store
        .artifacts_upsert("apis", "skill", "elsewhere", "body", "", true)
        .await
        .unwrap();

    // artifacts_list: ALL of the acting identity's artifacts (incl. disabled).
    let listed =
        content_json(&mcp::call_tool(&store, &ctx("pia"), "artifacts_list", &Map::new()).await);
    assert_eq!(listed["count"], 2);

    // identity_artifacts_sync: ENABLED only; defaults to the acting identity,
    // explicit actor override supported.
    let synced = content_json(
        &mcp::call_tool(&store, &ctx("pia"), "identity_artifacts_sync", &Map::new()).await,
    );
    assert_eq!(synced["count"], 1, "{synced}");
    assert_eq!(synced["artifacts"][0]["name"], "journal");
    let other = content_json(
        &mcp::call_tool(
            &store,
            &ctx("pia"),
            "identity_artifacts_sync",
            &args(json!({"actor": "apis"})),
        )
        .await,
    );
    assert_eq!(other["count"], 1);
    assert_eq!(other["artifacts"][0]["name"], "elsewhere");

    // artifacts_get by id (no identity gate; missing ids answer not found).
    let id = listed["artifacts"][0]["id"].as_str().unwrap();
    let got = content_json(
        &mcp::call_tool(
            &store,
            &ctx("apis"),
            "artifacts_get",
            &args(json!({"id": id})),
        )
        .await,
    );
    assert_eq!(got["id"], id);
    let missing = content_json(
        &mcp::call_tool(
            &store,
            &ctx("pia"),
            "artifacts_get",
            &args(json!({"id": "art_missing"})),
        )
        .await,
    );
    assert_eq!(missing["error"], "not found");
}

// ---- the JSON-RPC frame layer (moved into core by the PR 2.4 proxy flip) ----

#[tokio::test]
async fn frame_layer_speaks_mcp_json_rpc() {
    let store = test_store().await;
    let ctx = ctx("nate");
    let frame = |line: &'static str| mcp::handle_frame(&store, &ctx, line);

    // initialize: a supported requested version echoes; an unknown one is
    // countered with the latest we speak.
    let reply = frame(
        r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
    )
    .await
    .expect("requests get replies");
    assert_eq!(reply["id"], json!(0));
    assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(reply["result"]["serverInfo"]["name"], "hive");
    assert!(reply["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("journal-first"));
    let reply = frame(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1999-01-01"}}"#,
    )
    .await
    .unwrap();
    assert_eq!(
        reply["result"]["protocolVersion"],
        json!(mcp::LATEST_PROTOCOL_VERSION)
    );

    // Notifications and response frames produce no reply; ping answers {}.
    assert!(
        frame(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await
            .is_none()
    );
    assert!(frame(r#"{"jsonrpc":"2.0","id":9,"result":{}}"#)
        .await
        .is_none());
    let reply = frame(r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#)
        .await
        .unwrap();
    assert_eq!(reply["result"], json!({}));

    // tools/call dispatches into the tool layer with the ctx actor pinned;
    // a missing tool name is a transport-level -32602.
    let reply = frame(
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"journal_append","arguments":{"body":"Framed entry."}}}"#,
    )
    .await
    .unwrap();
    let entry = content_json(&reply["result"]);
    assert_eq!(entry["author"], "nate");
    let reply = frame(r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{}}"#)
        .await
        .unwrap();
    assert_eq!(reply["error"]["code"], json!(-32602));

    // Transport errors: garbage → -32700 null id; a non-object → -32600;
    // an unknown method → -32601 (we declare only tools).
    let reply = frame("not json").await.unwrap();
    assert_eq!(reply["error"]["code"], json!(-32700));
    assert!(reply["id"].is_null());
    let reply = frame("42").await.unwrap();
    assert_eq!(reply["error"]["code"], json!(-32600));
    let reply = frame(r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#)
        .await
        .unwrap();
    assert_eq!(reply["error"]["code"], json!(-32601));
}
