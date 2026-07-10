// The append-only op log (PR 1.4, D18): per-device, single-writer, encrypted
// segment files holding immutable CBOR records. This is the source of truth;
// SQLite (PR 1.5/1.6) is a rebuildable projection of it.
//
// THE FORMAT BELOW IS FROZEN. Every byte is specified here; the golden
// fixtures in core/tests/fixtures/oplog/ pin the record encoding, and any
// change to this layout is an envelope-version bump, not an edit.
//
// ── Record envelope (CBOR, via ciborium) ────────────────────────────────────
//
// A record is one CBOR definite-length map with exactly these nine text keys,
// serialized in exactly this order (serde struct field order):
//
//   "v"       u8      envelope version, =1
//   "device"  text    writing device id (allowlist: [A-Za-z0-9._-]{1,64})
//   "seq"     u64     per-device sequence, gapless, first record is 1
//   "lc"      u64     Lamport clock (caller-maintained; not enforced here)
//   "ts"      text    wall time in the store's ISO shape (store/mod.rs):
//                     `%Y-%m-%dT%H:%M:%S%.3fZ`, exactly 24 chars —
//                     lexicographic order == chronological order, load-bearing
//   "actor"   text    author identity (person or AI)
//   "kind"    text    one of the closed `kind::` set below
//   "prev"    bytes   32 bytes, blake3 frame chain (see below)
//   "payload" any     kind-specific CBOR value (by convention a map)
//
// Integers use CBOR's canonical minimal-width encoding (ciborium default);
// `prev` is a CBOR byte string (major type 2, 32 bytes), not an array.
// Payload schemas are NOT frozen here — the fold (PR 1.5) owns their meaning;
// the envelope and its encoding are.
//
// ── Segment files ───────────────────────────────────────────────────────────
//
// Path: `<data_dir>/log/<device>/<start_seq:016x>.seg`, where start_seq is
// the seq of the segment's first record (zero-padded lowercase hex, so the
// lexicographic directory order is the numeric order).
//
// Plaintext header (integers little-endian):
//
//   offset 0        8 bytes   magic "HIVELOG1"
//   offset 8        1 byte    envelope version (=1)
//   offset 9        2 bytes   u16 device id byte length D
//   offset 11       D bytes   device id, UTF-8
//   offset 11+D     8 bytes   u64 start seq
//   offset 19+D     2 bytes   u16 wrapped segment key length W (=72 today)
//   offset 21+D     W bytes   wrapped segment key: keys::wrap_key(master,
//                             segment_key) = nonce(24) ‖ ct(32) ‖ tag(16)
//
// Frames follow immediately, back to back, until EOF:
//
//   offset 0        4 bytes   u32 ciphertext length C
//   offset 4        24 bytes  XChaCha20-Poly1305 nonce
//   offset 28       C bytes   ciphertext = record CBOR bytes + 16-byte tag
//
//   frame_hash = blake3(the 4 + 24 + C frame bytes exactly as written)
//
// Each record's `prev` is the frame_hash of the previous frame in the SAME
// DEVICE LOG — the chain runs across segment boundaries; the device's first
// record carries prev = [0u8; 32]. The chain lives inside the AEAD-protected
// plaintext, so reordering, splicing, or dropping frames is detectable
// without any additional AAD.
//
// Rotation: when a segment reaches SEGMENT_ROTATE_BYTES the writer seals it
// (final fsync, never touched again) and starts the next one. Sealed segments
// are immutable.
//
// ── Cryptography (all derivations frozen) ───────────────────────────────────
//
// One fresh key per segment, derived — not sampled — so this module never
// touches an RNG (see "Determinism" below):
//
//   segment_key = blake3::keyed_hash(
//       blake3::derive_key("hive-oplog-segment-key-v1", master),
//       u16_le(len(device)) ‖ device ‖ u64_le(start_seq))
//
// (device, start_seq) is unique per segment — one writer, gapless seq — so
// every segment gets a distinct key within a master-key domain.
//
// Frames are sealed with XChaCha20-Poly1305 under the segment key, with a
// deterministic SIV-style nonce:
//
//   nonce = blake3::keyed_hash(
//       blake3::derive_key("hive-oplog-frame-nonce-v1", segment_key),
//       record_cbor_bytes)[..24]
//
// Why that is safe: the nonce is a PRF of the exact plaintext, so a
// (segment_key, nonce) pair can only repeat for an identical record — which
// produces the identical ciphertext, never two ciphertexts under one
// keystream. Distinct records collide on 192-bit nonces with negligible
// probability. (Records are never identical in practice anyway: seq differs.)
//
// ── Recovery semantics ──────────────────────────────────────────────────────
//
// `LogWriter::open` is the torn-tail repair path: it walks the tail segment's
// frames and truncates at the first invalid one (a crash mid-append leaves a
// short or garbled tail; nothing after it was ever acknowledged, because
// append_batch only returns after fsync). A tail file that dies inside its
// header is deleted whole. Sealed segments are never repaired.
//
// `LogReader::scan` is the strict verifier: it never repairs; it yields each
// frame and reports the first invalid frame as an error (chain break, auth
// failure, seq gap, torn tail — all distinct messages).
//
// ── Determinism boundary ────────────────────────────────────────────────────
//
// This module and blockstore never read clocks, environment, or randomness;
// ts/seq/lc/actor arrive from callers and key material arrives through the
// keys::KeySource seam. core/tests/determinism.rs greps these sources and
// fails on any clock/RNG/env token. Replaying the same records through a
// writer with the same master key reproduces every byte on disk.
//
// The writer/reader here are deliberately synchronous (std::fs): from PR 1.6
// on they live behind the single writer thread that owns the SQLite
// connection, not on the async runtime.

