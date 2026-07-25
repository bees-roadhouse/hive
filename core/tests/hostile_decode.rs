// HOSTILE-DECODE over the network-facing decoders (PLAN-v2.1 PR 4.1/4.3;
// TESTING-STRATEGY §2): `Record::from_cbor_bytes` (the frozen envelope) and
// the PR 4.3 keyless segment parsers — `oplog::parse_header` and
// `oplog::walk_segment`, which a blind node points straight at foreign bytes
// with no key and no prior trust. The convention demands this coverage BEFORE
// each surface widens. Invariants: Err (or a truthful partial walk), never
// panic; hostile length claims fail fast instead of driving allocation;
// unknown/missing map keys are refused (the envelope's own strictness
// promise); a truncated segment is always a PREFIX of the complete one.
//
// Proptests run through a FIXED ChaCha seed and a bounded case count (the
// convention's "fixed seeds, bounded, in normal CI" rule) — failures
// reproduce exactly from the source, so no persistence files are written.

use hive_core::keys::{MemoryKeySource, WRAPPED_KEY_LEN};
use hive_core::oplog::{self, kind, LogWriter, Record, ENVELOPE_VERSION, SEGMENT_MAGIC};
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};

/// Deterministic runner: per-test fixed seed, bounded cases, no persistence.
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

/// One valid record's frozen bytes — the mutation/truncation base.
fn valid_record_bytes() -> Vec<u8> {
    let payload = ciborium::Value::Map(vec![(
        ciborium::Value::Text("body".into()),
        ciborium::Value::Text("hostile-decode base record".into()),
    )]);
    let mut rec = Record::new(
        "dev-hostile",
        3,
        9,
        "2026-07-20T12:00:00.000Z",
        "nate",
        kind::JOURNAL_APPEND,
        payload,
    );
    rec.prev = [4u8; 32];
    rec.to_cbor_bytes().unwrap()
}

#[test]
fn arbitrary_bytes_decode_to_err_or_ok_never_panic() {
    runner(1)
        .run(&proptest::collection::vec(any::<u8>(), 0..2048), |bytes| {
            let _ = Record::from_cbor_bytes(&bytes);
            Ok(())
        })
        .unwrap();
}

#[test]
fn single_byte_mutations_of_a_valid_record_never_panic() {
    let base = valid_record_bytes();
    runner(2)
        .run(&(0..base.len(), any::<u8>()), |(pos, byte)| {
            let mut bytes = base.clone();
            bytes[pos] = byte;
            // Some mutations still decode (a flipped payload char), most
            // Err — either is fine; panicking is the only failure.
            let _ = Record::from_cbor_bytes(&bytes);
            Ok(())
        })
        .unwrap();
}

#[test]
fn every_strict_prefix_errors_never_panics() {
    let base = valid_record_bytes();
    for len in 0..base.len() {
        assert!(
            Record::from_cbor_bytes(&base[..len]).is_err(),
            "a strict prefix (len {len}) must not decode to a whole record"
        );
    }
    // The base itself is honest.
    assert!(Record::from_cbor_bytes(&base).is_ok());
}

/// Length-claim lies: CBOR headers claiming enormous strings/collections
/// with no bytes behind them must Err fast — never allocate-the-claim, never
/// panic. (ciborium reads incrementally rather than trusting the header; this
/// pins that property for the decoder we ship.)
#[test]
fn hostile_length_claims_error_fast() {
    for bytes in [
        vec![0x5A, 0xFF, 0xFF, 0xFF, 0xFF], // bstr claiming 4 GiB
        vec![
            0x5B, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // bstr claiming 2^64-1
        ],
        vec![
            0x7B, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // text claiming 2^64-1
        ],
        vec![
            0x9B, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // array claiming 2^64-1
        ],
        vec![
            0xBB, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, // map claiming 2^64-1
        ],
    ] {
        assert!(Record::from_cbor_bytes(&bytes).is_err());
    }

    // The same lie nested inside an otherwise-valid record: swap `prev`'s
    // 32-byte bstr header (0x58 0x20) for a 4 GiB claim.
    let base = valid_record_bytes();
    let pos = base
        .windows(2)
        .position(|w| w == [0x58, 0x20])
        .expect("the prev bstr header is in the encoding");
    let mut bytes = Vec::with_capacity(base.len() + 3);
    bytes.extend_from_slice(&base[..pos]);
    bytes.extend_from_slice(&[0x5A, 0xFF, 0xFF, 0xFF, 0xFF]);
    bytes.extend_from_slice(&base[pos + 2..]);
    assert!(Record::from_cbor_bytes(&bytes).is_err());
}

