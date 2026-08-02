// Unix-only, like proxy mode itself (the binary refuses on Windows until
// named pipes land with the Windows bundles).
#![cfg(unix)]

// The bridge over its real transport: stand up a socket host (a real Store
// served through core's frame layer over a unix listener — exactly the
// app-side shape, minus the GUI), spawn the built binary against it, drive
// the MCP stdio handshake (initialize → initialized → tools/list →
// tools/call), and check the one-shot `call` mode hooks use. Hermetic:
// HIVE_DATA_DIR points each test at its own tempdir (socket and store
// together), MemoryKeySource supplies the master key — no OS keychain.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::{mpsc, Arc};
use std::time::Duration;

use hive_core::keys::MemoryKeySource;
use hive_core::mcp;
use hive_core::store::Store;
use hive_embed::HashEmbedder;
use hive_shared::bridge_proto;
use serde_json::{json, Value};

/// Any fixed 32 bytes work with MemoryKeySource.
const KEY: [u8; 32] = [7u8; 32];
const RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// The app side in miniature: open the store, bind `<dir>/bridge.sock`, and
/// serve hello/ack + `mcp::handle_frame` per connection, concurrently, on a
/// detached thread (it dies with the test process). Returns once the socket
/// is bound, so a bridge spawned right after cannot race the listener.
fn start_host(dir: &Path) {
    let store = Store::new(dir, Arc::new(MemoryKeySource(KEY)), Arc::new(HashEmbedder))
        .expect("host store opens");
    let sock = dir.join(bridge_proto::BRIDGE_SOCKET_FILE);
    let (bound_tx, bound_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("host runtime");
        rt.block_on(async move {
            let listener = tokio::net::UnixListener::bind(&sock).expect("bind host socket");
            bound_tx.send(()).expect("signal bound");
            loop {
                let (stream, _) = listener.accept().await.expect("host accept");
                let store = store.clone();
                tokio::spawn(async move {
                    let _ = host_connection(store, stream).await;
                });
            }
        });
    });
    bound_rx
        .recv_timeout(RECV_TIMEOUT)
        .expect("host socket bound within timeout");
}

async fn host_connection(store: Store, stream: tokio::net::UnixStream) -> std::io::Result<()> {
    // The REAL app-side serving code (handshake + frame loop) — not a
    // replica — so these tests exercise exactly what the app runs; only the
    // peer-cred check and socket setup are app-only.
    let (read, write) = stream.into_split();
    mcp::serve_bridge_connection(&store, "owner", tokio::io::BufReader::new(read), write).await
}

fn bridge_command(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hive-bridge"));
    cmd.env("HIVE_DATA_DIR", dir).args(["--actor", "nate"]);
    cmd
}

/// A serving bridge with line-framed send/recv (reader thread + channel so a
/// wedged bridge fails the test by timeout instead of hanging it).
struct Serving {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
}

impl Serving {
    fn spawn(dir: &Path) -> Serving {
        let mut child = bridge_command(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // bridge logs belong in test output
            .spawn()
            .expect("spawn hive-bridge");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Serving {
            child,
            stdin,
            lines,
        }
    }

    fn send_raw(&mut self, line: &str) {
        writeln!(self.stdin, "{line}").expect("write frame");
        self.stdin.flush().expect("flush frame");
    }

    fn send(&mut self, msg: Value) {
        self.send_raw(&msg.to_string());
    }

    fn recv(&mut self) -> Value {
        let line = self
            .lines
            .recv_timeout(RECV_TIMEOUT)
            .expect("bridge reply within timeout");
        serde_json::from_str(&line).expect("stdout line is one JSON-RPC frame")
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let reply = self.recv();
        assert_eq!(reply["jsonrpc"], "2.0");
        assert_eq!(reply["id"], json!(id), "reply must echo the request id");
        reply
    }

    fn close(mut self) {
        drop(self.stdin); // EOF ends the pump
        let status = self.child.wait().expect("bridge exit");
        assert!(status.success(), "bridge must exit 0 on EOF: {status:?}");
    }
}

/// The tool payload inside a CallToolResult: content[0].text parsed as JSON.
fn tool_payload(result: &Value) -> Value {
    assert_ne!(
        result.get("isError").and_then(Value::as_bool),
        Some(true),
        "tool answered isError: {result}"
    );
    let text = result["content"][0]["text"].as_str().expect("text block");
    serde_json::from_str(text).expect("tool text is JSON")
}

#[test]
fn stdio_handshake_tools_list_and_tool_calls_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    start_host(dir.path());
    let mut bridge = Serving::spawn(dir.path());

