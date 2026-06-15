// POST /mcp — the stateless MCP endpoint (parity with server.ts's raw /mcp
// handler + the SDK's StreamableHTTPServerTransport, stateless mode).
//
// Auth shapes its own 401 (www-authenticate per RFC 9728) exactly like the
// Node raw server; the middleware attaches AuthCtx but does not gate /mcp.
// Transport gates (Accept/Content-Type/protocol-version), parse errors, the
// notification 202, and the JSON-RPC error codes/messages mirror the SDK
// (@modelcontextprotocol/sdk 1.29.0). One deliberate deviation: responses to
// requests are plain application/json (the spec's JSON response mode) rather
// than the SDK's SSE-framed stream — MCP clients must accept both.

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::any;
use axum::{Extension, Router};
use serde_json::{json, Map, Value};

use crate::error::ApiResult;
use crate::mcp;
use crate::middleware::{issuer_for, AuthCtx};
use crate::store::Store;

pub fn router() -> Router<Store> {
    Router::new().route("/mcp", any(entry))
}

/// `{"jsonrpc":"2.0","error":{...},"id":null}` with an HTTP status — the
/// transport's createJsonErrorResponse.
fn http_rpc_error(status: StatusCode, code: i64, message: &str) -> Response {
    (
        status,
        Json(json!({
            "jsonrpc": "2.0",
            "error": {"code": code, "message": message},
            "id": null
        })),
    )
        .into_response()
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// A JSON-RPC request id (string | number). Anything else means the message
/// is a notification (or a response) and gets no reply.
fn is_request_id(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::String(_)) | Some(Value::Number(_)))
}

fn is_request(m: &Value) -> bool {
    m.get("method").is_some_and(Value::is_string) && is_request_id(m.get("id"))
}

/// JSONRPCMessageSchema (lenient): jsonrpc "2.0" + a method (request or
/// notification), or an id'd result/error (a response — accepted, ignored).
fn is_valid_message(m: &Value) -> bool {
    let Some(obj) = m.as_object() else {
        return false;
    };
    if obj.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return false;
    }
    if obj.get("method").is_some_and(Value::is_string) {
        return true;
    }
    (obj.contains_key("result") || obj.contains_key("error")) && is_request_id(obj.get("id"))
}

