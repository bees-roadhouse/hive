// The whole proxy implementation — unix-only (unix sockets). main.rs holds
// the cfg dispatch; the wire contract lives in hive_shared::bridge_proto.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use hive_shared::bridge_proto::{self, AppAck};
use serde_json::{json, Map, Value};

const USAGE: &str = "\
hive-bridge — stdio MCP proxy to the running hive app

USAGE:
    hive-bridge [--actor <name>]
        Serve MCP (JSON-RPC 2.0, one message per line) on stdin/stdout,
        proxied to the hive app over <data_dir>/bridge.sock.

    hive-bridge call <tool> [--json '<args>'] [--actor <name>]
        One tool call through the running app: prints the result JSON on
        stdout, exits. Exit code 1 when the tool reports an error.

OPTIONS:
    --actor <name>   Acting identity for tool calls (default: $USER).
    --json '<args>'  Tool arguments as a JSON object (default: {}).

The hive app must be running: the bridge has no store access of its own
(D25) — it connects to the app's bridge socket under $XDG_DATA_HOME/hive
(fallback ~/.local/share/hive). HIVE_DATA_DIR overrides the location
(bridge-only), for tests and nonstandard homes.
";

/// The stable app-not-running marker. AGENTS.md documents it; the bridge
/// tests match it; docs quote it. Change it only as a contract amendment.
const APP_NOT_RUNNING: &str = "the hive app is not running";

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

pub(crate) fn run() -> Result<i32> {
    let cli = parse_args(std::env::args().skip(1).collect())?;
    let session = connect(&cli.actor)?;
    match cli.mode {
        Mode::Serve => serve(session, &cli.actor).map(|()| 0),
        Mode::Call { tool, args } => one_shot(session, &tool, args),
    }
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

/// Matches the app's author_name(): the OS login name. The hello carries it
/// to the app, which pins tool authorship to it for the connection.
fn default_actor() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "owner".to_string())
}

/// Mirrors app/src/main.rs::data_dir() exactly — the socket lives beside the
/// store the app opened — plus the bridge-only HIVE_DATA_DIR override.
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

/// A live, handshaken connection to the app. `reader` wraps a clone of
/// `stream` and MUST be used for every read after the handshake (it may
/// hold buffered bytes); `stream` stays the write side.
struct Session {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    ack: AppAck,
}

fn connect(actor: &str) -> Result<Session> {
    let path = data_dir().join(bridge_proto::BRIDGE_SOCKET_FILE);
    let mut stream = UnixStream::connect(&path).map_err(|e| match e.kind() {
        // No socket file, or a stale one nothing listens on: the two shapes
        // "app not running" takes. The message marker is a stable contract.
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => anyhow!(
            "{APP_NOT_RUNNING} (no live socket at {}) — start the hive app, then retry",
            path.display()
        ),
        _ => anyhow!("connecting to the hive app at {}: {e}", path.display()),
    })?;
    let mut reader = BufReader::new(stream.try_clone().context("cloning the socket")?);
    stream
        .write_all(format!("{}\n", bridge_proto::hello_line(actor)).as_bytes())
        .context("sending the hive handshake")?;
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("reading the hive handshake reply")?;
    let Some(ack) = bridge_proto::parse_ack(line.trim()) else {
        bail!(
            "the socket at {} did not answer the hive handshake — is something \
             else bound there, or is the app older than this bridge?",
            path.display()
        );
    };
    if ack.proto != bridge_proto::BRIDGE_PROTO_VERSION {
        bail!(
            "the hive app speaks bridge protocol v{} but this hive-bridge speaks \
             v{} — upgrade the older side",
            ack.proto,
            bridge_proto::BRIDGE_PROTO_VERSION
        );
    }
    Ok(Session {
        stream,
        reader,
        ack,
    })
}

// ── serve mode: the stdio ↔ socket pump ─────────────────────────────────────

fn serve(session: Session, actor: &str) -> Result<()> {
    let Session {
        mut stream,
        reader,
        ack,
    } = session;
    eprintln!(
        "[hive-bridge] proxying MCP on stdio → {} (store {}) — actor {}",
        data_dir().join(bridge_proto::BRIDGE_SOCKET_FILE).display(),
        ack.data_dir,
        actor
    );

    // Full duplex, no frame parsing: replies (and only replies — the app
    // never issues requests) flow socket→stdout on their own thread, while
    // this thread pumps stdin→socket. Notifications produce no reply, so a
    // lockstep write-then-read loop would hang on them.
    let closing = Arc::new(AtomicBool::new(false));
    let pump = {
        let closing = Arc::clone(&closing);
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut stdout = std::io::stdout();
            let mut buf: Vec<u8> = Vec::new();
            // read_until, not lines(): at EOF, lines() would yield a torn
            // final line, and a half frame must never reach the protocol
            // channel. Only newline-terminated buffers are forwarded.
            let reason: String = loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf) {
                    Ok(0) => break "the hive app closed the connection".into(),
                    Err(e) => break format!("socket read failed: {e}"),
                    Ok(_) => {}
                }
                if buf.last() != Some(&b'\n') {
                    break "the hive app closed the connection mid-frame (reply dropped)".into();
                }
                if buf.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                if stdout
                    .write_all(&buf)
                    .and_then(|()| stdout.flush())
                    .is_err()
                {
                    // The MCP client hung up, not the app — say so, or a
                    // dead Claude session gets blamed on a healthy hive.
                    break "stdout closed by the MCP client".into();
                }
            };
            // Any of these with stdin still open is a failure, not a
            // shutdown.
            if !closing.load(Ordering::SeqCst) {
                eprintln!("[hive-bridge] {reason} — exiting");
                std::process::exit(1);
            }
        })
    };

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line.context("reading stdin")?;
        if line.trim().is_empty() {
            continue;
        }
        stream
            .write_all(line.as_bytes())
            .and_then(|()| stream.write_all(b"\n"))
            .context("forwarding to the hive app (did it quit?)")?;
    }

    // stdin closed: half-close the socket so the app sees EOF and closes its
    // side; the pump thread drains any in-flight replies, then joins.
    closing.store(true, Ordering::SeqCst);
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = pump.join();
    eprintln!("[hive-bridge] stdin closed — shutting down");
    Ok(())
}

// ── call mode: one-shot tool invocation for hooks and scripts ───────────────

/// Run one tool through the app, print the CallToolResult JSON on stdout,
/// and report the exit code: 0 for a clean result, 1 when the tool answered
/// isError (the result still prints, so callers get the failure text either
/// way). Infrastructure failures print nothing on stdout and exit 1.
fn one_shot(session: Session, tool: &str, args: Map<String, Value>) -> Result<i32> {
    let Session {
        mut stream,
        mut reader,
        ..
    } = session;
    let frame = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    });
    stream
        .write_all(format!("{frame}\n").as_bytes())
        .context("sending the tool call (did the app quit?)")?;

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .context("reading the tool reply")?;
        if n == 0 {
            bail!("lost the connection to the hive app before a reply");
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("id") != Some(&json!(1)) {
            continue;
        }
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("the hive app rejected the call: {msg}");
        }
        let result = v.get("result").cloned().unwrap_or(Value::Null);
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
        return Ok(if is_error { 1 } else { 0 });
    }
}
