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
}
