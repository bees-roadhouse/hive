// The replication wire (PLAN-v2.1 PR 4.4, D29): length-prefixed CBOR control
// frames, with raw payload streams behind the two frames that carry bytes.
//
// ── Framing ─────────────────────────────────────────────────────────────────
//
//   control frame   4 bytes   u32 LE body length L (1..=MAX_CONTROL_FRAME)
//                   L bytes   CBOR-encoded `Frame`
//
//   payload stream  N bytes   raw, exactly the length the `Segment` or
//                             `PutBlock` frame in front of it declared
//
// Little-endian and blake3-addressed to match the frozen segment format
// (oplog/mod.rs) — one endianness in the tree. The payload is NOT wrapped in
// CBOR: verbatim sealed-segment bytes are the only transfer primitive (D29),
// and a `bstr` copy would cost a second buffer per megabyte to say nothing
// new. It also puts the resume story on the byte stream where it belongs — a
// connection that dies mid-payload simply desynchronises, and the next
// session re-asks from the receiver's landed offset.
//
// ── This is deliberately NOT a frozen format ────────────────────────────────
//
// GOLDEN-BYTES covers artifacts with a freeze promise — envelope kinds,
// control-record payloads, wrap formats, key derivations, segment headers.
// The wire is not one of them while it churns through PRs 4.4-4.16, so it
// gets ROUND-TRIP + HOSTILE-DECODE coverage instead and its compat fixtures
// are cut once, at the v0.8.0 tag that carries proto v1
// (docs/TESTING-STRATEGY.md §3.4 and the §8.2 rejection that decided it).
// `PROTO_VERSION` is refused-on-mismatch until then, never negotiated down.
//
// ── Decoder rules (HOSTILE-DECODE) ──────────────────────────────────────────
//
// Every length on this wire arrives from an unauthenticated peer, so every
// length is checked against a cap BEFORE it sizes an allocation, and every
// failure is an `Err` — never a panic, never a partial application. The caps
// are protocol constants rather than tuning knobs: both ends compute the same
// bound for the same frame, so an over-cap frame is a protocol violation on
// the sender's side, not a negotiation.

use anyhow::{anyhow, bail, Context, Result};
use hive_core::oplog::HeadsSnapshot;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Wire protocol version. Bumped for any shape change; a mismatched hello is
/// refused rather than guessed at (the bridge handshake's rule, D25).
pub const PROTO_VERSION: u32 = 1;

/// Largest control frame body. A heads snapshot is the only frame that grows
/// with the store, at roughly 60 bytes per segment — 1 MiB is some 17k
/// segments, i.e. ~130 GiB of log at the 8 MiB rotation threshold.
pub const MAX_CONTROL_FRAME: u32 = 1024 * 1024;

/// Largest payload one `Segment` frame may carry. A segment is transferred in
/// chunks of at most this, so a killed transfer costs at most one chunk of
/// re-request and the receiver's landing loop stays bounded.
pub const MAX_SEGMENT_CHUNK: u64 = 1024 * 1024;

/// Smallest a chunk may be configured down to. A chunk shorter than a segment
/// header (21 + device + 72 bytes, so 157 at the longest legal device id)
/// could never open a segment at all, and one much smaller than a record
/// makes the transfer mostly re-request.
pub const MIN_SEGMENT_CHUNK: u64 = 4096;

/// Largest payload one `PutBlock` frame may carry. Chunk blocks top out at
/// the blockstore's `CDC_MAX` (1 MiB) plus an AEAD tag; a blob manifest grows
/// with the blob, and 4 MiB of manifest describes roughly 90k chunks — far
/// past anything personal scale produces.
pub const MAX_BLOCK_BYTES: u64 = 4 * 1024 * 1024;

/// Block ids per `HaveBlocks`/`WantBlocks` frame. 4096 × 32 bytes = 128 KiB,
/// comfortably inside the control-frame cap; longer lists are batched.
pub const MAX_BLOCK_IDS_PER_FRAME: usize = 4096;

