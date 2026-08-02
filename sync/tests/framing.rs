// The wire, covered the way a churning wire is supposed to be covered
// (PLAN-v2.1 PR 4.4; docs/TESTING-STRATEGY.md §3.4): ROUND-TRIP over every
// frame plus HOSTILE-DECODE proptests, and deliberately NO byte goldens.
//
// That exclusion is a decision, not an omission. GOLDEN-BYTES is reserved for
// artifacts carrying a freeze promise — the record envelope, control-record
// payloads, wrap formats, key derivations, segment headers — and the
// replication wire keeps moving through PRs 4.4-4.16. Freezing it early would
// either dilute what "frozen" means or force a fixture churn per PR. Compat
// fixtures get cut once, at the v0.8.0 tag that ships proto v1.
//
// What stands in for goldens until then:
//
//   * a corpus with one frame of EVERY variant, asserted complete against
//     `frame::VARIANTS` — the `kind::ALL` discipline from core/tests/oplog.rs,
//     so a new frame cannot ship without coverage here;
//   * round-trips through both the value codec and the framed byte stream;
//   * proptests (fixed seeds, bounded cases, normal CI) proving the decoders
//     answer Err and never panic on arbitrary, mutated, and truncated input,
//     and that every peer-declared length is refused against its cap BEFORE
//     it sizes an allocation.

use hive_core::oplog::{HeadsSnapshot, SegmentInfo};
use hive_sync::frame::{
    self, read_frame, read_frame_capped, read_payload, write_frame, write_payload, BlockIds,
    BlockLanded, EnrollGrant, EnrollRequest, Frame, Hello, ProtoError, PutBlock, Role, Segment,
    SegmentLanded, WantSegment, MAX_BLOCK_BYTES, MAX_BLOCK_IDS_PER_FRAME, MAX_CONTROL_FRAME,
    MAX_OPENING_FRAME, MAX_SEGMENT_CHUNK, PROTO_VERSION,
};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

/// Deterministic runner: per-test fixed seed, bounded cases, no persistence
/// files — a failure reproduces from the source alone (the idiom in
/// core/tests/hostile_decode.rs).
fn runner(seed_tag: u8) -> TestRunner {
    let mut seed = [7u8; 32];
    seed[0] = seed_tag;
    TestRunner::new_with_rng(
        Config {
            cases: 512,
            failure_persistence: None,
            ..Config::default()
        },
        TestRng::from_seed(RngAlgorithm::ChaCha, &seed),
    )
}

fn seg(device: &str, start_seq: u64, len: u64, sealed: bool) -> SegmentInfo {
    SegmentInfo {
        device: device.to_string(),
        start_seq,
        len,
        sealed,
        file_hash: [start_seq as u8; 32],
    }
}

/// One frame of every variant. Values are deliberately awkward (empty lists,
/// a multi-batch id list, non-ASCII text) so a round-trip failure shows up
/// here rather than in a scenario.
fn corpus() -> Vec<Frame> {
    vec![
        Frame::Hello(Hello {
            proto: PROTO_VERSION,
            domain: "example.com".to_string(),
            peer: "dev-æ-01".to_string(),
            role: Role::Push,
        }),
        Frame::Enroll(EnrollRequest {
            proto: PROTO_VERSION,
            code: "K4RT9X2MABCDEFGHIJKLM".to_string(),
            device: "dev-æ-01".to_string(),
            ed25519_pk: [0x11; 32],
            x25519_pk: [0x22; 32],
        }),
        Frame::Enrolled(EnrollGrant {
            domain: "example.com".to_string(),
            node_ed25519_pk: [0x33; 32],
            epoch: u64::MAX,
        }),
        Frame::Heads(HeadsSnapshot {
            segments: vec![
                seg("dev-alpha", 1, 8 * 1024 * 1024, true),
                seg("dev-alpha", 4096, 512, false),
                seg("dev-beta", 1, 0, false),
            ],
        }),
        Frame::HaveBlocks(BlockIds {
            ids: (0u8..64).map(|i| [i; 32]).collect(),
        }),
        Frame::Done,
        Frame::WantSegment(WantSegment {
            device: "dev-alpha".to_string(),
            start_seq: 4096,
            from_offset: u64::MAX / 2,
        }),
        Frame::Segment(Segment {
            device: "dev-alpha".to_string(),
            start_seq: 4096,
            from_offset: 0,
            len: MAX_SEGMENT_CHUNK,
        }),
        Frame::SegmentLanded(SegmentLanded {
            device: "dev-alpha".to_string(),
            start_seq: 4096,
            landed: 1234,
        }),
        Frame::WantBlocks(BlockIds { ids: Vec::new() }),
        Frame::PutBlock(PutBlock {
            id: [0xAB; 32],
            len: MAX_BLOCK_BYTES,
        }),
        Frame::BlockLanded(BlockLanded { id: [0x00; 32] }),
        Frame::Error(ProtoError {
            code: frame::err_code::REFUSED.to_string(),
            message: "nothing left to serve".to_string(),
        }),
    ]
}

