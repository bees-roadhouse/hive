// The session state machine's refusals (PLAN-v2.1 PR 4.4).
//
// Hermetic and store-free: a mock `SyncSource` and a tempdir vault are enough
// to drive both halves, and a raw peer built out of `frame::{read,write}_*`
// plays the hostile side. That is what the source/sink traits are for — every
// case here is one a real peer can produce, and none of them need a device.
//
// The property under test is single: a session either proceeds on terms both
// sides agreed to, or it stops with an error and lands nothing. Never a
// panic, never a partial application, never a silent skip. These are the
// protocol-level half of ADVERSARIAL-SMOKE (docs/TESTING-STRATEGY.md §2); the
// transport-auth half arrives with mTLS at PRs 4.5/4.7, which is also when
// "who is this peer" stops being an announced string.

use anyhow::Result;
use async_trait::async_trait;
use hive_core::oplog::HeadsSnapshot;
use hive_sync::frame::{
    read_frame, read_payload, write_frame, write_payload, BlockIds, Frame, Hello, PutBlock, Role,
    Segment, WantSegment, MAX_SEGMENT_CHUNK, PROTO_VERSION,
};
use hive_sync::{push_session, receive_session, DirVault, SessionConfig, SyncSource};
use tokio::io::{AsyncRead, AsyncWrite};

const DOMAIN: &str = "example.com";

/// A source with nothing in it — enough for every handshake case, and the
/// base a hostile-answer source specialises.
struct EmptySource;

#[async_trait]
impl SyncSource for EmptySource {
    async fn heads(&self) -> Result<HeadsSnapshot> {
        Ok(HeadsSnapshot::default())
    }
    async fn segment_bytes(&self, _d: &str, _s: u64, _o: u64, _m: u64) -> Result<Vec<u8>> {
        anyhow::bail!("this source holds no segments")
    }
    async fn block_ids(&self) -> Result<Vec<[u8; 32]>> {
        Ok(Vec::new())
    }
    async fn block_bytes(&self, _id: [u8; 32]) -> Result<Vec<u8>> {
        anyhow::bail!("this source holds no blocks")
    }
}

fn vault() -> (tempfile::TempDir, DirVault) {
    let dir = tempfile::tempdir().expect("vault tempdir");
    let vault = DirVault::open(dir.path()).expect("vault opens");
    (dir, vault)
}

fn cfg(domain: &str, peer: &str) -> SessionConfig {
    SessionConfig::new(domain, peer)
}

/// A peer that speaks frames by hand, for cases a well-behaved session
/// cannot produce.
struct RawPeer<R, W> {
    reader: R,
    writer: W,
}

impl<R: AsyncRead + Unpin, W: AsyncWrite + Unpin> RawPeer<R, W> {
    async fn send(&mut self, frame: Frame) -> Result<()> {
        write_frame(&mut self.writer, &frame).await
    }
    async fn send_payload(&mut self, bytes: &[u8]) -> Result<()> {
        write_payload(&mut self.writer, bytes).await
    }
    async fn recv(&mut self) -> Result<Frame> {
        read_frame(&mut self.reader).await
    }
    async fn hello(&mut self, role: Role, domain: &str, proto: u32) -> Result<()> {
        self.send(Frame::Hello(Hello {
            proto,
            domain: domain.to_string(),
            peer: "raw-peer".to_string(),
            role,
        }))
        .await
    }
}

/// Split a duplex pair into a session end and a raw-peer end.
fn rig() -> (
    (impl AsyncRead + Unpin, impl AsyncWrite + Unpin),
    RawPeer<impl AsyncRead + Unpin, impl AsyncWrite + Unpin>,
) {
    let (a, b) = tokio::io::duplex(256 * 1024);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    (
        (ar, aw),
        RawPeer {
            reader: br,
            writer: bw,
        },
    )
}

/// The empty happy path: two well-behaved halves complete a session with
/// nothing to move. Everything below is this, minus one agreement.
#[tokio::test]
async fn an_empty_session_completes_both_ways() {
    let (a, b) = tokio::io::duplex(256 * 1024);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let (_dir, vault) = vault();

    let (pushing, receiving) = (cfg(DOMAIN, "dev-alpha"), cfg(DOMAIN, "node-1"));
    let push = push_session(ar, aw, &EmptySource, &pushing);
    let recv = receive_session(br, bw, &vault, &receiving);
    let (push, recv) = tokio::join!(push, recv);

    let push = push.expect("push session completes");
    let recv = recv.expect("receive session completes");
    assert_eq!(push.segments_offered, 0);
    assert_eq!(recv.segments_offered, 0);
    assert_eq!(recv.segments_completed, 0);
    assert_eq!(recv.blocks_landed, 0);
}

