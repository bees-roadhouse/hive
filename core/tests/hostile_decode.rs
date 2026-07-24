// HOSTILE-DECODE over `Record::from_cbor_bytes` (PLAN-v2.1 PR 4.1;
// TESTING-STRATEGY §2). The frozen envelope decoder is about to face
// network-supplied bytes (foreign segment ingest, PR 4.3+); today its only
// callers hand it AEAD-verified frames, so it has zero adversarial-input
// coverage — the convention demands that coverage BEFORE the surface
// widens. Invariants: Err, never panic; hostile length claims fail fast
// instead of driving allocation; unknown/missing map keys are refused
// (the decoder's own strictness promise).
//
// Proptests run through a FIXED ChaCha seed and a bounded case count (the
// convention's "fixed seeds, bounded, in normal CI" rule) — failures
// reproduce exactly from the source, so no persistence files are written.

use hive_core::oplog::{kind, Record};
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
