//! Reading the SNI out of a ClientHello without decrypting anything.
//!
//! This is the one place the daemon looks at a byte of the forwarded stream,
//! and it is worth being precise about what it does and does not do.
//!
//! A TLS ClientHello is sent in the CLEAR, before any key agreement. Extension
//! type 0 carries `server_name`. Reading it is a plain structural parse of a
//! plaintext record ... no key, no cipher, no handshake state. The daemon
//! cannot progress the handshake even if it wanted to, because it holds no
//! certificate for any instance name.
//!
//! Crucially the bytes are PEEKED, not consumed: whatever is read here is
//! replayed verbatim to the instance, so the instance sees a byte-identical
//! ClientHello and completes the handshake with the browser directly. The
//! daemon is not a party to that handshake, which is exactly why it cannot read
//! what follows.
//!
//! frp does the same job differently ... `pkg/util/vhost/https.go` feeds the
//! connection to Go's own TLS server through a `readOnlyConn` whose writes
//! return `io.ErrClosedPipe`, so the standard library parses the hello and then
//! the handshake dies harmlessly. That reuses a hardened parser, which is the
//! better instinct. The parse below is the same idea done by hand, kept small
//! and bounded, because it must run before any allocation decision is made.
//!
//! Every length is checked against the remaining slice. There is no recursion,
//! no allocation beyond the caller's buffer, and a malformed hello returns
//! `None` instead of panicking.

/// A ClientHello has to arrive within this many bytes or the connection is not
/// something this relay can route.
pub const MAX_HELLO: usize = 16 * 1024;

/// Extract the SNI host from a buffered TLS ClientHello.
///
/// Returns `Ok(None)` when the buffer holds a well-formed but incomplete
/// record and more bytes should be read, and `Err(())` when it is not a TLS
/// handshake at all.
#[allow(clippy::result_unit_err)]
pub fn peek_sni(buf: &[u8]) -> Result<Option<String>, ()> {
    // TLS record: type(1) version(2) length(2)
    if buf.len() < 5 {
        return Ok(None);
    }
    if buf[0] != 0x16 {
        return Err(()); // not a handshake record
    }
    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    if record_len == 0 || record_len > MAX_HELLO {
        return Err(());
    }
    if buf.len() < 5 + record_len {
        return Ok(None);
    }
    let body = &buf[5..5 + record_len];

    // Handshake: type(1) length(3)
    let Some(&msg_type) = body.first() else {
        return Err(());
    };
    if msg_type != 0x01 {
        return Err(()); // not a ClientHello
    }
    if body.len() < 4 {
        return Err(());
    }
    let hs_len = u32::from_be_bytes([0, body[1], body[2], body[3]]) as usize;
    let hello = body.get(4..4 + hs_len).ok_or(())?;

    let mut c = Cursor::new(hello);
    c.skip(2)?; // legacy_version
    c.skip(32)?; // random
    let sid = c.u8()? as usize;
    c.skip(sid)?; // legacy_session_id
    let suites = c.u16()? as usize;
    c.skip(suites)?;
    let comp = c.u8()? as usize;
    c.skip(comp)?;

    // Extensions are optional in the grammar; absent means no SNI.
    if c.remaining() == 0 {
        return Ok(None);
    }
    let ext_total = c.u16()? as usize;
    let mut ext = Cursor::new(c.take(ext_total)?);

    while ext.remaining() >= 4 {
        let kind = ext.u16()?;
        let len = ext.u16()? as usize;
        let data = ext.take(len)?;
        if kind != 0x0000 {
            continue;
        }
        // server_name_list: length(2), then entries of type(1) length(2) host
        let mut names = Cursor::new(data);
        let list_len = names.u16()? as usize;
        let mut list = Cursor::new(names.take(list_len)?);
        while list.remaining() >= 3 {
            let name_type = list.u8()?;
            let name_len = list.u16()? as usize;
            let host = list.take(name_len)?;
            if name_type == 0 {
                let host = std::str::from_utf8(host).map_err(|_| ())?;
                return Ok(Some(host.to_ascii_lowercase()));
            }
        }
        return Ok(None);
    }
    Ok(None)
}

/// Bounds-checked forward reader. Every accessor returns `Err` rather than
/// panicking, so a hostile hello cannot take the daemon down.
struct Cursor<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.at)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ()> {
        let end = self.at.checked_add(n).ok_or(())?;
        let slice = self.buf.get(self.at..end).ok_or(())?;
        self.at = end;
        Ok(slice)
    }

    fn skip(&mut self, n: usize) -> Result<(), ()> {
        self.take(n).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, ()> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ()> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
}