/// Which half of a session a peer is speaking. Stage A is one-way backup
/// (D29): the device pushes, the node receives, and neither ingests the
/// other's records. Roles are declared in the hello and must be complementary
/// — two pushers have nothing to say to each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Offers its heads and blocks, answers wants.
    Push,
    /// Asks for what it lacks and lands it.
    Receive,
}

impl Role {
    /// The role a peer must claim for this one to be talking to it.
    pub fn counterpart(self) -> Role {
        match self {
            Role::Push => Role::Receive,
            Role::Receive => Role::Push,
        }
    }
}

/// First frame each way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub proto: u32,
    /// The one domain this session addresses. A session is scoped to exactly
    /// one (D29's hub-only boundary): there is no frame that names a second,
    /// so a connection authenticated for one domain cannot reach another.
    pub domain: String,
    /// Who is speaking — device id or node id. Informational in v2.1:
    /// authentication is the transport's job (mTLS, PRs 4.5/4.7), and nothing
    /// here is trusted because of this string.
    pub peer: String,
    pub role: Role,
}

/// "Send me `(device, start_seq)` starting at `from_offset`" — the receiver's
/// resume request. `from_offset` is always the receiver's landed length, so a
/// growing tail is asked for exactly once past what it already holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WantSegment {
    pub device: String,
    pub start_seq: u64,
    pub from_offset: u64,
}

/// The answer to a `WantSegment`: `len` verbatim bytes follow on the stream.
/// `device`/`start_seq`/`from_offset` echo the request so a receiver never
/// files bytes against the wrong segment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Segment {
    pub device: String,
    pub start_seq: u64,
    pub from_offset: u64,
    pub len: u64,
}

/// The receiver's ack: what it now holds of that segment. `landed` can be
/// short of what was sent — whole frames land, a partial frame at the end of
/// a chunk does not — and it is exactly the `from_offset` of the next want.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentLanded {
    pub device: String,
    pub start_seq: u64,
    pub landed: u64,
}

/// A list of bare 32-byte ciphertext block ids — the payload of both
/// `HaveBlocks` (the pusher's inventory) and `WantBlocks` (the subset the
/// receiver lacks).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockIds {
    #[serde(with = "idlist")]
    pub ids: Vec<[u8; 32]>,
}

/// One block by bare id: `len` ciphertext bytes follow on the stream. The
/// receiver re-hashes them (`BlockStore::put_block`), which is the only
/// integrity check a keyless holder has and the whole reason ids are the
/// address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PutBlock {
    #[serde(with = "bytes32")]
    pub id: [u8; 32],
    pub len: u64,
}

/// The receiver's ack for one landed block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockLanded {
    #[serde(with = "bytes32")]
    pub id: [u8; 32],
}

/// A session-fatal refusal. Sent best-effort before hanging up, so the peer
/// logs a reason instead of a bare EOF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtoError {
    /// One of `err_code::*` — stable enough to branch on.
    pub code: String,
    /// Human text. Never trusted, only logged.
    pub message: String,
}

/// Error codes carried by [`ProtoError`].
pub mod err_code {
    /// The peer speaks a different `PROTO_VERSION`.
    pub const VERSION: &str = "proto-version";
    /// The peer's session is scoped to a different domain.
    pub const DOMAIN: &str = "domain-mismatch";
    /// The peer claims a role this side cannot pair with.
    pub const ROLE: &str = "role-mismatch";
    /// A frame arrived that the session's state does not allow.
    pub const UNEXPECTED: &str = "unexpected-frame";
    /// A well-formed request this side will not serve.
    pub const REFUSED: &str = "refused";
}

/// One control frame.
///
/// Payload-bearing variants (`Segment`, `PutBlock`) are a header for the raw
/// bytes that follow them on the stream — reading one and not draining its
/// payload desynchronises the session, which is why only [`crate::session`]
/// reads frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frame {
    Hello(Hello),
    /// The pusher's segment inventory (PR 4.3's snapshot type, unchanged).
    Heads(HeadsSnapshot),
    /// The pusher's block inventory, batched.
    HaveBlocks(BlockIds),
    /// Offer complete / session complete, depending on who sends it.
    Done,
    WantSegment(WantSegment),
    Segment(Segment),
    SegmentLanded(SegmentLanded),
    WantBlocks(BlockIds),
    PutBlock(PutBlock),
    BlockLanded(BlockLanded),
    Error(ProtoError),
}