#[tokio::test]
async fn a_protocol_version_mismatch_is_refused_by_both_halves() {
    let ((ar, aw), mut raw) = rig();
    let session =
        tokio::spawn(
            async move { push_session(ar, aw, &EmptySource, &cfg(DOMAIN, "dev-alpha")).await },
        );
    raw.hello(Role::Receive, DOMAIN, PROTO_VERSION + 1)
        .await
        .unwrap();
    let err = session.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("proto"), "{err:#}");
    // And the peer is told why, rather than seeing a bare hangup.
    let told = raw.recv().await.expect("hello");
    assert!(matches!(told, Frame::Hello(_)));
    match raw.recv().await.expect("refusal") {
        Frame::Error(e) => assert_eq!(e.code, "proto-version"),
        other => panic!("expected an error frame, got {}", other.variant()),
    }
}

/// A session addresses exactly one domain (D29's hub-only boundary). Two
/// sides that disagree about which one have nothing to say.
#[tokio::test]
async fn a_domain_mismatch_is_refused() {
    let (a, b) = tokio::io::duplex(64 * 1024);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let (_dir, vault) = vault();

    let pushing = cfg("example.com", "dev-alpha");
    let receiving = cfg("someone-else.example", "node-1");
    let push = push_session(ar, aw, &EmptySource, &pushing);
    let recv = receive_session(br, bw, &vault, &receiving);
    let (push, recv) = tokio::join!(push, recv);

    assert!(push.is_err() || recv.is_err(), "one side must refuse");
    let mut text = String::new();
    if let Err(e) = &push {
        text.push_str(&format!("{e:#} "));
    }
    if let Err(e) = &recv {
        text.push_str(&format!("{e:#}"));
    }
    assert!(text.contains("domain"), "{text}");
}

#[tokio::test]
async fn two_pushers_refuse_each_other() {
    let ((ar, aw), mut raw) = rig();
    let session =
        tokio::spawn(
            async move { push_session(ar, aw, &EmptySource, &cfg(DOMAIN, "dev-alpha")).await },
        );
    raw.hello(Role::Push, DOMAIN, PROTO_VERSION).await.unwrap();
    let err = session.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("role"), "{err:#}");
}

#[tokio::test]
async fn a_session_that_does_not_open_with_a_hello_is_refused() {
    let ((ar, aw), mut raw) = rig();
    let (_dir, vault) = vault();
    let session =
        tokio::spawn(async move { receive_session(ar, aw, &vault, &cfg(DOMAIN, "node-1")).await });
    raw.send(Frame::Done).await.unwrap();
    let err = session.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("hello"), "{err:#}");
}

/// The device id in a want becomes a directory name on the serving side.
/// It is checked at the protocol edge, before any implementation of the
/// source trait gets a chance to join it onto a path.
#[tokio::test]
async fn a_want_for_a_traversal_device_id_is_refused() {
    let ((ar, aw), mut raw) = rig();
    let session =
        tokio::spawn(
            async move { push_session(ar, aw, &EmptySource, &cfg(DOMAIN, "dev-alpha")).await },
        );
    raw.hello(Role::Receive, DOMAIN, PROTO_VERSION)
        .await
        .unwrap();
    // hello, heads, done — the offer from an empty source.
    for _ in 0..3 {
        raw.recv().await.expect("offer frame");
    }
    raw.send(Frame::WantSegment(WantSegment {
        device: "../../../etc".to_string(),
        start_seq: 1,
        from_offset: 0,
    }))
    .await
    .unwrap();

    match raw.recv().await.expect("refusal") {
        Frame::Error(e) => {
            assert_eq!(e.code, "refused");
            assert!(e.message.contains("allowlist"), "{}", e.message);
        }
        other => panic!("expected a refusal, got {}", other.variant()),
    }
    let err = session.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("allowlist"), "{err:#}");
}

/// A segment answer must answer the want that was asked. Otherwise a peer
/// could file bytes against a segment the receiver never requested.
#[tokio::test]
async fn a_segment_answer_that_does_not_echo_the_want_is_refused() {
    let ((ar, aw), mut raw) = rig();
    let (_dir, vault) = vault();
    let session =
        tokio::spawn(async move { receive_session(ar, aw, &vault, &cfg(DOMAIN, "node-1")).await });

    raw.hello(Role::Push, DOMAIN, PROTO_VERSION).await.unwrap();
    raw.recv().await.expect("hello back");
    raw.send(Frame::Heads(HeadsSnapshot {
        segments: vec![hive_core::oplog::SegmentInfo {
            device: "dev-alpha".to_string(),
            start_seq: 1,
            len: 4096,
            sealed: true,
            file_hash: [0u8; 32],
        }],
    }))
    .await
    .unwrap();
    raw.send(Frame::Done).await.unwrap();

    match raw.recv().await.expect("a want") {
        Frame::WantSegment(w) => assert_eq!(w.from_offset, 0),
        other => panic!("expected want_segment, got {}", other.variant()),
    }
    // Answer for a different segment entirely.
    raw.send(Frame::Segment(Segment {
        device: "dev-beta".to_string(),
        start_seq: 9,
        from_offset: 0,
        len: 8,
    }))
    .await
    .unwrap();
    raw.send_payload(b"12345678").await.unwrap();

    let err = session.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("was answered"), "{err:#}");
}