mod reader;
mod segment;
mod writer;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub use reader::LogReader;
pub use writer::LogWriter;

/// Envelope version written into records and segment headers.
pub const ENVELOPE_VERSION: u8 = 1;

/// Segment file magic, first 8 bytes of every segment.
pub const SEGMENT_MAGIC: [u8; 8] = *b"HIVELOG1";

/// Rotation threshold: once a segment reaches this many bytes it is sealed.
pub const SEGMENT_ROTATE_BYTES: u64 = 8 * 1024 * 1024;

/// Reader sanity bound on a single frame's ciphertext length. A frame length
/// above this is treated as corruption rather than an allocation request.
/// Frozen as part of the format: writers must not produce records whose
/// encoding exceeds this minus the 16-byte tag.
pub const MAX_FRAME_CIPHERTEXT: u32 = 64 * 1024 * 1024;

/// The closed set of record kinds (v1). The writer rejects anything else;
/// widening the set is an explicit format decision, not a drive-by.
pub mod kind {
    pub const JOURNAL_APPEND: &str = "journal.append";
    pub const ENTITY_CREATE: &str = "entity.create";
    pub const ENTITY_UPDATE: &str = "entity.update";
    pub const LINK_ADD: &str = "link.add";
    pub const LINK_REMOVE: &str = "link.remove";
    pub const TOMBSTONE: &str = "tombstone";
    pub const REDACT: &str = "redact";
    pub const CONFIG_SET: &str = "config.set";
    pub const MODULE_DOC: &str = "module.doc";
    pub const CURSOR_SET: &str = "cursor.set";
    pub const ALIAS: &str = "alias";

    /// Every valid kind, in the fixed fixture order.
    pub const ALL: &[&str] = &[
        JOURNAL_APPEND,
        ENTITY_CREATE,
        ENTITY_UPDATE,
        LINK_ADD,
        LINK_REMOVE,
        TOMBSTONE,
        REDACT,
        CONFIG_SET,
        MODULE_DOC,
        CURSOR_SET,
        ALIAS,
    ];
}

/// serde adapter: `[u8; 32]` as a CBOR byte string (major type 2) instead of
/// serde's default array-of-32-integers. Part of the frozen encoding; the
/// blockstore reuses it for hashes inside `BlobRef` and manifests.
pub(crate) mod bytes32 {
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        struct B32;
        impl Visitor<'_> for B32 {
            type Value = [u8; 32];
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a 32-byte string")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                v.try_into().map_err(|_| E::invalid_length(v.len(), &self))
            }
        }
        d.deserialize_bytes(B32)
    }
}

/// serde adapter: `Vec<u8>` as a CBOR byte string (used by the blockstore's
/// `BlobRef.wrapped_key`). Same rationale as `bytes32`.
pub(crate) mod bytesvec {
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(v)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct BV;
        impl Visitor<'_> for BV {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                Ok(v.to_vec())
            }
        }
        d.deserialize_bytes(BV)
    }
}

/// One op-log record. See the module header for the frozen field-by-field
/// encoding. Field order here IS the wire order — do not reorder.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub v: u8,
    pub device: String,
    pub seq: u64,
    pub lc: u64,
    pub ts: String,
    pub actor: String,
    pub kind: String,
    #[serde(with = "bytes32")]
    pub prev: [u8; 32],
    pub payload: ciborium::Value,
}