/// The completeness gate: every declared variant appears in the corpus, and
/// the corpus invents nothing. Adding a frame breaks `Frame::variant` at
/// compile time and then breaks this test.
#[test]
fn the_corpus_covers_every_frame_variant() {
    let mut names: Vec<&str> = corpus().iter().map(Frame::variant).collect();
    names.sort_unstable();
    names.dedup();
    let mut declared: Vec<&str> = frame::VARIANTS.to_vec();
    declared.sort_unstable();
    assert_eq!(
        names, declared,
        "every frame needs a round-trip and hostile-decode case"
    );
}

#[test]
fn every_frame_round_trips_through_cbor() {
    for frame in corpus() {
        let bytes = frame.to_cbor_bytes().expect("encodes");
        let back = Frame::from_cbor_bytes(&bytes)
            .unwrap_or_else(|e| panic!("{} frame failed to decode: {e:#}", frame.variant()));
        assert_eq!(back, frame, "{} frame changed in flight", frame.variant());
    }
}

#[tokio::test]
async fn every_frame_round_trips_through_the_framed_stream() {
    let (mut a, mut b) = tokio::io::duplex(256 * 1024);
    let sent = corpus();
    for frame in &sent {
        write_frame(&mut a, frame).await.expect("writes");
    }
    for want in &sent {
        let got = read_frame(&mut b).await.expect("reads");
        assert_eq!(&got, want);
    }
}

/// The framing itself: a u32 LE length prefix and a CBOR body, in that order.
/// Structural, not byte-golden — this pins the shape a decoder must assume
/// without freezing the payload.
#[tokio::test]
async fn a_frame_is_a_little_endian_length_then_its_body() {
    let frame = Frame::Done;
    let body = frame.to_cbor_bytes().unwrap();
    let (mut a, mut b) = tokio::io::duplex(4096);
    write_frame(&mut a, &frame).await.unwrap();

    use tokio::io::AsyncReadExt;
    let mut prefix = [0u8; 4];
    b.read_exact(&mut prefix).await.unwrap();
    assert_eq!(u32::from_le_bytes(prefix) as usize, body.len());
    let mut got = vec![0u8; body.len()];
    b.read_exact(&mut got).await.unwrap();
    assert_eq!(got, body);
    assert_eq!(Frame::from_cbor_bytes(&got).unwrap(), frame);
}

/// Payload streams are raw: what a `Segment` frame declares is exactly what
/// follows, byte for byte, with no wrapping. Verbatim transfer is the whole
/// point of the segment primitive (D29).
#[tokio::test]
async fn a_payload_stream_carries_its_bytes_verbatim() {
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let (mut a, mut b) = tokio::io::duplex(64 * 1024);
    let frame = Frame::Segment(Segment {
        device: "dev-alpha".to_string(),
        start_seq: 1,
        from_offset: 0,
        len: payload.len() as u64,
    });
    write_frame(&mut a, &frame).await.unwrap();
    write_payload(&mut a, &payload).await.unwrap();

    let got = read_frame(&mut b).await.unwrap();
    assert_eq!(got.payload_len(), payload.len() as u64);
    let bytes = read_payload(&mut b, got.payload_len(), MAX_SEGMENT_CHUNK)
        .await
        .unwrap();
    assert_eq!(bytes, payload);
}

// ── caps: every declared length is checked before it allocates ──────────────

#[tokio::test]
async fn a_hostile_length_prefix_is_refused_unread() {
    // 4 GiB claimed, four bytes delivered. The refusal must come from the cap
    // check, not from waiting for (or reserving) the body.
    let mut stream = &u32::MAX.to_le_bytes()[..];
    let err = read_frame(&mut stream).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("refused unread"),
        "want a cap refusal, got: {err:#}"
    );

    let mut zero = &0u32.to_le_bytes()[..];
    assert!(
        read_frame(&mut zero).await.is_err(),
        "a zero-length control frame is not a frame"
    );
}

#[tokio::test]
async fn a_payload_over_its_cap_is_refused_unread() {
    let mut empty: &[u8] = &[];
    let err = read_payload(&mut empty, MAX_SEGMENT_CHUNK + 1, MAX_SEGMENT_CHUNK)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("refused unread"), "{err:#}");

    let err = read_payload(&mut empty, u64::MAX, MAX_BLOCK_BYTES)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("refused unread"), "{err:#}");
}

#[tokio::test]
async fn a_short_payload_stream_is_an_error_not_a_short_read() {
    let mut stream = &b"only-ten-b"[..];
    assert!(
        read_payload(&mut stream, 4096, MAX_SEGMENT_CHUNK)
            .await
            .is_err(),
        "a truncated payload must never land as a shorter one"
    );
}