/// The decoder's strictness promise (oplog/mod.rs: "a record this version
/// cannot fully understand is a record it must not half-understand"):
/// unknown map keys are refused, and so is a map missing a required key.
#[test]
fn unknown_and_missing_map_keys_are_refused() {
    use ciborium::Value as Cb;
    let t = |s: &str| Cb::Text(s.to_string());
    let fields: Vec<(Cb, Cb)> = vec![
        (t("v"), Cb::from(1u8)),
        (t("device"), t("dev-strict")),
        (t("seq"), Cb::from(1u64)),
        (t("lc"), Cb::from(1u64)),
        (t("ts"), t("2026-07-20T12:00:00.000Z")),
        (t("actor"), t("nate")),
        (t("kind"), t(kind::ALIAS)),
        (t("prev"), Cb::Bytes(vec![0u8; 32])),
        (t("payload"), Cb::Null),
    ];

    let encode = |entries: Vec<(Cb, Cb)>| {
        let mut out = Vec::new();
        ciborium::into_writer(&Cb::Map(entries), &mut out).unwrap();
        out
    };

    // The nine exact keys decode.
    assert!(Record::from_cbor_bytes(&encode(fields.clone())).is_ok());

    // A tenth, unknown key is refused (deny_unknown_fields).
    let mut extra = fields.clone();
    extra.push((t("shiny_new_field"), Cb::Null));
    assert!(Record::from_cbor_bytes(&encode(extra)).is_err());

    // Any missing required key is refused.
    for skip in 0..fields.len() {
        let mut short = fields.clone();
        short.remove(skip);
        assert!(
            Record::from_cbor_bytes(&encode(short)).is_err(),
            "map missing key #{skip} must not decode"
        );
    }
}

// ── The keyless segment parsers (PR 4.3) ────────────────────────────────────

/// A real, writer-produced segment: the truncation/mutation base for the
/// walker properties below (frames whose lengths and hashes are honest).
fn valid_segment_bytes() -> Vec<u8> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let keys = MemoryKeySource([7u8; 32]);
    let mut w = LogWriter::open(tmp.path(), "dev-hostile", &keys).expect("writer");
    let records: Vec<Record> = (1..=4)
        .map(|seq| {
            Record::new(
                "dev-hostile",
                seq,
                seq,
                "2026-07-20T12:00:00.000Z",
                "nate",
                kind::JOURNAL_APPEND,
                ciborium::Value::Text(format!("hostile-decode segment record {seq}")),
            )
        })
        .collect();
    w.append_batch(&records).expect("append");
    drop(w);
    std::fs::read(oplog::segment_path(tmp.path(), "dev-hostile", 1)).expect("read segment")
}

/// A header assembled by hand, so the fields can lie however a hostile peer
/// wants them to (the writer would never emit these).
fn crafted_header(
    device: &[u8],
    declared_dlen: u16,
    declared_wlen: u16,
    wrapped: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&SEGMENT_MAGIC);
    out.push(ENVELOPE_VERSION);
    out.extend_from_slice(&declared_dlen.to_le_bytes());
    out.extend_from_slice(device);
    out.extend_from_slice(&7u64.to_le_bytes());
    out.extend_from_slice(&declared_wlen.to_le_bytes());
    out.extend_from_slice(&vec![0xAB; wrapped]);
    out
}

#[test]
fn arbitrary_bytes_never_panic_the_keyless_parsers() {
    runner(4)
        .run(&proptest::collection::vec(any::<u8>(), 0..2048), |bytes| {
            let _ = oplog::parse_header(&bytes);
            let _ = oplog::walk_segment(&bytes);
            Ok(())
        })
        .unwrap();
}

#[test]
fn single_byte_mutations_of_a_real_segment_never_panic() {
    let base = valid_segment_bytes();
    runner(5)
        .run(&(0..base.len(), any::<u8>()), |(pos, byte)| {
            let mut bytes = base.clone();
            bytes[pos] = byte;
            // Some mutations still walk (a flipped ciphertext byte is opaque
            // to a keyless reader by design — the AEAD tag is the keyed
            // side's job); the only failure mode is a panic.
            let _ = oplog::parse_header(&bytes);
            let _ = oplog::walk_segment(&bytes);
            Ok(())
        })
        .unwrap();
}

/// The growing-tail invariant, stated as a property: whatever prefix of a
/// segment has arrived, the walk of it is a PREFIX of the complete walk —
/// same frames, same offsets, same hashes, just fewer — and it never claims
/// bytes the prefix does not contain. This is what lets a receiver land whole
/// frames only and re-request from `whole_end`.
#[test]
fn every_prefix_of_a_segment_walks_to_a_prefix_of_the_whole() {
    let base = valid_segment_bytes();
    let whole = oplog::walk_segment(&base).unwrap();
    assert!(whole.is_whole() && whole.frames.len() == 4);
    for len in 0..=base.len() {
        let cut = &base[..len];
        match oplog::walk_segment(cut) {
            Ok(walk) => {
                assert_eq!(walk.header, whole.header);
                assert!(walk.frames.len() <= whole.frames.len());
                assert_eq!(walk.frames, whole.frames[..walk.frames.len()].to_vec());
                assert!(walk.whole_end <= len as u64);
                assert_eq!(walk.whole_end + walk.partial_tail, len as u64);
            }
            // Only a truncated HEADER may refuse outright.
            Err(_) => assert!((len as u64) < whole.header.len),
        }
    }
}