    // initialize: a supported requested version is echoed back.
    let reply = bridge.request(
        0,
        "initialize",
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "stdio-test", "version": "0.0.0"}
        }),
    );
    let init = &reply["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "hive");
    assert!(init["capabilities"]["tools"].is_object());
    assert!(init["instructions"]
        .as_str()
        .unwrap()
        .contains("journal-first"));

    // initialized notification: no reply (the next request answers next).
    bridge.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));

    // tools/list: core's full surface rides through the transport untouched.
    let reply = bridge.request(1, "tools/list", json!({}));
    let tools = reply["result"]["tools"].as_array().expect("tools array");
    assert!(
        tools.len() >= 40,
        "expected the full core tool surface, got {}",
        tools.len()
    );
    assert!(tools.iter().any(
        |t| t["name"] == "journal_append" && t["inputSchema"]["properties"]["body"].is_object()
    ));

    // tools/call: a real write with emergence, then reads that see it.
    let reply = bridge.request(
        2,
        "tools/call",
        json!({"name": "journal_append", "arguments": {
            "body": "Bridged entry about [topic: Stdio Bridges] for the smoke test.",
            "tags": ["bridge-smoke"]
        }}),
    );
    let entry = tool_payload(&reply["result"]);
    assert_eq!(entry["author"], "nate", "--actor pins authorship");

    let reply = bridge.request(
        3,
        "tools/call",
        json!({"name": "journal_list", "arguments": {}}),
    );
    let entries = tool_payload(&reply["result"]);
    assert_eq!(entries.as_array().map(Vec::len), Some(1));

    let reply = bridge.request(
        4,
        "tools/call",
        json!({"name": "search", "arguments": {"q": "stdio bridges"}}),
    );
    let hits = tool_payload(&reply["result"]);
    assert!(
        !hits.as_array().unwrap().is_empty(),
        "FTS must see the bridged entry: {hits}"
    );

    // ping is part of the protocol surface.
    let reply = bridge.request(5, "ping", json!({}));
    assert_eq!(reply["result"], json!({}));

    // Unknown tool: SDK-parity isError CONTENT, not a transport error.
    let reply = bridge.request(
        6,
        "tools/call",
        json!({"name": "no_such_tool", "arguments": {}}),
    );
    assert_eq!(reply["result"]["isError"], json!(true));

    // Unknown method: JSON-RPC -32601 (we declare only tools).
    let reply = bridge.request(7, "resources/list", json!({}));
    assert_eq!(reply["error"]["code"], json!(-32601));

    // Garbage line: forwarded verbatim; the app answers -32700, null id.
    bridge.send_raw("this is not json");
    let reply = bridge.recv();
    assert_eq!(reply["error"]["code"], json!(-32700));
    assert!(reply["id"].is_null());

    bridge.close();
}

#[test]
fn unsupported_protocol_version_gets_countered_with_latest() {
    let dir = tempfile::tempdir().unwrap();
    start_host(dir.path());
    let mut bridge = Serving::spawn(dir.path());
    let reply = bridge.request(
        0,
        "initialize",
        json!({"protocolVersion": "1999-01-01", "capabilities": {}, "clientInfo": {"name": "x", "version": "0"}}),
    );
    assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
    bridge.close();
}

fn run_call(dir: &Path, tool: &str, args: Value) -> Output {
    bridge_command(dir)
        .args(["call", tool, "--json", &args.to_string()])
        .output()
        .expect("run hive-bridge call")
}

#[test]
fn one_shot_call_mode_writes_and_reads_like_the_hooks_do() {
    let dir = tempfile::tempdir().unwrap();
    start_host(dir.path());

    // Write. The [task:] token emerges a task auto-assigned to the actor.
    let out = run_call(
        dir.path(),
        "journal_append",
        json!({"body": "One-shot memory via hive-bridge call — [task: Verify the bridge]."}),
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim())
        .expect("stdout is exactly the result JSON");
    let entry = tool_payload(&result);
    assert_eq!(entry["author"], "nate");

    // Read it back through the same mode (what session-start's recall does):
    // the emerged open task rides the brief. (The brief's JOURNAL section is
    // semantic and stays empty until an embedding backfill runs.)
    let out = run_call(dir.path(), "recall", json!({"identity": "nate"}));
    assert!(out.status.success());
    let result: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let recall = tool_payload(&result);
    assert!(
        recall["brief"]
            .as_str()
            .unwrap()
            .contains("Verify the bridge"),
        "recall brief must include the emerged open task: {recall}"
    );

    // A tool-level failure exits 1 and still prints the result JSON.
    let out = run_call(dir.path(), "no_such_tool", json!({}));
    assert_eq!(out.status.code(), Some(1));
    let result: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!(result["isError"], json!(true));
}

/// The point of proxy mode (PR 2.4): concurrent clients share the one
/// running app — the interim single-writer restriction is gone.
#[test]
fn concurrent_clients_share_the_running_app() {
    let dir = tempfile::tempdir().unwrap();
    start_host(dir.path());
    let mut bridge = Serving::spawn(dir.path());
    bridge.request(
        0,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "x", "version": "0"}}),
    );
    let reply = bridge.request(
        1,
        "tools/call",
        json!({"name": "journal_append", "arguments": {"body": "Written while another client is connected."}}),
    );
    tool_payload(&reply["result"]);

    // A second client, WHILE the first serves: succeeds and sees the write.
    let out = run_call(dir.path(), "journal_list", json!({}));
    assert!(
        out.status.success(),
        "a concurrent one-shot must succeed in proxy mode — stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let result: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    let entries = tool_payload(&result);
    assert_eq!(entries.as_array().map(Vec::len), Some(1));

    bridge.close();
}

/// D25's failure story: no store access of its own — when the app is not
/// running the bridge says so on stderr, exits 1, and prints NOTHING on
/// stdout. The marker text and that shape are the plugin's soft-fail
/// contract (see AGENTS.md).
#[test]
fn no_running_app_is_a_clean_error() {
    let dir = tempfile::tempdir().unwrap(); // no host: nothing listens

    // call mode.
    let out = run_call(dir.path(), "journal_list", json!({}));
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "no stdout on infrastructure failure: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the hive app is not running"),
        "stderr must carry the app-not-running marker: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // serve mode fails the same way, before ever reading stdin.
    let out = bridge_command(dir.path())
        .output()
        .expect("run hive-bridge serve");
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("the hive app is not running"),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