#[tokio::test]
async fn a_control_frame_over_the_cap_is_refused_at_the_writer() {
    // The only frame that grows with the store. ~17k segments is past 1 MiB.
    let segments: Vec<SegmentInfo> = (0..20_000u64)
        .map(|i| seg("dev-enormous-log", i, i, true))
        .collect();
    let frame = Frame::Heads(HeadsSnapshot { segments });
    let mut sink: Vec<u8> = Vec::new();
    let err = write_frame(&mut sink, &frame).await.unwrap_err();
    assert!(
        format!("{err:#}").contains("control cap"),
        "want a cap refusal, got: {err:#}"
    );
    assert!(sink.is_empty(), "nothing may go out before the cap check");
}

/// A batch at exactly the session's batching size — the wire's real worst
/// case for a control frame that is not heads.
///
/// Regression: ciborium's borrowing byte-string path stops at its 4 KiB
/// scratch buffer and answers anything longer with a bare type error, so a
/// full id batch failed to decode until the id-list adapter moved to the
/// buffering path. Small batches (the corpus's 64 ids) never caught it.
#[test]
fn a_full_id_batch_round_trips() {
    let ids: Vec<[u8; 32]> = (0..MAX_BLOCK_IDS_PER_FRAME)
        .map(|i| {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&(i as u64).to_le_bytes());
            id
        })
        .collect();
    let frame = Frame::HaveBlocks(BlockIds { ids });
    let bytes = frame.to_cbor_bytes().unwrap();
    assert!(
        bytes.len() > 4096,
        "this test is only meaningful past ciborium's scratch buffer"
    );
    assert!((bytes.len() as u32) < MAX_CONTROL_FRAME);
    assert_eq!(Frame::from_cbor_bytes(&bytes).unwrap(), frame);
}

/// The id-list decoder owns two arithmetic invariants, and both are the
/// receiver's protection: the session batches at the cap, but a peer is not
/// obliged to.
#[test]
fn a_block_id_list_must_be_whole_ids_within_the_cap() {
    let over = BlockIds {
        ids: vec![[9u8; 32]; MAX_BLOCK_IDS_PER_FRAME + 1],
    };
    let bytes = Frame::HaveBlocks(over).to_cbor_bytes().unwrap();
    let err = Frame::from_cbor_bytes(&bytes).unwrap_err();
    assert!(format!("{err:#}").contains("over the"), "{err:#}");

    // A ragged list: 32 ids' worth of bytes plus one. Built by hand, since a
    // well-formed encoder cannot produce it.
    let ragged = ciborium::Value::Map(vec![(
        ciborium::Value::Text("have_blocks".into()),
        ciborium::Value::Map(vec![(
            ciborium::Value::Text("ids".into()),
            ciborium::Value::Bytes(vec![0u8; 32 * 3 + 1]),
        )]),
    )]);
    let mut raw = Vec::new();
    ciborium::into_writer(&ragged, &mut raw).unwrap();
    let err = Frame::from_cbor_bytes(&raw).unwrap_err();
    assert!(format!("{err:#}").contains("multiple of 32"), "{err:#}");
}

/// Strictness is inherited from the frozen envelope's posture: an unknown key
/// or an unknown variant is refused, never ignored, so a peer cannot smuggle
/// meaning past a decoder that "just skips what it does not know".
#[test]
fn unknown_variants_and_unknown_fields_are_refused() {
    let unknown_variant = ciborium::Value::Map(vec![(
        ciborium::Value::Text("forget".into()),
        ciborium::Value::Map(Vec::new()),
    )]);
    let mut raw = Vec::new();
    ciborium::into_writer(&unknown_variant, &mut raw).unwrap();
    assert!(
        Frame::from_cbor_bytes(&raw).is_err(),
        "a frame from a later protocol is refused, not guessed at"
    );

    let extra_field = ciborium::Value::Map(vec![(
        ciborium::Value::Text("want_segment".into()),
        ciborium::Value::Map(vec![
            (
                ciborium::Value::Text("device".into()),
                ciborium::Value::Text("dev-alpha".into()),
            ),
            (
                ciborium::Value::Text("start_seq".into()),
                ciborium::Value::Integer(1.into()),
            ),
            (
                ciborium::Value::Text("from_offset".into()),
                ciborium::Value::Integer(0.into()),
            ),
            (
                ciborium::Value::Text("priority".into()),
                ciborium::Value::Integer(9.into()),
            ),
        ]),
    )]);
    let mut raw = Vec::new();
    ciborium::into_writer(&extra_field, &mut raw).unwrap();
    assert!(
        Frame::from_cbor_bytes(&raw).is_err(),
        "deny_unknown_fields is the contract"
    );
}