/// Length fields are the classic allocate-the-claim trap. Every lie below is
/// refused (header) or stops the walk (frames) against a tiny buffer — the
/// parsers slice a caller-supplied slab and never size an allocation from a
/// declared length, so a 4 GiB claim costs nothing.
#[test]
fn hostile_length_claims_are_refused_not_allocated() {
    // Device length: zero, past the 64-byte cap, and a claim far beyond the
    // buffer (the classic "read 65535 bytes from a 20-byte message").
    assert!(oplog::parse_header(&crafted_header(
        b"",
        0,
        WRAPPED_KEY_LEN as u16,
        WRAPPED_KEY_LEN
    ))
    .is_err());
    assert!(oplog::parse_header(&crafted_header(
        b"dev",
        65,
        WRAPPED_KEY_LEN as u16,
        WRAPPED_KEY_LEN
    ))
    .is_err());
    assert!(
        oplog::parse_header(&crafted_header(
            b"dev",
            60,
            WRAPPED_KEY_LEN as u16,
            WRAPPED_KEY_LEN
        ))
        .is_err(),
        "a device length past the buffer must not be honored"
    );
    // Wrapped-key length: only the frozen 72 is acceptable, present or not.
    assert!(oplog::parse_header(&crafted_header(b"dev", 3, 0, 0)).is_err());
    assert!(oplog::parse_header(&crafted_header(b"dev", 3, 65535, 8)).is_err());
    assert!(oplog::parse_header(&crafted_header(b"dev", 3, WRAPPED_KEY_LEN as u16, 0)).is_err());
    // …and the honest one parses, so the assertions above are about the lies.
    let honest = crafted_header(b"dev", 3, WRAPPED_KEY_LEN as u16, WRAPPED_KEY_LEN);
    assert_eq!(oplog::parse_header(&honest).unwrap().device, "dev");

    // Frame length words: absurd, impossible, and just-past-EOF all end the
    // walk at the last whole frame instead of reserving anything.
    for claim in [u32::MAX, 64 * 1024 * 1024 + 1, 15, 0] {
        let mut bytes = honest.clone();
        bytes.extend_from_slice(&claim.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 24]);
        let walk = oplog::walk_segment(&bytes).unwrap();
        assert!(walk.frames.is_empty(), "frame length claim {claim}");
        assert_eq!(walk.whole_end, walk.header.len);
        assert_eq!(walk.partial_tail, 28);
    }
}

/// The device id becomes a directory name on a blind vault, so the keyless
/// parse applies the writer's allowlist to it — path traversal is refused at
/// the parse, not at the use.
#[test]
fn header_device_ids_outside_the_allowlist_are_refused() {
    for device in [
        &b"../escape"[..],
        &b"a/b"[..],
        &b"."[..],
        &b".."[..],
        &b"nul\0"[..],
        &[0xFFu8, 0xFE][..], // not UTF-8
    ] {
        let bytes = crafted_header(
            device,
            device.len() as u16,
            WRAPPED_KEY_LEN as u16,
            WRAPPED_KEY_LEN,
        );
        assert!(
            oplog::parse_header(&bytes).is_err(),
            "device {device:?} must not parse"
        );
        assert!(oplog::walk_segment(&bytes).is_err());
    }
}

/// The positive property under arbitrary (bounded) content: whatever text,
/// integers, and prev bytes a record carries, encode→decode reproduces it
/// exactly. Content-dependent codec bugs (escaping, integer widths, unicode)
/// have nowhere to hide.
#[test]
fn arbitrary_records_roundtrip_exactly() {
    let text = || proptest::collection::vec(any::<char>(), 0..32).prop_map(String::from_iter);
    let strat = (
        (text(), text(), text(), text()), // device, ts, actor, kind (free strings to the CODEC)
        any::<u64>(),
        any::<u64>(),
        any::<[u8; 32]>(),
        text(), // payload body text
    );
    runner(3)
        .run(
            &strat,
            |((device, ts, actor, kind_s), seq, lc, prev, body)| {
                let mut rec = Record::new(
                    &device,
                    seq,
                    lc,
                    &ts,
                    &actor,
                    &kind_s,
                    ciborium::Value::Text(body),
                );
                rec.prev = prev;
                let bytes = rec
                    .to_cbor_bytes()
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                let back = Record::from_cbor_bytes(&bytes)
                    .map_err(|e| TestCaseError::fail(e.to_string()))?;
                prop_assert_eq!(rec, back);
                Ok(())
            },
        )
        .unwrap();
}