/// Every frame name, for the round-trip corpus's completeness assert (the
/// `kind::ALL` discipline from the op log, applied to the wire): a new
/// variant fails to compile in [`Frame::variant`] and then fails the corpus
/// test here, so no frame can ship without round-trip and hostile-decode
/// coverage.
pub const VARIANTS: &[&str] = &[
    "hello",
    "heads",
    "have_blocks",
    "done",
    "want_segment",
    "segment",
    "segment_landed",
    "want_blocks",
    "put_block",
    "block_landed",
    "error",
];

impl Frame {
    /// This frame's wire name — the same string serde writes.
    pub fn variant(&self) -> &'static str {
        match self {
            Frame::Hello(_) => "hello",
            Frame::Heads(_) => "heads",
            Frame::HaveBlocks(_) => "have_blocks",
            Frame::Done => "done",
            Frame::WantSegment(_) => "want_segment",
            Frame::Segment(_) => "segment",
            Frame::SegmentLanded(_) => "segment_landed",
            Frame::WantBlocks(_) => "want_blocks",
            Frame::PutBlock(_) => "put_block",
            Frame::BlockLanded(_) => "block_landed",
            Frame::Error(_) => "error",
        }
    }

    /// How many raw payload bytes follow this frame on the stream (0 for the
    /// control-only frames). The declared length is peer-supplied and still
    /// has to pass [`read_payload`]'s cap.
    pub fn payload_len(&self) -> u64 {
        match self {
            Frame::Segment(s) => s.len,
            Frame::PutBlock(p) => p.len,
            _ => 0,
        }
    }

    /// Encode without framing (the body a length prefix would describe).
    pub fn to_cbor_bytes(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        ciborium::into_writer(self, &mut out)
            .map_err(|e| anyhow!("encoding {} frame: {e}", self.variant()))?;
        Ok(out)
    }

    /// Decode one control-frame body. Strict by construction: unknown map
    /// keys and unknown variants are refused, not ignored.
    pub fn from_cbor_bytes(bytes: &[u8]) -> Result<Frame> {
        ciborium::from_reader(bytes).map_err(|e| anyhow!("decoding control frame: {e}"))
    }
}

/// Write one control frame (length prefix + body) and flush it.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> Result<()> {
    let body = frame.to_cbor_bytes()?;
    let len = u32::try_from(body.len())
        .ok()
        .filter(|l| *l > 0 && *l <= MAX_CONTROL_FRAME)
        .with_context(|| {
            format!(
                "{} frame encodes to {} bytes, over the {MAX_CONTROL_FRAME}-byte control cap",
                frame.variant(),
                body.len()
            )
        })?;
    w.write_all(&len.to_le_bytes())
        .await
        .context("writing control-frame length")?;
    w.write_all(&body)
        .await
        .context("writing control-frame body")?;
    w.flush().await.context("flushing control frame")?;
    Ok(())
}

/// Read one control frame. The declared length is checked against
/// [`MAX_CONTROL_FRAME`] before anything is allocated, so a peer claiming
/// 4 GiB costs one refused read, not 4 GiB of memory.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame> {
    let mut prefix = [0u8; 4];
    r.read_exact(&mut prefix)
        .await
        .context("reading the control-frame length prefix")?;
    let len = u32::from_le_bytes(prefix);
    if len == 0 || len > MAX_CONTROL_FRAME {
        bail!("control frame declares {len} bytes (cap {MAX_CONTROL_FRAME}) — refused unread");
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)
        .await
        .context("reading control-frame body")?;
    Frame::from_cbor_bytes(&body)
}

/// Write a raw payload stream — the bytes a `Segment`/`PutBlock` frame just
/// declared, verbatim.
pub async fn write_payload<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> Result<()> {
    w.write_all(bytes).await.context("writing payload stream")?;
    w.flush().await.context("flushing payload stream")?;
    Ok(())
}