// ── HOSTILE-DECODE ─────────────────────────────────────────────────────────

#[test]
fn arbitrary_bytes_decode_to_err_or_ok_never_panic() {
    runner(1)
        .run(&proptest::collection::vec(any::<u8>(), 0..2048), |bytes| {
            let _ = Frame::from_cbor_bytes(&bytes);
            Ok(())
        })
        .unwrap();
}

#[test]
fn single_byte_mutations_of_every_frame_never_panic() {
    for frame in corpus() {
        let base = frame.to_cbor_bytes().unwrap();
        let name = frame.variant();
        runner(2)
            .run(&(0..base.len(), any::<u8>()), |(pos, byte)| {
                let mut bytes = base.clone();
                bytes[pos] = byte;
                // Some mutations still decode (a flipped character in a
                // device id); most Err. Panicking is the only failure.
                let _ = Frame::from_cbor_bytes(&bytes);
                Ok(())
            })
            .unwrap_or_else(|e| panic!("{name} frame: {e}"));
    }
}

#[test]
fn every_strict_prefix_of_every_frame_errors() {
    for frame in corpus() {
        let base = frame.to_cbor_bytes().unwrap();
        for len in 0..base.len() {
            assert!(
                Frame::from_cbor_bytes(&base[..len]).is_err(),
                "a {}-byte prefix of a {} frame must not decode",
                len,
                frame.variant()
            );
        }
    }
}

#[test]
fn arbitrary_framed_bytes_never_panic_the_reader() {
    // The length prefix is attacker-controlled too, so the property covers
    // the framing layer and not just the CBOR body.
    runner(3)
        .run(&proptest::collection::vec(any::<u8>(), 0..1024), |bytes| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let mut stream = bytes.as_slice();
                let _ = read_frame(&mut stream).await;
            });
            Ok(())
        })
        .unwrap();
}

#[test]
fn any_declared_length_is_bounded_by_its_cap() {
    runner(4)
        .run(&(any::<u64>(), 1u64..=MAX_BLOCK_BYTES), |(len, cap)| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            rt.block_on(async {
                let mut empty: &[u8] = &[];
                let got = read_payload(&mut empty, len, cap).await;
                // Over the cap: refused. Within it: refused too, because the
                // stream is empty — but never accepted, and never sized from
                // the claim.
                prop_assert!(got.is_err());
                Ok(())
            })?;
            Ok(())
        })
        .unwrap();
}

/// The opening frame — the one a listener reads before it has authorized
/// anything — is bounded far tighter than the session cap (PR 4.7). A caller
/// may narrow the cap and may never widen it.
#[tokio::test]
async fn the_opening_read_is_capped_tighter_than_a_session_frame() {
    const { assert!(MAX_OPENING_FRAME < MAX_CONTROL_FRAME) };

    // Both frames that may legally open a connection fit, with room to spare.
    for frame in corpus()
        .into_iter()
        .filter(|f| matches!(f, Frame::Hello(_) | Frame::Enroll(_)))
    {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        write_frame(&mut a, &frame).await.unwrap();
        assert_eq!(
            read_frame_capped(&mut b, MAX_OPENING_FRAME).await.unwrap(),
            frame
        );
    }

    // A heads snapshot is a legitimate session frame and an illegitimate
    // opening one: over the opening cap, it is refused before it allocates.
    let heads = Frame::Heads(HeadsSnapshot {
        segments: (0..1000u64).map(|i| seg("dev-alpha", i, i, true)).collect(),
    });
    let (mut a, mut b) = tokio::io::duplex(256 * 1024);
    write_frame(&mut a, &heads).await.unwrap();
    let err = read_frame_capped(&mut b, MAX_OPENING_FRAME)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("refused unread"), "{err:#}");

    // And the cap only ever narrows: asking for more than the protocol's own
    // ceiling gets the ceiling.
    let mut over = &u32::MAX.to_le_bytes()[..];
    let err = read_frame_capped(&mut over, u32::MAX).await.unwrap_err();
    assert!(
        format!("{err:#}").contains(&MAX_CONTROL_FRAME.to_string()),
        "{err:#}"
    );
}

/// A frame body that decodes must still fit the framing: the reader's cap and
/// the writer's cap are the same number, so a frame either survives the round
/// trip whole or is refused at both ends.
#[test]
fn the_reader_and_writer_agree_on_the_control_cap() {
    assert_eq!(MAX_CONTROL_FRAME, 1024 * 1024);
    assert!(MAX_SEGMENT_CHUNK <= MAX_CONTROL_FRAME as u64);
    assert!(
        (MAX_BLOCK_IDS_PER_FRAME * 32) as u32 + 64 < MAX_CONTROL_FRAME,
        "a full id batch plus CBOR overhead has to fit in one control frame"
    );
}