/// A pusher may only send blocks that were asked for — the receiver's
/// storage is not a place a peer gets to write at will.
#[tokio::test]
async fn an_unrequested_block_is_refused() {
    let ((ar, aw), mut raw) = rig();
    let (dir, vault) = vault();
    let probe = DirVault::open(dir.path()).unwrap();
    let session =
        tokio::spawn(async move { receive_session(ar, aw, &vault, &cfg(DOMAIN, "node-1")).await });

    let offered = *blake3::hash(b"offered block").as_bytes();
    raw.hello(Role::Push, DOMAIN, PROTO_VERSION).await.unwrap();
    raw.recv().await.expect("hello back");
    raw.send(Frame::Heads(HeadsSnapshot::default()))
        .await
        .unwrap();
    raw.send(Frame::HaveBlocks(BlockIds { ids: vec![offered] }))
        .await
        .unwrap();
    raw.send(Frame::Done).await.unwrap();
    match raw.recv().await.expect("a want") {
        Frame::WantBlocks(w) => assert_eq!(w.ids, vec![offered]),
        other => panic!("expected want_blocks, got {}", other.variant()),
    }

    let smuggled = b"a block nobody asked for".to_vec();
    raw.send(Frame::PutBlock(PutBlock {
        id: *blake3::hash(&smuggled).as_bytes(),
        len: smuggled.len() as u64,
    }))
    .await
    .unwrap();
    raw.send_payload(&smuggled).await.unwrap();

    let err = session.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("not requested"), "{err:#}");
    assert!(
        std::fs::read_dir(probe.root().join("blocks"))
            .unwrap()
            .next()
            .is_none(),
        "a refused session must land nothing"
    );
}

/// Ids are the address: bytes that do not hash to the id they were sent
/// under are refused on the way in, which is the only integrity check a
/// keyless holder has.
#[tokio::test]
async fn a_block_whose_bytes_do_not_match_its_id_is_refused() {
    let ((ar, aw), mut raw) = rig();
    let (dir, vault) = vault();
    let probe = DirVault::open(dir.path()).unwrap();
    let session =
        tokio::spawn(async move { receive_session(ar, aw, &vault, &cfg(DOMAIN, "node-1")).await });

    let id = *blake3::hash(b"the honest bytes").as_bytes();
    raw.hello(Role::Push, DOMAIN, PROTO_VERSION).await.unwrap();
    raw.recv().await.expect("hello back");
    raw.send(Frame::Heads(HeadsSnapshot::default()))
        .await
        .unwrap();
    raw.send(Frame::HaveBlocks(BlockIds { ids: vec![id] }))
        .await
        .unwrap();
    raw.send(Frame::Done).await.unwrap();
    raw.recv().await.expect("a want");

    let lie = b"different bytes entirely".to_vec();
    raw.send(Frame::PutBlock(PutBlock {
        id,
        len: lie.len() as u64,
    }))
    .await
    .unwrap();
    raw.send_payload(&lie).await.unwrap();

    let err = session.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("misaddressed"), "{err:#}");
    assert!(std::fs::read_dir(probe.root().join("blocks"))
        .unwrap()
        .next()
        .is_none());
}

/// A payload bigger than its cap is refused before it is read, so a peer
/// cannot make a receiver allocate by claiming.
#[tokio::test]
async fn an_over_cap_payload_claim_is_refused_before_it_is_read() {
    let ((ar, aw), mut raw) = rig();
    let (_dir, vault) = vault();
    let session =
        tokio::spawn(async move { receive_session(ar, aw, &vault, &cfg(DOMAIN, "node-1")).await });

    raw.hello(Role::Push, DOMAIN, PROTO_VERSION).await.unwrap();
    raw.recv().await.expect("hello back");
    raw.send(Frame::Heads(HeadsSnapshot {
        segments: vec![hive_core::oplog::SegmentInfo {
            device: "dev-alpha".to_string(),
            start_seq: 1,
            len: 4096,
            sealed: true,
            file_hash: [0u8; 32],
        }],
    }))
    .await
    .unwrap();
    raw.send(Frame::Done).await.unwrap();
    raw.recv().await.expect("a want");
    raw.send(Frame::Segment(Segment {
        device: "dev-alpha".to_string(),
        start_seq: 1,
        from_offset: 0,
        len: MAX_SEGMENT_CHUNK * 64,
    }))
    .await
    .unwrap();

    let err = session.await.unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("refused unread"), "{err:#}");
}

/// The reader half of the same rule, exercised directly: a session that is
/// handed a payload it never agreed to read still refuses by cap, not by
/// running out of memory.
#[tokio::test]
async fn the_payload_cap_is_the_same_number_on_both_sides() {
    let mut empty: &[u8] = &[];
    assert!(
        read_payload(&mut empty, MAX_SEGMENT_CHUNK + 1, MAX_SEGMENT_CHUNK)
            .await
            .is_err()
    );
}
