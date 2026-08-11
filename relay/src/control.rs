//! The control channel: newline-delimited JSON, one message per line.
//!
//! This is the ONLY thing either side parses. Once a data connection is paired
//! it becomes an opaque byte pipe and nothing reads it again ... which is the
//! whole point, so it is worth being explicit about where parsing stops.
//!
//! The control channel is deliberately boring. It is not the data path, so its
//! simplicity costs nothing, and JSON-per-line means an operator can `tail` a
//! capture and understand what happened.

use serde::{Deserialize, Serialize};

/// Agent to daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Opening message on a control connection: claim an instance id.
    Hello {
        instance: String,
        token: String,
        #[serde(default)]
        label: Option<String>,
    },
    /// Opening message on a DATA connection: claim a pending pairing. After
    /// the daemon accepts this line the socket carries opaque bytes.
    Data {
        nonce: String,
    },
    Pong,
}

/// Daemon to agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    /// Registration accepted. `host` is the name the instance must hold a
    /// certificate for ... the daemon never sees that certificate's key.
    Welcome {
        host: String,
    },
    /// A client is waiting. Dial back with `Data { nonce }`.
    Open {
        nonce: String,
    },
    Ping,
    Error {
        msg: String,
    },
}

/// Serialize a message as one line, newline included.
pub fn line<T: Serialize>(msg: &T) -> anyhow::Result<String> {
    let mut s = serde_json::to_string(msg)?;
    s.push('\n');
    Ok(s)
}

/// An instance id is a house-format nanoid. Constrained tightly because it
/// becomes a DNS label: lowercase alphanumeric and hyphen, 3..=63 characters,
/// no leading or trailing hyphen.
pub fn valid_instance_id(id: &str) -> bool {
    let ok_len = (3..=63).contains(&id.len());
    let ok_chars = id
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    let ok_edges = !id.starts_with('-') && !id.ends_with('-');
    ok_len && ok_chars && ok_edges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_roundtrip() {
        let m = ServerMsg::Open {
            nonce: "abc".into(),
        };
        let wire = line(&m).expect("line");
        assert!(wire.ends_with('\n'));
        let back: ServerMsg = serde_json::from_str(wire.trim()).expect("parse");
        assert!(matches!(back, ServerMsg::Open { ref nonce } if nonce == "abc"));
    }

    #[test]
    fn instance_ids_must_be_dns_safe() {
        assert!(valid_instance_id("hv7bqk2m9x"));
        assert!(!valid_instance_id("HV7BQK"), "uppercase is not a DNS label");
        assert!(!valid_instance_id("ab"), "too short");
        assert!(!valid_instance_id("-lead"), "leading hyphen");
        assert!(!valid_instance_id("trail-"), "trailing hyphen");
        assert!(!valid_instance_id("has.dot"), "a dot would add a label");
        assert!(!valid_instance_id(&"a".repeat(64)), "over the label limit");
    }
}