/// Read exactly `len` payload bytes, refusing anything over `cap` before the
/// allocation. A short stream is an error and lands nothing: the receiver's
/// resume point stays whatever it was.
pub async fn read_payload<R: AsyncRead + Unpin>(r: &mut R, len: u64, cap: u64) -> Result<Vec<u8>> {
    if len > cap {
        bail!("payload declares {len} bytes (cap {cap}) — refused unread");
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf)
        .await
        .with_context(|| format!("reading a {len}-byte payload stream"))?;
    Ok(buf)
}

/// serde adapter: one `[u8; 32]` as a CBOR byte string rather than serde's
/// default 32 integers. Same shape as the op log's own `bytes32` (which is
/// crate-private to the frozen format).
mod bytes32 {
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

/// serde adapter: a block-id list as ONE byte string of 32-byte ids back to
/// back. Compact (4096 ids in 128 KiB, no per-id framing) and it gives the
/// decoder a single arithmetic invariant to enforce — a length that is not a
/// multiple of 32, or an id count over [`MAX_BLOCK_IDS_PER_FRAME`], is
/// refused rather than rounded off.
mod idlist {
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};

    use super::MAX_BLOCK_IDS_PER_FRAME;

    pub fn serialize<S: Serializer>(v: &[[u8; 32]], s: S) -> Result<S::Ok, S::Error> {
        let mut flat = Vec::with_capacity(v.len() * 32);
        for id in v {
            flat.extend_from_slice(id);
        }
        s.serialize_bytes(&flat)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<[u8; 32]>, D::Error> {
        struct Ids;
        impl Visitor<'_> for Ids {
            type Value = Vec<[u8; 32]>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a byte string of 32-byte block ids")
            }
            fn visit_bytes<E: de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                if v.len() % 32 != 0 {
                    return Err(E::custom(format!(
                        "block-id list is {} bytes, not a multiple of 32",
                        v.len()
                    )));
                }
                if v.len() / 32 > MAX_BLOCK_IDS_PER_FRAME {
                    return Err(E::custom(format!(
                        "block-id list carries {} ids, over the {MAX_BLOCK_IDS_PER_FRAME}-id cap",
                        v.len() / 32
                    )));
                }
                Ok(v.chunks_exact(32)
                    .map(|c| c.try_into().expect("32-byte chunk"))
                    .collect())
            }

            fn visit_byte_buf<E: de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
                self.visit_bytes(&v)
            }
        }
        // `deserialize_byte_buf`, not `deserialize_bytes`: ciborium's
        // borrowing path only covers byte strings that fit its 4 KiB scratch
        // buffer and rejects anything longer as a type error, and a full id
        // batch is 128 KiB. The buffering path handles any length — and the
        // cap that bounds the allocation is the control-frame length prefix,
        // which was checked before a byte of this body was read.
        d.deserialize_byte_buf(Ids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variant_table_matches_the_wire_names() {
        // Every name in VARIANTS is produced by some frame, and vice versa —
        // the corpus test in tests/framing.rs leans on this pairing.
        let named: Vec<&str> = vec![
            Frame::Done.variant(),
            Frame::Hello(Hello {
                proto: PROTO_VERSION,
                domain: "d".into(),
                peer: "p".into(),
                role: Role::Push,
            })
            .variant(),
        ];
        for name in named {
            assert!(VARIANTS.contains(&name), "{name} missing from VARIANTS");
        }
        assert_eq!(
            VARIANTS.len(),
            11,
            "a new frame needs round-trip and hostile-decode coverage"
        );
    }

    #[test]
    fn roles_pair_up() {
        assert_eq!(Role::Push.counterpart(), Role::Receive);
        assert_eq!(Role::Receive.counterpart(), Role::Push);
    }

    #[test]
    fn only_payload_frames_declare_a_payload() {
        assert_eq!(Frame::Done.payload_len(), 0);
        assert_eq!(
            Frame::Segment(Segment {
                device: "dev-a".into(),
                start_seq: 1,
                from_offset: 0,
                len: 4096,
            })
            .payload_len(),
            4096
        );
        assert_eq!(
            Frame::PutBlock(PutBlock {
                id: [3u8; 32],
                len: 17,
            })
            .payload_len(),
            17
        );
    }
}