async fn entry(
    State(store): State<Store>,
    Extension(ctx): Extension<AuthCtx>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult {
    // Auth first, like server.ts (it gates every non-OPTIONS method before
    // handleMcp; OPTIONS is short-circuited by the middleware): 401 + RFC 9728
    // resource-metadata pointer.
    if store.onboarding_required().await? || ctx.actor.is_none() {
        let issuer = issuer_for(&store, &headers).await;
        let mut res = http_rpc_error(
            StatusCode::UNAUTHORIZED,
            -32001,
            "Unauthorized — authorize via OAuth or provide a Bearer API token.",
        );
        if let Ok(v) = HeaderValue::from_str(&format!(
            "Bearer resource_metadata=\"{issuer}/.well-known/oauth-protected-resource\""
        )) {
            res.headers_mut().insert("www-authenticate", v);
        }
        res.headers_mut()
            .insert("access-control-allow-origin", HeaderValue::from_static("*"));
        return Ok(res);
    }

    // Authenticated non-POST gets Node's 405 (handleMcp).
    if method != Method::POST {
        let mut res = http_rpc_error(
            StatusCode::METHOD_NOT_ALLOWED,
            -32000,
            "Use POST for the stateless MCP endpoint.",
        );
        res.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return Ok(res);
    }

    // Transport gates (webStandardStreamableHttp.handlePostRequest order).
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return Ok(http_rpc_error(
            StatusCode::NOT_ACCEPTABLE,
            -32000,
            "Not Acceptable: Client must accept both application/json and text/event-stream",
        ));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.contains("application/json") {
        return Ok(http_rpc_error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            -32000,
            "Unsupported Media Type: Content-Type must be application/json",
        ));
    }

    let raw: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return Ok(http_rpc_error(
                StatusCode::BAD_REQUEST,
                -32700,
                "Parse error: Invalid JSON",
            ))
        }
    };
    let messages: Vec<Value> = match raw {
        Value::Array(a) => a,
        v => vec![v],
    };
    if !messages.iter().all(is_valid_message) {
        return Ok(http_rpc_error(
            StatusCode::BAD_REQUEST,
            -32700,
            "Parse error: Invalid JSON-RPC message",
        ));
    }

    let has_init = messages
        .iter()
        .any(|m| is_request(m) && m["method"] == "initialize");
    if has_init && messages.len() > 1 {
        return Ok(http_rpc_error(
            StatusCode::BAD_REQUEST,
            -32600,
            "Invalid Request: Only one initialization request is allowed",
        ));
    }
    if !has_init {
        if let Some(v) = headers
            .get("mcp-protocol-version")
            .and_then(|h| h.to_str().ok())
        {
            if !mcp::SUPPORTED_PROTOCOL_VERSIONS.contains(&v) {
                let msg = format!(
                    "Bad Request: Unsupported protocol version: {v} (supported versions: {})",
                    mcp::SUPPORTED_PROTOCOL_VERSIONS.join(", ")
                );
                return Ok(http_rpc_error(StatusCode::BAD_REQUEST, -32000, &msg));
            }
        }
    }

    // Notifications/responses only → 202 Accepted, empty body.
    if !messages.iter().any(is_request) {
        return Ok(StatusCode::ACCEPTED.into_response());
    }

    let mut responses: Vec<Value> = Vec::new();
    for m in messages.iter().filter(|m| is_request(m)) {
        responses.push(handle_request(&store, &ctx, m).await);
    }
    let out = if responses.len() == 1 {
        responses.pop().unwrap_or(Value::Null)
    } else {
        Value::Array(responses)
    };
    Ok(Json(out).into_response())
}

async fn handle_request(store: &Store, ctx: &AuthCtx, msg: &Value) -> Value {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params");

    match method {
        "initialize" => initialize(&id, params),
        "ping" => rpc_result(&id, json!({})),
        "tools/list" => rpc_result(&id, json!({"tools": mcp::tools_list().clone()})),
        "tools/call" => {
            let Some(p) = params.and_then(Value::as_object) else {
                return rpc_error(&id, -32603, &v4_issue("object", &["params"], params));
            };
            let Some(name) = p.get("name").and_then(Value::as_str) else {
                return rpc_error(
                    &id,
                    -32603,
                    &v4_issue("string", &["params", "name"], p.get("name")),
                );
            };
            let arguments: Map<String, Value> = match p.get("arguments") {
                None | Some(Value::Null) => Map::new(),
                Some(Value::Object(m)) => m.clone(),
                other => {
                    return rpc_error(
                        &id,
                        -32603,
                        &v4_issue("object", &["params", "arguments"], other),
                    )
                }
            };
            rpc_result(&id, mcp::call_tool(store, ctx, name, &arguments).await)
        }
        // Notifications never reach here (no id ⇒ no response); a request
        // using a notification method has no request handler in Node either.
        _ => rpc_error(&id, -32601, "Method not found"),
    }
}

