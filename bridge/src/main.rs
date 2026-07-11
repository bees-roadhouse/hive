// hive-bridge — the stdio MCP doorway into the local hive store (D25;
// PLAN.md PR 1.8, interim mode).
//
// Interim mode is the ONLY mode in this PR: the bridge opens the store
// directly via `Store::new` — the same data dir the desktop app opens — so
// the store's single-writer lock means the app and the bridge cannot run at
// the same time (the second opener gets a clear error saying so). Phase 2.4
// flips this binary to a UDS proxy against the running app and retires that
// restriction; the CLI surface here (serve + `call`) is built to survive
// that flip unchanged.
//
// Transport rules:
//   - stdout carries JSON-RPC frames ONLY — one message per line
//     (newline-delimited JSON, the MCP stdio framing). Nothing else may
//     ever print there; every log line goes to stderr.
//   - The tool layer is hive-core's `mcp` module (transport-free). This
//     file is a thin loop: parse a frame, dispatch, print the reply. Tool
//     names, schemas, and results are core's — never reimplemented here.
//
// Bridge-only environment (deliberately NOT honored by core or the app):
//   HIVE_DATA_DIR        override the data dir (tests, headless use)
//   HIVE_MEMORY_KEY_HEX  64-hex master key bypassing the OS keychain
//                        (tests, keychain-less headless hosts)

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use hive_core::keys::{KeySource, KeychainKeySource, MemoryKeySource};
use hive_core::mcp::{self, LocalCtx};
use hive_core::store::Store;
use hive_embed::HashEmbedder;
use serde_json::{json, Map, Value};

const USAGE: &str = "\
hive-bridge — stdio MCP server over the local hive store

USAGE:
    hive-bridge [--actor <name>]
        Serve MCP (JSON-RPC 2.0, one message per line) on stdin/stdout.

    hive-bridge call <tool> [--json '<args>'] [--actor <name>]
        One tool call: opens the store, runs the tool, prints the result
        JSON on stdout, exits. Exit code 1 when the tool reports an error.

OPTIONS:
    --actor <name>   Acting identity for tool calls (default: $USER).
    --json '<args>'  Tool arguments as a JSON object (default: {}).

The store lives under $XDG_DATA_HOME/hive (fallback ~/.local/share/hive) —
the same data dir as the hive app, which is why only one of them can run at
a time for now. HIVE_DATA_DIR overrides the location (bridge-only).
";

enum Mode {
    Serve,
    Call {
        tool: String,
        args: Map<String, Value>,
    },
}

struct Cli {
    actor: String,
    mode: Mode,
}

fn main() {
    // Logs (ours and hive-core's tracing events) go to stderr, never stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match run() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("hive-bridge: {e:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<i32> {
    let cli = parse_args(std::env::args().skip(1).collect())?;
    // The store opens BEFORE the tokio runtime exists (the same order the
    // app uses ahead of the dioxus launch): keyring's sync Secret Service
    // backend drives its own zbus executor via block_on, which panics with
    // "Cannot start a runtime from within a runtime" on a tokio thread.
    let store = open_store()?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?;
    rt.block_on(async move {
        let ctx = LocalCtx {
            actor: cli.actor.clone(),
        };
        let outcome = match cli.mode {
            Mode::Serve => serve(&store, &ctx).await.map(|()| 0),
            Mode::Call { tool, args } => one_shot(&store, &ctx, &tool, args).await,
        };
        // Orderly close either way: join the writer thread so the last fold
        // lands and the data-dir lock releases before the process exits.
        store.shutdown().await?;
        outcome
    })
}

fn parse_args(args: Vec<String>) -> Result<Cli> {
    let mut actor: Option<String> = None;
    let mut call_tool: Option<String> = None;
    let mut call_json: Option<String> = None;
    let mut in_call = false;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                // Help is a human interaction, not protocol: stderr.
                eprint!("{USAGE}");
                std::process::exit(0);
            }
            "--actor" => {
                actor = Some(
                    it.next()
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| anyhow!("--actor requires a value\n\n{USAGE}"))?,
                );
            }
            "--json" if in_call => {
                call_json = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--json requires a value\n\n{USAGE}"))?,
                );
            }
            "call" if !in_call && call_tool.is_none() => in_call = true,
            other if in_call && call_tool.is_none() && !other.starts_with('-') => {
                call_tool = Some(other.to_string());
            }
            other => return Err(anyhow!("unrecognized argument {other:?}\n\n{USAGE}")),
        }
    }
    let mode = if in_call {
        let tool = call_tool.ok_or_else(|| anyhow!("call requires a tool name\n\n{USAGE}"))?;
        let args: Value = match call_json.as_deref() {
            None => json!({}),
            Some(raw) => serde_json::from_str(raw).context("--json is not valid JSON")?,
        };
        let args = match args {
            Value::Object(map) => map,
            _ => return Err(anyhow!("--json must be a JSON object")),
        };
        Mode::Call { tool, args }
    } else {
        Mode::Serve
    };
    Ok(Cli {
        actor: actor.unwrap_or_else(default_actor),
        mode,
    })
}

/// Matches the app's author_name(): the OS login name, until identity setup
/// lands (Phase 2 settings).
fn default_actor() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "owner".to_string())
}

/// Mirrors app/src/main.rs::data_dir() exactly — the bridge MUST open the
/// same store the app opens — plus the bridge-only HIVE_DATA_DIR override.
fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("HIVE_DATA_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        });
    base.join("hive")
}