impl Record {
    /// Convenience constructor: current envelope version, zeroed `prev` (the
    /// writer owns the chain and overwrites `prev` on append — see
    /// `LogWriter::append_batch`).
    pub fn new(
        device: &str,
        seq: u64,
        lc: u64,
        ts: &str,
        actor: &str,
        kind: &str,
        payload: ciborium::Value,
    ) -> Record {
        Record {
            v: ENVELOPE_VERSION,
            device: device.to_string(),
            seq,
            lc,
            ts: ts.to_string(),
            actor: actor.to_string(),
            kind: kind.to_string(),
            prev: [0u8; 32],
            payload,
        }
    }

    /// Encode to the frozen CBOR byte form.
    pub fn to_cbor_bytes(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(256);
        ciborium::into_writer(self, &mut out).context("encoding record to CBOR")?;
        Ok(out)
    }

    /// Decode from the frozen CBOR byte form. Strict: unknown map keys are an
    /// error (a record this version cannot fully understand is a record it
    /// must not half-understand).
    pub fn from_cbor_bytes(bytes: &[u8]) -> Result<Record> {
        ciborium::from_reader(bytes).context("decoding record from CBOR")
    }
}

/// True when `ts` has the exact shape the envelope freezes (see module
/// header): `YYYY-MM-DDTHH:MM:SS.mmmZ`, 24 ASCII chars, digits in every
/// numeric position. Shape-only by design — calendar validity is the
/// caller's business; lexicographic sortability is this format's job.
pub fn ts_shape_ok(ts: &str) -> bool {
    let b = ts.as_bytes();
    if b.len() != 24 {
        return false;
    }
    const SEPS: [(usize, u8); 7] = [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
        (23, b'Z'),
    ];
    if SEPS.iter().any(|&(i, c)| b[i] != c) {
        return false;
    }
    b.iter()
        .enumerate()
        .all(|(i, &c)| matches!(i, 4 | 7 | 10 | 13 | 16 | 19 | 23) || c.is_ascii_digit())
}

/// Device id allowlist: 1–64 chars of `[A-Za-z0-9._-]`, and not "." / "..".
/// Device ids become directory names; this keeps them boring.
pub fn device_id_ok(device: &str) -> bool {
    !device.is_empty()
        && device.len() <= 64
        && device != "."
        && device != ".."
        && device
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_shape_accepts_the_store_format() {
        assert!(ts_shape_ok("2026-07-10T12:34:56.789Z"));
        assert!(ts_shape_ok("0001-01-01T00:00:00.000Z"));
    }

    #[test]
    fn ts_shape_rejects_near_misses() {
        assert!(!ts_shape_ok("2026-07-10T12:34:56.789")); // no Z
        assert!(!ts_shape_ok("2026-07-10T12:34:56Z")); // no millis
        assert!(!ts_shape_ok("2026-07-10T12:34:56.7890Z")); // micros
        assert!(!ts_shape_ok("2026-07-10 12:34:56.789Z")); // space
        assert!(!ts_shape_ok("2026-07-10T12:34:56.78!Z")); // non-digit
        assert!(!ts_shape_ok(""));
    }

    #[test]
    fn device_ids() {
        assert!(device_id_ok("laptop-01"));
        assert!(device_id_ok("dev.A_2"));
        assert!(!device_id_ok(""));
        assert!(!device_id_ok("."));
        assert!(!device_id_ok(".."));
        assert!(!device_id_ok("a/b"));
        assert!(!device_id_ok("nul\0"));
        assert!(!device_id_ok(&"x".repeat(65)));
    }

    #[test]
    fn record_roundtrip_preserves_every_field() {
        let payload = ciborium::Value::Map(vec![(
            ciborium::Value::Text("body".into()),
            ciborium::Value::Text("hello".into()),
        )]);
        let mut r = Record::new(
            "dev-1",
            7,
            42,
            "2026-07-10T12:34:56.789Z",
            "nate",
            kind::JOURNAL_APPEND,
            payload,
        );
        r.prev = [9u8; 32];
        let bytes = r.to_cbor_bytes().unwrap();
        let back = Record::from_cbor_bytes(&bytes).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn prev_encodes_as_byte_string_not_int_array() {
        // Major type 2 (byte string), length 32 → header byte 0x58 0x20.
        let r = Record::new(
            "d",
            1,
            1,
            "2026-07-10T00:00:00.000Z",
            "nate",
            kind::ALIAS,
            ciborium::Value::Null,
        );
        let bytes = r.to_cbor_bytes().unwrap();
        let needle = [0x58u8, 0x20];
        assert!(
            bytes.windows(2).any(|w| w == needle),
            "expected a 32-byte CBOR byte string in the encoding"
        );
    }
}