fn initialize(id: &Value, params: Option<&Value>) -> Value {
    let Some(p) = params.and_then(Value::as_object) else {
        return rpc_error(id, -32603, &v4_issue("object", &["params"], params));
    };
    let mut issues: Vec<String> = Vec::new();
    let requested = p.get("protocolVersion").and_then(Value::as_str);
    if requested.is_none() {
        issues.push(v4_issue_inner(
            "string",
            &["params", "protocolVersion"],
            p.get("protocolVersion"),
        ));
    }
    if !p.get("capabilities").is_some_and(Value::is_object) {
        issues.push(v4_issue_inner(
            "object",
            &["params", "capabilities"],
            p.get("capabilities"),
        ));
    }
    if !p.get("clientInfo").is_some_and(Value::is_object) {
        issues.push(v4_issue_inner(
            "object",
            &["params", "clientInfo"],
            p.get("clientInfo"),
        ));
    }
    if !issues.is_empty() {
        return rpc_error(id, -32603, &format!("[\n{}\n]", issues.join(",\n")));
    }

    let requested = requested.unwrap_or_default();
    let negotiated = if mcp::SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        mcp::LATEST_PROTOCOL_VERSION
    };
    rpc_result(
        id,
        json!({
            "protocolVersion": negotiated,
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": mcp::SERVER_NAME, "version": mcp::SERVER_VERSION},
            "instructions": mcp::instructions(),
        }),
    )
}

// The protocol layer parses requests with zod v4 — its ZodError message is a
// pretty-printed issue array (measured against the Node server). These mirror
// that text for the missing/invalid-params cases.

fn v4_received(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

fn v4_issue_inner(expected: &str, path: &[&str], received: Option<&Value>) -> String {
    let segs = path
        .iter()
        .map(|p| format!("      \"{p}\""))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        "  {{\n    \"expected\": \"{expected}\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n{segs}\n    ],\n    \"message\": \"Invalid input: expected {expected}, received {received}\"\n  }}",
        received = v4_received(received),
    )
}

fn v4_issue(expected: &str, path: &[&str], received: Option<&Value>) -> String {
    format!("[\n{}\n]", v4_issue_inner(expected, path, received))
}

// Wire-shape tests pinned against the Node server's actual responses
// (@modelcontextprotocol/sdk 1.29.0, captured over an in-memory transport).
#[cfg(test)]
mod tests {
    use super::*;

    async fn test_store() -> (Store, ()) {
        std::env::set_var("HIVE_EMBED", "hash");
        let pool = crate::db::test_pool().await;
        let store = Store::new(pool);
        store
            .onboarding_complete("test", "nate", "nate@example.com", "Password123!")
            .await
            .expect("onboarding");
        (store, ())
    }

    fn req(method: &str, params: Value, id: i64) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    async fn post(store: &Store, ctx: AuthCtx, body: Value) -> Response {
        post_raw(store, ctx, body.to_string()).await
    }

    async fn post_raw(store: &Store, ctx: AuthCtx, body: String) -> Response {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        entry(
            State(store.clone()),
            Extension(ctx),
            Method::POST,
            headers,
            Bytes::from(body),
        )
        .await
        .map_err(|_| "handler error")
        .unwrap()
    }

    fn authed() -> AuthCtx {
        AuthCtx {
            actor: Some("nate".to_string()),
            principal: Some("token"),
            role: Some(hive_shared::UserRole::Admin),
            namespace_user: Some("nate".to_string()),
            session_cookie: None,
        }
    }

    /// A non-admin token principal acting as `actor` (its own namespace).
    fn ctx_for(actor: &str) -> AuthCtx {
        AuthCtx {
            actor: Some(actor.to_string()),
            principal: Some("token"),
            role: None,
            namespace_user: Some(actor.to_string()),
            session_cookie: None,
        }
    }

