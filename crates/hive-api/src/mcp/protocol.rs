use serde::Deserialize;
use serde_json::{Value, json};

use crate::auth::claims::{Principal, ResolvedPermissions};
use crate::mcp::tools;
use crate::state::AppState;

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "hive-api";

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: Option<String>,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// Dispatch one JSON-RPC payload (single request or batch array).
pub async fn handle_jsonrpc(state: &AppState, principal: Option<&Principal>, body: &str) -> Value {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return error_response(None, -32600, "empty request body");
    }

    if trimmed.starts_with('[') {
        let requests: Vec<JsonRpcRequest> = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => return error_response(None, -32700, &format!("parse error: {e}")),
        };
        let mut responses = Vec::new();
        for req in requests {
            if let Some(resp) = dispatch_one(state, principal, req).await {
                responses.push(resp);
            }
        }
        return Value::Array(responses);
    }

    let req: JsonRpcRequest = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => return error_response(None, -32700, &format!("parse error: {e}")),
    };
    match dispatch_one(state, principal, req).await {
        Some(resp) => resp,
        None => json!(null),
    }
}

async fn dispatch_one(
    state: &AppState,
    principal: Option<&Principal>,
    req: JsonRpcRequest,
) -> Option<Value> {
    if req.jsonrpc.as_deref().is_some_and(|v| v != "2.0") {
        return Some(error_response(req.id, -32600, "jsonrpc must be \"2.0\""));
    }

    let perms = effective_permissions(principal);
    let method = req.method.as_str();

    let result = match method {
        "initialize" => Ok(initialize_result()),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools::list_definitions()),
        "tools/call" => tools::call(state, principal, &perms, &req.params).await,
        // MCP notification — no JSON-RPC response.
        "notifications/initialized" | "notifications/cancelled" => return None,
        _ => Err(format!("unknown method: {method}")),
    };

    match result {
        Ok(value) => Some(success_response(req.id, value)),
        Err(msg) => Some(error_response(req.id, -32603, &msg)),
    }
}

fn effective_permissions(principal: Option<&Principal>) -> ResolvedPermissions {
    match principal {
        Some(p) => p.permissions.clone(),
        None => ResolvedPermissions::full(),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn success_response(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn error_response(id: Option<Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_shape() {
        let v = initialize_result();
        assert_eq!(v["protocolVersion"], PROTOCOL_VERSION);
        assert!(v["capabilities"]["tools"].is_object());
        assert_eq!(v["serverInfo"]["name"], SERVER_NAME);
    }

    #[test]
    fn error_response_has_code() {
        let v = error_response(Some(json!(1)), -32601, "nope");
        assert_eq!(v["error"]["code"], -32601);
        assert_eq!(v["id"], 1);
    }
}
