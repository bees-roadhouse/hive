// The bridge over its real transport: spawn the built binary, drive the MCP
// stdio handshake (initialize → initialized → tools/list → tools/call), and
// check the one-shot `call` mode hooks use. Hermetic: HIVE_DATA_DIR points
// each test at its own tempdir and HIVE_MEMORY_KEY_HEX supplies the master
// key, so no OS keychain is touched (both are bridge-only escape hatches).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

/// 64 hex chars = 32 bytes; any fixed value works with MemoryKeySource.
const KEY_HEX: &str = "0707070707070707070707070707070707070707070707070707070707070707";
const RECV_TIMEOUT: Duration = Duration::from_secs(60);

fn bridge_command(dir: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_hive-bridge"));
    cmd.env("HIVE_DATA_DIR", dir)
        .env("HIVE_MEMORY_KEY_HEX", KEY_HEX)
        .args(["--actor", "nate"]);
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
        drop(self.stdin); // EOF ends the serve loop
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

    // Garbage line: -32700 parse error with a null id.
    bridge.send_raw("this is not json");
    let reply = bridge.recv();
    assert_eq!(reply["error"]["code"], json!(-32700));
    assert!(reply["id"].is_null());

    bridge.close();
}

#[test]
fn unsupported_protocol_version_gets_countered_with_latest() {
    let dir = tempfile::tempdir().unwrap();
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

    // Write (each invocation opens, commits, releases — sequential is fine).
    // The [task:] token emerges a task auto-assigned to the acting identity.
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
    // semantic and stays empty until an embedding backfill runs — the
    // backfill daemon returns with the Phase 2/3 app loop.)
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

#[test]
fn a_second_process_on_the_same_data_dir_is_told_to_close_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let mut bridge = Serving::spawn(dir.path());
    // The initialize reply proves the serving bridge holds the store (and
    // therefore the data-dir lock) before the contender starts.
    bridge.request(
        0,
        "initialize",
        json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "x", "version": "0"}}),
    );

    let out = run_call(dir.path(), "journal_list", json!({}));
    assert!(
        !out.status.success(),
        "second opener must be refused while the bridge serves"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("another hive process"),
        "stderr must carry the mutual-exclusion message: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    bridge.close();

    // Lock released on exit: the same call now succeeds.
    let out = run_call(dir.path(), "journal_list", json!({}));
    assert!(
        out.status.success(),
        "lock must release when the bridge exits"
    );
}
