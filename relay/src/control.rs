//! The control channel: newline-delimited JSON, one message per line.
//!
//! This is the ONLY thing either side parses. Once a data connection is paired
//! it becomes an opaque byte pipe and nothing reads it again ... which is the
//! whole point, so it is worth being explicit about where parsing stops.
//!
//! The control channel is deliberately boring. It is not the data path, so its
//! simplicity costs nothing, and JSON-per-line means an operator can `tail` a
//! capture and understand what happened.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt};

/// Hard ceiling on one control line, enforced on the BUFFER rather than after
/// the fact.
///
/// This is the whole defect: `read_line` grows its target until it sees a
/// newline, so checking the length afterwards checks an allocation the peer
/// already chose the size of. The control port is public, unauthenticated
/// until the first line is parsed, and a stream with no newline in it is one
/// `yes | nc relay 7500` away.
pub const MAX_LINE: usize = 8 * 1024;

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

/// Newline-delimited reader with a hard ceiling and no hidden buffering.
///
/// Two properties this has and `BufReader::read_line` does not:
///
/// * **The ceiling bounds the allocation, not the outcome.** Nothing a peer
///   sends makes this hold more than `MAX_LINE` plus one read.
/// * **It is cancel safe.** Partial data stays in `buf`, so dropping the
///   future in a `select!` arm loses nothing. `read_line` moves the caller's
///   `String` into the future and drops it on cancel, which silently eats a
///   half-received line.
///
/// [`into_parts`](Self::into_parts) hands back whatever was read past the last
/// line, which is why the daemon can turn a control socket into a data socket
/// without dropping bytes on the floor.
pub struct LineReader<S> {
    sock: S,
    buf: Vec<u8>,
}

impl<S: AsyncRead + Unpin> LineReader<S> {
    pub fn new(sock: S) -> Self {
        Self {
            sock,
            buf: Vec::with_capacity(512),
        }
    }

    /// The next line, without its newline. `Ok(None)` is a clean EOF.
    pub async fn next_line(&mut self) -> Result<Option<String>> {
        loop {
            if let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=i).collect();
                let line = String::from_utf8(line).context("control line was not utf-8")?;
                return Ok(Some(line.trim_end_matches(['\r', '\n']).to_string()));
            }
            if self.buf.len() >= MAX_LINE {
                bail!("control line exceeded {MAX_LINE} bytes with no newline");
            }
            let mut chunk = [0u8; 1024];
            // `read` is cancel safe: cancelled, no bytes were consumed.
            let n = self.sock.read(&mut chunk).await?;
            if n == 0 {
                return Ok(None);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    pub fn get_mut(&mut self) -> &mut S {
        &mut self.sock
    }

    /// The socket, plus anything read past the last line returned.
    pub fn into_parts(self) -> (S, Vec<u8>) {
        (self.sock, self.buf)
    }
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

    #[tokio::test]
    async fn reads_lines_and_keeps_the_remainder() {
        let src = b"{\"t\":\"pong\"}\nleftover-bytes".to_vec();
        let mut r = LineReader::new(std::io::Cursor::new(src));
        assert_eq!(
            r.next_line().await.expect("line").as_deref(),
            Some("{\"t\":\"pong\"}")
        );
        // Read to EOF so the residue is in the buffer, then check it survives.
        assert_eq!(r.next_line().await.expect("eof"), None);
        let (_sock, residue) = r.into_parts();
        assert_eq!(residue, b"leftover-bytes");
    }

    /// The control port is public and pre-authentication. A stream with no
    /// newline in it must cost a bounded amount of memory and then an error,
    /// not an allocation the size of whatever the peer felt like sending.
    #[tokio::test]
    async fn a_line_with_no_newline_is_capped() {
        let src = vec![b'A'; 4 * 1024 * 1024];
        let mut r = LineReader::new(std::io::Cursor::new(src));
        let err = r.next_line().await.expect_err("must refuse");
        assert!(err.to_string().contains("exceeded"), "{err}");
        let (_sock, held) = r.into_parts();
        assert!(
            held.len() <= MAX_LINE + 1024,
            "buffered {} bytes, which is not bounded",
            held.len()
        );
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
