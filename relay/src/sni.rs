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
//! Every length is checked against the remaining slice. There is no recursion
//! and a malformed hello returns `None` instead of panicking. Allocation is
//! bounded by [`MAX_HELLO`]: one `String` for the name, plus one coalescing
//! copy when a hello arrives fragmented across records.

/// A ClientHello has to arrive within this many bytes or the connection is not
/// something this relay can route. It is also the cap on a single TLS record,
/// which the protocol puts at 2^14 anyway.
pub const MAX_HELLO: usize = 16 * 1024;

/// What a peek at the buffered bytes established.
///
/// These three are deliberately distinct. Conflating the first two costs a
/// connection slot for the whole handshake deadline every time someone sends a
/// hello with no SNI, which is the cheapest resource hold on the port.
#[derive(Debug, PartialEq, Eq)]
pub enum Peek {
    /// Well-formed so far, and truncated. Read more and ask again.
    Incomplete,
    /// A complete ClientHello naming this host.
    Name(String),
    /// A complete ClientHello carrying no usable `server_name`. There is
    /// nothing to route on and no amount of further reading changes that, so
    /// the caller must fail immediately rather than wait for more bytes.
    Nameless,
}

/// Extract the SNI host from a buffered TLS ClientHello.
///
/// `Err(())` means the bytes are not a TLS handshake at all.
#[allow(clippy::result_unit_err)]
pub fn peek_sni(buf: &[u8]) -> Result<Peek, ()> {
    let Some((first, mut next)) = record_at(buf, 0)? else {
        return Ok(Peek::Incomplete);
    };
    if let Some(body) = client_hello_body(first)? {
        return parse_hello(body);
    }

    // The hello is fragmented across records. Legal, some stacks do it, and
    // refusing it would be a compat break rather than a security property ...
    // so coalesce, bounded by MAX_HELLO like everything else here.
    let mut msg = first.to_vec();
    loop {
        let Some((more, after)) = record_at(buf, next)? else {
            return Ok(Peek::Incomplete);
        };
        msg.extend_from_slice(more);
        if msg.len() > MAX_HELLO {
            return Err(());
        }
        next = after;
        if let Some(body) = client_hello_body(&msg)? {
            return parse_hello(body);
        }
    }
}

/// One TLS record body starting at `at`, plus the offset just past it.
/// `Ok(None)` means the record is not fully buffered yet.
fn record_at(buf: &[u8], at: usize) -> Result<Option<(&[u8], usize)>, ()> {
    // record: type(1) version(2) length(2)
    let head = buf.get(at..).ok_or(())?;
    if head.len() < 5 {
        return Ok(None);
    }
    if head[0] != 0x16 {
        return Err(()); // not a handshake record
    }
    let len = u16::from_be_bytes([head[3], head[4]]) as usize;
    if len == 0 || len > MAX_HELLO {
        return Err(());
    }
    let Some(body) = head.get(5..5 + len) else {
        return Ok(None);
    };
    Ok(Some((body, at + 5 + len)))
}

/// The ClientHello body inside a handshake message. `Ok(None)` means the
/// message continues into a record that has not been read yet.
fn client_hello_body(msg: &[u8]) -> Result<Option<&[u8]>, ()> {
    // handshake: type(1) length(3)
    if msg.is_empty() {
        return Err(());
    }
    if msg[0] != 0x01 {
        return Err(()); // not a ClientHello
    }
    if msg.len() < 4 {
        return Ok(None); // header itself split across records
    }
    let len = u32::from_be_bytes([0, msg[1], msg[2], msg[3]]) as usize;
    if len > MAX_HELLO {
        return Err(());
    }
    Ok(msg.get(4..4 + len))
}

fn parse_hello(hello: &[u8]) -> Result<Peek, ()> {
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
        return Ok(Peek::Nameless);
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
                return Ok(Peek::Name(host.to_ascii_lowercase()));
            }
        }
        return Ok(Peek::Nameless);
    }
    Ok(Peek::Nameless)
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

    client_hello_with_extensions(&ext)
}

/// A well-formed ClientHello with no extensions at all, so no SNI. Test
/// support: this is the shape that must fail fast rather than be waited on.
#[doc(hidden)]
pub fn test_client_hello_without_sni() -> Vec<u8> {
    client_hello_with_extensions(&[])
}

fn client_hello_with_extensions(ext: &[u8]) -> Vec<u8> {
    let mut hello = Vec::new();
    hello.extend_from_slice(&0x0303u16.to_be_bytes()); // legacy_version
    hello.extend_from_slice(&[0u8; 32]); // random
    hello.push(0); // session id length
    hello.extend_from_slice(&2u16.to_be_bytes()); // cipher suites length
    hello.extend_from_slice(&[0x13, 0x01]);
    hello.push(1); // compression methods length
    hello.push(0);
    if !ext.is_empty() {
        hello.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        hello.extend_from_slice(ext);
    }

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

/// Re-frame a single-record ClientHello into records of at most `chunk` bytes
/// of handshake payload each. Test support for the fragmented case, which is
/// legal TLS and which some stacks emit.
#[doc(hidden)]
pub fn test_fragment_records(single_record: &[u8], chunk: usize) -> Vec<u8> {
    let hs = &single_record[5..];
    let mut out = Vec::new();
    for piece in hs.chunks(chunk.max(1)) {
        out.push(0x16);
        out.extend_from_slice(&0x0301u16.to_be_bytes());
        out.extend_from_slice(&(piece.len() as u16).to_be_bytes());
        out.extend_from_slice(piece);
    }
    out
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
            Peek::Name("hv7bqk.relay.example".to_string())
        );
    }

    #[test]
    fn lowercases_the_name() {
        let hello = client_hello("HV7BQK.Relay.Example");
        assert_eq!(
            peek_sni(&hello).expect("parse"),
            Peek::Name("hv7bqk.relay.example".to_string())
        );
    }

    #[test]
    fn asks_for_more_bytes_when_truncated() {
        let hello = client_hello("a.relay.example");
        for cut in [0, 1, 4, 5, 20, hello.len() - 1] {
            assert_eq!(
                peek_sni(&hello[..cut]),
                Ok(Peek::Incomplete),
                "cut at {cut}"
            );
        }
    }

    /// A complete hello with no SNI is a decided answer, not a truncated one.
    /// The daemon must be able to hang up on it immediately instead of
    /// holding a slot until the handshake deadline.
    #[test]
    fn a_complete_hello_without_sni_is_not_incomplete() {
        let hello = test_client_hello_without_sni();
        assert_eq!(peek_sni(&hello), Ok(Peek::Nameless));
    }

    /// A ClientHello split across TLS records is legal and some stacks emit
    /// it. Refusing it would be a compat break, not a security property.
    #[test]
    fn reassembles_a_hello_split_across_records() {
        let single = client_hello("frag.relay.example");
        for chunk in [1, 3, 17, 64] {
            let split = test_fragment_records(&single, chunk);
            assert!(split.len() > single.len(), "chunk {chunk} should fragment");
            assert_eq!(
                peek_sni(&split).expect("parse"),
                Peek::Name("frag.relay.example".to_string()),
                "chunk {chunk}"
            );
            // ... and every prefix of it still just asks for more bytes.
            for cut in 0..split.len() {
                assert_eq!(
                    peek_sni(&split[..cut]),
                    Ok(Peek::Incomplete),
                    "chunk {chunk} cut at {cut}"
                );
            }
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
