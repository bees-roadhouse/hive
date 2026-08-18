// The bridge socket contract (D25; PLAN.md PR 2.4) — shared by the app
// (which serves it) and hive-bridge (which proxies stdio MCP over it), so
// the two binaries can never drift on the wire shape.
//
// Transport: newline-delimited JSON over a unix domain socket at
// `<data_dir>/bridge.sock`. The first line each way is a handshake — the
// bridge announces its protocol version and acting identity, the app
// acknowledges — and every line after that is a raw MCP JSON-RPC 2.0 frame,
// exactly what the bridge's stdio side speaks. The handshake is what lets
// the bridge tell "the hive app" apart from "something else bound this
// path", and it carries the actor so `--actor` keeps pinning authorship
// per connection without touching the MCP frames themselves.

use serde::{Deserialize, Serialize};

/// Socket filename inside the data dir. The data dir (not XDG_RUNTIME_DIR)
/// so one path works in and out of the flatpak, and so the bridge's
/// HIVE_DATA_DIR override relocates the socket and the store together.
pub const BRIDGE_SOCKET_FILE: &str = "bridge.sock";

/// Bumped when the handshake or framing changes shape. The app refuses a
/// mismatched hello rather than guessing.
pub const BRIDGE_PROTO_VERSION: u32 = 1;

/// First line bridge → app: `{"hive_bridge":{"proto":1,"actor":"nate"}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeHello {
    pub proto: u32,
    pub actor: String,
}

/// First line app → bridge: `{"hive_app":{"proto":1,"data_dir":"..."}}`.
/// `data_dir` is informational (the bridge's serve banner names the store).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppAck {
    pub proto: u32,
    pub data_dir: String,
}

#[derive(Serialize, Deserialize)]
struct HelloWire {
    hive_bridge: BridgeHello,
}

#[derive(Serialize, Deserialize)]
struct AckWire {
    hive_app: AppAck,
}

pub fn hello_line(actor: &str) -> String {
    serde_json::to_string(&HelloWire {
        hive_bridge: BridgeHello {
            proto: BRIDGE_PROTO_VERSION,
            actor: actor.to_string(),
        },
    })
    .expect("hello serializes")
}

pub fn parse_hello(line: &str) -> Option<BridgeHello> {
    serde_json::from_str::<HelloWire>(line)
        .ok()
        .map(|w| w.hive_bridge)
}

pub fn ack_line(data_dir: &str) -> String {
    serde_json::to_string(&AckWire {
        hive_app: AppAck {
            proto: BRIDGE_PROTO_VERSION,
            data_dir: data_dir.to_string(),
        },
    })
    .expect("ack serializes")
}

pub fn parse_ack(line: &str) -> Option<AppAck> {
    serde_json::from_str::<AckWire>(line)
        .ok()
        .map(|w| w.hive_app)
}

/// The Windows doorway name for a data dir.
///
/// Windows has no filesystem sockets. The unix socket took its identity from
/// its PATH — one `bridge.sock` per data dir, so two hives (a test store, a
/// HIVE_DATA_DIR relocation) could never share a doorway. A named pipe lives
/// in the machine-global `\\.\pipe\` namespace instead, so that identity has
/// to move into the NAME or those two hives would collide on one pipe.
///
/// FNV-1a over the lowercased path: this NAMESPACES, it does not authenticate.
/// A crypto hash here would imply the name is a secret, and it is not — the
/// pipe name is discoverable by anything on the machine. Access control is the
/// pipe's DACL (owner-only) plus FILE_FLAG_FIRST_PIPE_INSTANCE on the app
/// side, exactly as the unix side leant on 0600 plus SO_PEERCRED rather than
/// on an unguessable path.
///
/// Lowercased because Windows paths are case-insensitive: the app and the
/// bridge must land on the same name having each resolved the dir themselves.
#[cfg(windows)]
pub fn pipe_path(data_dir: &std::path::Path) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

    let key = data_dir.to_string_lossy().to_lowercase();
    let mut hash = FNV_OFFSET;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!(r"\\.\pipe\hive-bridge-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trips() {
        let hello = parse_hello(&hello_line("nate")).expect("hello parses");
        assert_eq!(hello.proto, BRIDGE_PROTO_VERSION);
        assert_eq!(hello.actor, "nate");
        let ack = parse_ack(&ack_line("/data/hive")).expect("ack parses");
        assert_eq!(ack.proto, BRIDGE_PROTO_VERSION);
        assert_eq!(ack.data_dir, "/data/hive");
    }

    #[test]
    fn foreign_lines_do_not_parse_as_handshake() {
        assert!(parse_hello("{\"jsonrpc\":\"2.0\",\"id\":1}").is_none());
        assert!(parse_hello("not json").is_none());
        assert!(parse_ack("{}").is_none());
    }

    /// The app and the bridge each resolve the data dir themselves and must
    /// meet on one name. If this ever stops being a pure function of the path,
    /// they stop meeting and the doorway silently does not exist.
    #[cfg(windows)]
    #[test]
    fn the_pipe_name_is_stable_per_data_dir_and_case_insensitive() {
        use std::path::Path;

        let a = pipe_path(Path::new(r"C:\Users\nate\AppData\Local\hive"));
        assert_eq!(a, pipe_path(Path::new(r"C:\Users\nate\AppData\Local\hive")));
        // Windows paths are case-insensitive, so these are the SAME store —
        // the app writing one casing and the bridge another must not split
        // the doorway in two.
        assert_eq!(a, pipe_path(Path::new(r"c:\users\nate\appdata\local\hive")));
        assert!(a.starts_with(r"\\.\pipe\hive-bridge-"));
    }

    /// Two stores, two doorways. The unix side got this free from the path;
    /// on Windows it is the hash doing the work, so it is worth asserting.
    #[cfg(windows)]
    #[test]
    fn different_data_dirs_get_different_pipes() {
        use std::path::Path;

        assert_ne!(
            pipe_path(Path::new(r"C:\Users\nate\AppData\Local\hive")),
            pipe_path(Path::new(r"C:\tmp\hive-test-store")),
        );
    }
}