/// Master key: HIVE_MEMORY_KEY_HEX when set (tests / keychain-less hosts),
/// else the OS keychain — resolved exactly once, here, like the app does.
fn master_key() -> Result<[u8; 32]> {
    if let Ok(hex) = std::env::var("HIVE_MEMORY_KEY_HEX") {
        let hex = hex.trim();
        if !hex.is_empty() {
            let bytes = data_encoding::HEXLOWER_PERMISSIVE
                .decode(hex.as_bytes())
                .context("HIVE_MEMORY_KEY_HEX is not valid hex")?;
            return bytes
                .try_into()
                .map_err(|_| anyhow!("HIVE_MEMORY_KEY_HEX must decode to exactly 32 bytes"));
        }
    }
    KeychainKeySource::new()
        .master_key()
        .context("OS keychain unavailable (set HIVE_MEMORY_KEY_HEX only for tests)")
}

fn open_store() -> Result<Store> {
    let dir = data_dir();
    let master = master_key()?;
    // The store works from the in-memory copy thereafter (same shape as the
    // app: keychain once at startup, MemoryKeySource into Store::new).
    Store::new(
        &dir,
        Arc::new(MemoryKeySource(master)),
        Arc::new(HashEmbedder),
    )
    .with_context(|| format!("opening hive store at {}", dir.display()))
}

// ── serve mode: the stdio transport loop ────────────────────────────────────

async fn serve(store: &Store, ctx: &LocalCtx) -> Result<()> {
    eprintln!(
        "[hive-bridge] serving MCP on stdio — store {} — actor {} (interim mode: \
         the hive app can't run while this is connected)",
        store.data_dir().display(),
        ctx.actor
    );
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_frame(store, ctx, line.trim()).await {
            let framed = serde_json::to_string(&reply).context("serializing reply")?;
            stdout
                .write_all(framed.as_bytes())
                .and_then(|()| stdout.write_all(b"\n"))
                .and_then(|()| stdout.flush())
                .context("writing to stdout")?;
        }
    }
    eprintln!("[hive-bridge] stdin closed — shutting down");
    Ok(())
}

/// One inbound frame → at most one reply. Notifications and client→server
/// responses produce none (matching the SDK's stdio transport).
async fn handle_frame(store: &Store, ctx: &LocalCtx, line: &str) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(error_response(
                Value::Null,
                -32700,
                &format!("Parse error: {e}"),
            ))
        }
    };
    let Some(obj) = msg.as_object() else {
        return Some(error_response(Value::Null, -32600, "Invalid Request"));
    };
    // A null id is treated like no id (MCP requires non-null request ids).
    let id = obj.get("id").filter(|v| !v.is_null()).cloned();
    let Some(method) = obj.get("method").and_then(Value::as_str) else {
        if obj.contains_key("result") || obj.contains_key("error") {
            return None; // a response frame; we never issue requests, ignore
        }
        return Some(error_response(
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid Request",
        ));
    };
    let params = obj.get("params").and_then(Value::as_object);
    match id {
        None => None, // notification (notifications/initialized, …): no reply
        Some(id) => Some(handle_request(store, ctx, id, method, params).await),
    }
}

async fn handle_request(
    store: &Store,
    ctx: &LocalCtx,
    id: Value,
    method: &str,
    params: Option<&Map<String, Value>>,
) -> Value {
    match method {
        "initialize" => {
            // Version negotiation per the MCP spec: echo a supported
            // requested version, otherwise offer the latest we speak.
            let requested = params
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str);
            let version = requested
                .filter(|v| mcp::SUPPORTED_PROTOCOL_VERSIONS.contains(v))
                .unwrap_or(mcp::LATEST_PROTOCOL_VERSION);
            result_response(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": mcp::SERVER_NAME, "version": mcp::SERVER_VERSION},
                    "instructions": mcp::instructions(),
                }),
            )
        }
        "ping" => result_response(id, json!({})),
        "tools/list" => result_response(id, json!({"tools": mcp::tools_list()})),
        "tools/call" => {
            let Some(name) = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                return error_response(id, -32602, "Invalid params: missing tool name");
            };
            let args = params
                .and_then(|p| p.get("arguments"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            // Unknown tools and validation failures come back as isError
            // content from core's dispatch — SDK parity, not transport errors.
            result_response(id, mcp::call_tool(store, ctx, &name, &args).await)
        }
        other => error_response(id, -32601, &format!("Method not found: {other}")),
    }
}

fn result_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

// ── call mode: one-shot tool invocation for hooks and scripts ───────────────

/// Run one tool, print the CallToolResult JSON on stdout, and report the
/// exit code: 0 for a clean result, 1 when the tool answered isError (the
/// result still prints, so callers get the failure text either way).
async fn one_shot(
    store: &Store,
    ctx: &LocalCtx,
    tool: &str,
    args: Map<String, Value>,
) -> Result<i32> {
    let result = mcp::call_tool(store, ctx, tool, &args).await;
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut stdout = std::io::stdout();
    stdout
        .write_all(serde_json::to_string(&result)?.as_bytes())
        .and_then(|()| stdout.write_all(b"\n"))
        .and_then(|()| stdout.flush())
        .context("writing result to stdout")?;
    Ok(if is_error { 1 } else { 0 })
}