/// Strip the zone suffix to recover the instance id: `abc.relay.example` with
/// zone `relay.example` yields `abc`. Rejects deeper names so
/// `evil.abc.relay.example` cannot masquerade.
pub fn instance_from_sni(sni: &str, zone: &str) -> Option<String> {
    let suffix = format!(".{zone}");
    let id = sni.strip_suffix(&suffix)?;
    if id.is_empty() || id.contains('.') {
        return None;
    }
    Some(id.to_string())
}

/// Build a ClientHello carrying `host` in the SNI extension.
///
/// Test support, exposed so the tunnel integration test can drive the daemon
/// without a real TLS stack.
#[doc(hidden)]
pub fn test_client_hello(host: &str) -> Vec<u8> {
    let mut sni = Vec::new();
    sni.push(0u8); // name_type = host_name
    sni.extend_from_slice(&(host.len() as u16).to_be_bytes());
    sni.extend_from_slice(host.as_bytes());

    let mut list = Vec::new();
    list.extend_from_slice(&(sni.len() as u16).to_be_bytes());
    list.extend_from_slice(&sni);

    let mut ext = Vec::new();
    ext.extend_from_slice(&0x0000u16.to_be_bytes()); // server_name
    ext.extend_from_slice(&(list.len() as u16).to_be_bytes());
    ext.extend_from_slice(&list);

    let mut hello = Vec::new();
    hello.extend_from_slice(&0x0303u16.to_be_bytes()); // legacy_version
    hello.extend_from_slice(&[0u8; 32]); // random
    hello.push(0); // session id length
    hello.extend_from_slice(&2u16.to_be_bytes()); // cipher suites length
    hello.extend_from_slice(&[0x13, 0x01]);
    hello.push(1); // compression methods length
    hello.push(0);
    hello.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    hello.extend_from_slice(&ext);

    let mut hs = Vec::new();
    hs.push(0x01); // ClientHello
    hs.extend_from_slice(&(hello.len() as u32).to_be_bytes()[1..]);
    hs.extend_from_slice(&hello);

    let mut rec = Vec::new();
    rec.push(0x16); // handshake
    rec.extend_from_slice(&0x0301u16.to_be_bytes());
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_client_hello as client_hello;

    #[test]
    fn reads_the_server_name() {
        let hello = client_hello("hv7bqk.relay.example");
        assert_eq!(
            peek_sni(&hello).expect("parse"),
            Some("hv7bqk.relay.example".to_string())
        );
    }

    #[test]
    fn lowercases_the_name() {
        let hello = client_hello("HV7BQK.Relay.Example");
        assert_eq!(
            peek_sni(&hello).expect("parse"),
            Some("hv7bqk.relay.example".to_string())
        );
    }

    #[test]
    fn asks_for_more_bytes_when_truncated() {
        let hello = client_hello("a.relay.example");
        for cut in [0, 1, 4, 5, 20, hello.len() - 1] {
            assert_eq!(peek_sni(&hello[..cut]), Ok(None), "cut at {cut}");
        }
    }

    #[test]
    fn rejects_non_tls() {
        assert!(peek_sni(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").is_err());
    }

    /// The parser runs before authentication on a public port, so a hostile
    /// hello is the expected case rather than the exceptional one.
    #[test]
    fn survives_truncation_at_every_offset() {
        let hello = client_hello("a.relay.example");
        for cut in 0..hello.len() {
            let _ = peek_sni(&hello[..cut]);
        }
        for corrupt in 0..hello.len() {
            let mut bad = hello.clone();
            bad[corrupt] = 0xff;
            let _ = peek_sni(&bad);
        }
    }

    #[test]
    fn extracts_the_instance_id() {
        assert_eq!(
            instance_from_sni("abc.relay.example", "relay.example"),
            Some("abc".into())
        );
    }

    #[test]
    fn rejects_deeper_names_and_foreign_zones() {
        assert_eq!(
            instance_from_sni("evil.abc.relay.example", "relay.example"),
            None
        );
        assert_eq!(
            instance_from_sni("abc.other.example", "relay.example"),
            None
        );
        assert_eq!(instance_from_sni("relay.example", "relay.example"), None);
        assert_eq!(instance_from_sni(".relay.example", "relay.example"), None);
    }
}