    async fn body_json(res: Response) -> Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    }

    fn content_text(result: &Value) -> Value {
        let text = result["content"][0]["text"].as_str().expect("text block");
        assert_eq!(result["content"][0]["type"], "text");
        serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
    }

    #[tokio::test]
    async fn initialize_negotiates_protocol_version() {
        let (store, _dir) = test_store().await;
        let res = handle_request(
            &store,
            &authed(),
            &req(
                "initialize",
                json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
                1,
            ),
        )
        .await;
        assert_eq!(res["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(
            res["result"]["capabilities"],
            json!({"tools": {"listChanged": true}})
        );
        assert_eq!(
            res["result"]["serverInfo"],
            json!({"name": "hive", "version": "0.1.0"})
        );
        assert!(res["result"]["instructions"]
            .as_str()
            .unwrap()
            .starts_with("hive is journal-first."));

        // Unsupported version → the SDK's latest.
        let res = handle_request(
            &store,
            &authed(),
            &req(
                "initialize",
                json!({"protocolVersion": "1999-01-01", "capabilities": {}, "clientInfo": {"name": "t", "version": "0"}}),
                2,
            ),
        )
        .await;
        assert_eq!(
            res["result"]["protocolVersion"],
            mcp::LATEST_PROTOCOL_VERSION
        );

        // Missing params → the protocol layer's -32603 zod message (measured).
        let res = handle_request(
            &store,
            &authed(),
            &json!({"jsonrpc": "2.0", "id": 3, "method": "initialize"}),
        )
        .await;
        assert_eq!(res["error"]["code"], -32603);
        assert_eq!(
            res["error"]["message"],
            "[\n  {\n    \"expected\": \"object\",\n    \"code\": \"invalid_type\",\n    \"path\": [\n      \"params\"\n    ],\n    \"message\": \"Invalid input: expected object, received undefined\"\n  }\n]"
        );
    }

    #[tokio::test]
    async fn tools_list_matches_node_surface() {
        let (store, _dir) = test_store().await;
        let res = handle_request(&store, &authed(), &req("tools/list", json!({}), 1)).await;
        let tools = res["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // Node order, then the Rust-branch identity tools.
        assert_eq!(
            names,
            vec![
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
                "dashboard",
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
                "share_entry",
                "actor_delete",
                "actor_merge",
                "identity_link",
                "identity_resolve",
                "identity_list",
                "identity_unlink",
                "artifacts_list",
                "artifacts_get",
            ]
        );
        // Spot-check a schema verbatim against the captured Node output.
        let journal_get = tools.iter().find(|t| t["name"] == "journal_get").unwrap();
        assert_eq!(
            journal_get["inputSchema"],
            json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"],
                "additionalProperties": false,
                "$schema": "http://json-schema.org/draft-07/schema#"
            })
        );
        // Every parity tool carries the SDK's execution stanza.
        for t in tools.iter().take(29) {
            assert_eq!(
                t["execution"],
                json!({"taskSupport": "forbidden"}),
                "{}",
                t["name"]
            );
        }
    }

    #[tokio::test]
    async fn tools_call_results_and_errors() {
        let (store, _dir) = test_store().await;

        // Authorship pins to the token actor even if the client tries to spoof.
        let res = handle_request(
            &store,
            &ctx_for("pia"),
            &req(
                "tools/call",
                json!({"name": "journal_append", "arguments": {"body": "hello hive", "author": "nate"}}),
                1,
            ),
        )
        .await;
        let entry = content_text(&res["result"]);
        assert_eq!(entry["author"], "pia");

        // Missing required arg → the SDK's exact wrapped zod message (measured).
        let res = handle_request(
            &store,
            &authed(),
            &req(
                "tools/call",
                json!({"name": "journal_get", "arguments": {}}),
                2,
            ),
        )
        .await;
        assert_eq!(res["result"]["isError"], true);
        assert_eq!(
            res["result"]["content"][0]["text"],
            "MCP error -32602: Input validation error: Invalid arguments for tool journal_get: [\n  {\n    \"code\": \"invalid_type\",\n    \"expected\": \"string\",\n    \"received\": \"undefined\",\n    \"path\": [\n      \"id\"\n    ],\n    \"message\": \"Required\"\n  }\n]"
        );

        // Unknown tool → isError content, not a JSON-RPC error (measured).
        let res = handle_request(
            &store,
            &authed(),
            &req(
                "tools/call",
                json!({"name": "nope_not_a_tool", "arguments": {}}),
                3,
            ),
        )
        .await;
        assert_eq!(res["result"]["isError"], true);
        assert_eq!(
            res["result"]["content"][0]["text"],
            "MCP error -32602: Tool nope_not_a_tool not found"
        );

        // Unknown method → -32601 (measured).
        let res = handle_request(&store, &authed(), &req("no/such_method", json!({}), 4)).await;
        assert_eq!(res["error"]["code"], -32601);
        assert_eq!(res["error"]["message"], "Method not found");

        // ping → empty result (measured).
        let res = handle_request(
            &store,
            &authed(),
            &json!({"jsonrpc": "2.0", "id": 5, "method": "ping"}),
        )
        .await;
        assert_eq!(res["result"], json!({}));

        // Admin gate: nate (onboarding admin) previews; pia is refused.
        let res = handle_request(
            &store,
            &ctx_for("pia"),
            &req(
                "tools/call",
                json!({"name": "actor_delete", "arguments": {"slug": "nate"}}),
                6,
            ),
        )
        .await;
        assert_eq!(
            content_text(&res["result"]),
            json!({"error": "forbidden — admin only"})
        );
        let res = handle_request(
            &store,
            &authed(),
            &req(
                "tools/call",
                json!({"name": "actor_delete", "arguments": {"slug": "nate", "dry_run": true}}),
                7,
            ),
        )
        .await;
        assert!(
            res["result"]["isError"].is_null(),
            "dry-run preview should succeed: {res}"
        );
    }

    #[tokio::test]
    async fn http_layer_parity() {
        let (store, _dir) = test_store().await;

        // Unauthenticated → 401 + RFC 9728 pointer (Node's raw-server shape).
        let res = post(&store, AuthCtx::default(), req("tools/list", json!({}), 1)).await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let www = res
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(www.starts_with("Bearer resource_metadata=\""));
        assert!(www.contains("/.well-known/oauth-protected-resource"));
        let body = body_json(res).await;
        assert_eq!(body["error"]["code"], -32001);
        assert_eq!(
            body["error"]["message"],
            "Unauthorized — authorize via OAuth or provide a Bearer API token."
        );
        assert_eq!(body["id"], Value::Null);

        // Notifications get 202 with no body.
        let res = post(
            &store,
            authed(),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        assert_eq!(body_json(res).await, Value::Null);

        // Invalid JSON → 400 -32700 (transport message).
        let res = post_raw(&store, authed(), "{not json".to_string()).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = body_json(res).await;
        assert_eq!(body["error"]["code"], -32700);
        assert_eq!(body["error"]["message"], "Parse error: Invalid JSON");

        // Valid JSON, invalid JSON-RPC → 400 -32700 (transport message).
        let res = post(&store, authed(), json!({"hello": "world"})).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        let body = body_json(res).await;
        assert_eq!(
            body["error"]["message"],
            "Parse error: Invalid JSON-RPC message"
        );

        // Missing Accept pair → 406; wrong Content-Type → 415 (transport gates).
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let res = entry(
            State(store.clone()),
            Extension(authed()),
            Method::POST,
            headers,
            Bytes::from(req("tools/list", json!({}), 1).to_string()),
        )
        .await
        .map_err(|_| "handler error")
        .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_ACCEPTABLE);

        // Non-POST (authed) → Node's 405.
        let res = entry(
            State(store.clone()),
            Extension(authed()),
            Method::GET,
            HeaderMap::new(),
            Bytes::new(),
        )
        .await
        .map_err(|_| "handler error")
        .unwrap();
        assert_eq!(res.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(
            res.headers()
                .get(header::ALLOW)
                .and_then(|v| v.to_str().ok()),
            Some("POST")
        );
        let body = body_json(res).await;
        assert_eq!(
            body["error"]["message"],
            "Use POST for the stateless MCP endpoint."
        );

        // Batch of two requests → array response; single request → object.
        let res = post(
            &store,
            authed(),
            json!([req("ping", json!({}), 10), req("tools/list", json!({}), 11)]),
        )
        .await;
        let body = body_json(res).await;
        let arr = body.as_array().expect("batch array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], 10);
    }
}
