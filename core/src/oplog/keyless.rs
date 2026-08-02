// The KEYLESS read surface over the op log (PLAN-v2.1 PR 4.3, D29): segment
// enumeration, header parse, and frame walk — everything replication must
// know about a segment before, and without, decrypting it. Nothing here
// touches `keys::KeySource`; `LogReader` remains the only thing that opens a
// frame.
//
// Why keyless is the point: a blind node holds ciphertext and no keys (D29),
// so every structural claim it verifies — this file is a segment, it belongs
// to device X starting at seq N, its frames are whole, its bytes hash to H —
// has to be checkable from plaintext bytes alone. The frozen layout already
// allows exactly that: magic, envelope version, device id and start seq all
// precede the wrapped key (mod.rs "Segment files"), and every frame's length
// word and its blake3 are plaintext too. core/tests/sync_surface.rs pins the
// keyless view against the keyed reader's, frame hash for frame hash.
//
// Two rules this module keeps:
//   - read-only, always. It creates nothing, repairs nothing, and never
//     truncates: `LogWriter::open` stays the only repair path.
//   - allocation-bounded. Parsers slice a caller-supplied buffer and never
//     size an allocation from a declared length, so a frame header claiming
//     4 GiB is refused rather than reserved (HOSTILE-DECODE coverage lives in
//     core/tests/hostile_decode.rs — this is network-facing decoder #1).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::{
    bytes32, device_id_ok, segment, ENVELOPE_VERSION, MAX_FRAME_CIPHERTEXT, SEGMENT_MAGIC,
};
use crate::keys::WRAPPED_KEY_LEN;

/// One segment file as replication sees it: the frozen identity tuple
/// (device, start_seq) plus what a peer needs to decide whether it already
/// holds these bytes.
///
/// Serializable because it IS the heads-exchange payload (PR 4.4). It carries
/// no path on purpose — a local filesystem location must never go on a wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentInfo {
    /// Owning device (the `log/<device>/` directory this file lives in).
    pub device: String,
    /// Seq of the segment's first record — with `device`, the segment's
    /// identity, and half of the frozen segment-key derivation input.
    pub start_seq: u64,
    /// File length in bytes. Final for a sealed segment; for the tail it is
    /// the resume point a peer asks to continue from.
    pub len: u64,
    /// False for the one segment the writer may still append to. Sealing is a
    /// writer transition, not a byte in the file: on disk the segment with the
    /// highest start_seq is the tail and every earlier one is sealed, because
    /// rotation only ever adds a new, higher-numbered file (writer.rs).
    pub sealed: bool,
    /// blake3 of the whole file exactly as it is on disk. Stable forever for a
    /// sealed segment; for the tail it describes the prefix that existed when
    /// this was read (`len` bytes of it).
    #[serde(with = "bytes32")]
    pub file_hash: [u8; 32],
}

/// Every segment of every device in one data dir — the heads snapshot a
/// session exchanges before deciding what to ask for.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeadsSnapshot {
    /// Ordered by (device, start_seq): a snapshot's byte form is a function of
    /// the log's contents, never of directory-read order.
    pub segments: Vec<SegmentInfo>,
}

impl HeadsSnapshot {
    /// The devices present, in order, each once.
    pub fn devices(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for seg in &self.segments {
            if out.last() != Some(&seg.device.as_str()) {
                out.push(&seg.device);
            }
        }
        out
    }

    /// One segment by its identity tuple (the receiver's "do I have this?").
    pub fn segment(&self, device: &str, start_seq: u64) -> Option<&SegmentInfo> {
        self.segments
            .iter()
            .find(|s| s.device == device && s.start_seq == start_seq)
    }

    /// Total log bytes described by this snapshot (transfer/quota estimates).
    pub fn total_len(&self) -> u64 {
        self.segments.iter().map(|s| s.len).sum()
    }
}

/// A segment header parsed WITHOUT the master key. Everything before the
/// wrapped key is plaintext; the wrapped bytes themselves are left on disk —
/// this parse deliberately stops before any unwrap attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Envelope version byte (=1; anything else is refused here).
    pub version: u8,
    /// Device id from the header, validated against the same allowlist the
    /// writer enforces — a blind vault turns this string into a directory
    /// name, so it is checked at the parse, not at the use.
    pub device: String,
    pub start_seq: u64,
    /// Length of the wrapped segment key (72 today; the frozen 72-byte wrap
    /// format). The bytes are not exposed: this parse never unwraps.
    pub wrapped_key_len: usize,
    /// Total header length — frame 0 starts at this offset.
    pub len: u64,
}

/// One frame's plaintext-visible facts: where it sits, how long it is, and the
/// blake3 the chain links to. Identical to what the keyed reader computes,
/// because the hash covers the frame bytes exactly as written (mod.rs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameInfo {
    /// Byte offset of the frame's length word within the segment file.
    pub offset: u64,
    /// Total frame length on disk (4 + 24 + C).
    pub len: u64,
    /// blake3 of the frame bytes — the next record's `prev`.
    pub hash: [u8; 32],
}

/// The result of walking a segment's frames keylessly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentWalk {
    pub header: SegmentHeader,
    /// Whole frames, in file order.
    pub frames: Vec<FrameInfo>,
    /// Offset just past the last WHOLE frame (the header length when there are
    /// none). This is the landing point a receiver resumes from: a growing
    /// tail is only ever extended by whole frames, so bytes past here belong
    /// to a batch nobody has acknowledged.
    pub whole_end: u64,
    /// Bytes present past `whole_end` — a partial frame, i.e. a torn write or
    /// a tail caught mid-append. Zero for an intact segment. A caller that
    /// knows the file is SEALED treats any leftover as corruption; only the
    /// tail may legitimately carry one (mod.rs "Recovery semantics").
    pub partial_tail: u64,
}

impl SegmentWalk {
    /// True when the file ends exactly on a frame boundary.
    pub fn is_whole(&self) -> bool {
        self.partial_tail == 0
    }
}

/// `<data_dir>/log/<device>/<start_seq:016x>.seg` — the frozen segment path.
/// Public so callers can read verbatim bytes (the only sanctioned transfer
/// primitive, D29) without re-deriving the naming rule.
pub fn segment_path(data_dir: &Path, device: &str, start_seq: u64) -> PathBuf {
    segment::segment_path(data_dir, device, start_seq)
}

/// The devices with a log under `<data_dir>/log/`, sorted. A missing log dir
/// is an empty list, not an error (a store that has never committed).
///
/// Non-directory entries are ignored; a directory whose name is not a valid
/// device id is an error, matching the log dir's "nothing else may squat
/// here" rule (segment.rs) — a name that cannot be a device is either damage
/// or something that must not be replicated under that name.
pub fn list_devices(data_dir: &Path) -> Result<Vec<String>> {
    let root = data_dir.join("log");
    let entries = match std::fs::read_dir(&root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading log root {}", root.display())),
    };
    let mut devices = Vec::new();
    for entry in entries {
        let entry = entry.context("reading log root entry")?;
        if !entry.file_type().context("stat log root entry")?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !device_id_ok(&name) {
            bail!(
                "log dir {} holds an invalid device id {name:?}",
                root.display()
            );
        }
        devices.push(name);
    }
    devices.sort();
    Ok(devices)
}

/// Enumerate one device's segments, oldest first: the `(device, start_seq,
/// len, sealed, blake3-of-file)` tuple per file. A device with no log is an
/// empty list.
///
/// Length and hash come from ONE read, so they always describe the same bytes
/// — a tail that grows mid-enumeration yields a shorter-but-consistent
/// description, never a length from one instant and a hash from another.
pub fn list_segments(data_dir: &Path, device: &str) -> Result<Vec<SegmentInfo>> {
    if !device_id_ok(device) {
        bail!("invalid device id {device:?}");
    }
    let paths = segment::list_segments(data_dir, device)?;
    let last = paths.len().saturating_sub(1);
    let mut out = Vec::with_capacity(paths.len());
    for (i, (start_seq, path)) in paths.into_iter().enumerate() {
        let bytes =
            std::fs::read(&path).with_context(|| format!("reading segment {}", path.display()))?;
        out.push(SegmentInfo {
            device: device.to_string(),
            start_seq,
            len: bytes.len() as u64,
            sealed: i != last,
            file_hash: *blake3::hash(&bytes).as_bytes(),
        });
    }
    Ok(out)
}

/// The whole data dir's segments as one snapshot, ordered by (device,
/// start_seq).
pub fn heads(data_dir: &Path) -> Result<HeadsSnapshot> {
    let mut segments = Vec::new();
    for device in list_devices(data_dir)? {
        segments.extend(list_segments(data_dir, &device)?);
    }
    Ok(HeadsSnapshot { segments })
}

/// Parse a segment header from the front of `bytes` without unwrapping the
/// segment key. Structural problems (wrong magic, unsupported version, an
/// out-of-range or non-conforming device id, a wrapped-key length that is not
/// the frozen one, a buffer that ends inside the header) are errors — this is
/// a decoder for foreign bytes, so everything it cannot vouch for is refused.
pub fn parse_header(bytes: &[u8]) -> Result<SegmentHeader> {
    let at = |offset: usize, len: usize| -> Result<&[u8]> {
        bytes
            .get(offset..offset + len)
            .with_context(|| format!("segment header ends at {} bytes (truncated)", bytes.len()))
    };
    if at(0, 8)? != SEGMENT_MAGIC {
        bail!("bad segment magic {:02x?}", &bytes[..8]);
    }
    let version = at(8, 1)?[0];
    if version != ENVELOPE_VERSION {
        bail!("unsupported segment envelope version {version}");
    }
    let dlen = u16::from_le_bytes(at(9, 2)?.try_into().expect("2 bytes")) as usize;
    if dlen == 0 || dlen > 64 {
        bail!("segment header device length {dlen} out of range");
    }
    let device = std::str::from_utf8(at(11, dlen)?)
        .context("segment header device id is not UTF-8")?
        .to_string();
    if !device_id_ok(&device) {
        bail!("segment header device id {device:?} is outside the allowlist");
    }
    let start_seq = u64::from_le_bytes(at(11 + dlen, 8)?.try_into().expect("8 bytes"));
    let wrapped_key_len =
        u16::from_le_bytes(at(19 + dlen, 2)?.try_into().expect("2 bytes")) as usize;
    if wrapped_key_len != WRAPPED_KEY_LEN {
        bail!("segment header wrapped-key length {wrapped_key_len}, expected {WRAPPED_KEY_LEN}");
    }
    // The wrapped key itself must be PRESENT (frame 0 starts after it) but is
    // never read here: unwrapping is the keyed side's business.
    at(21 + dlen, wrapped_key_len)?;
    Ok(SegmentHeader {
        version,
        device,
        start_seq,
        wrapped_key_len,
        len: (21 + dlen + wrapped_key_len) as u64,
    })
}

/// Walk a whole segment file's frames keylessly: header, then every whole
/// frame's offset/length/hash, then how the file ends.
///
/// Frame-level damage is NOT an error, mirroring the frozen recovery
/// semantics (`segment::read_frame` treats an out-of-bounds length word as a
/// torn write): the walk stops and reports the leftover bytes in
/// `partial_tail`, because keylessly there is no way to tell a half-written
/// tail from corruption — that judgment needs to know whether the file is
/// sealed, which is the caller's knowledge, not the parser's.
pub fn walk_segment(bytes: &[u8]) -> Result<SegmentWalk> {
    let header = parse_header(bytes)?;
    let total = bytes.len() as u64;
    let mut offset = header.len;
    let mut frames = Vec::new();
    loop {
        let remaining = total - offset;
        if remaining == 0 {
            break;
        }
        if remaining < 4 {
            break; // not even a length word: torn
        }
        let at = offset as usize;
        let clen = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"));
        if !(16..=MAX_FRAME_CIPHERTEXT).contains(&clen) {
            break; // shorter than an AEAD tag or absurdly long: torn length word
        }
        // u64 arithmetic on purpose: the declared length is attacker-supplied
        // and must never be added into a usize that could wrap.
        let end = offset + 4 + 24 + clen as u64;
        if end > total {
            break; // the frame runs past EOF: torn
        }
        frames.push(FrameInfo {
            offset,
            len: end - offset,
            hash: *blake3::hash(&bytes[at..end as usize]).as_bytes(),
        });
        offset = end;
    }
    Ok(SegmentWalk {
        header,
        frames,
        whole_end: offset,
        partial_tail: total - offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(device: &str, start_seq: u64, len: u64) -> SegmentInfo {
        SegmentInfo {
            device: device.to_string(),
            start_seq,
            len,
            sealed: true,
            file_hash: [0u8; 32],
        }
    }

    #[test]
    fn heads_helpers_read_the_ordered_snapshot() {
        let snap = HeadsSnapshot {
            segments: vec![
                info("dev-a", 1, 10),
                info("dev-a", 5, 20),
                info("dev-b", 1, 7),
            ],
        };
        assert_eq!(snap.devices(), vec!["dev-a", "dev-b"]);
        assert_eq!(snap.segment("dev-a", 5).unwrap().len, 20);
        assert!(snap.segment("dev-b", 5).is_none());
        assert_eq!(snap.total_len(), 37);
    }

    #[test]
    fn parse_header_refuses_a_truncated_buffer_at_every_prefix() {
        // A synthetic but structurally exact header (no keys involved).
        let device = "dev-parse";
        let mut buf = Vec::new();
        buf.extend_from_slice(&SEGMENT_MAGIC);
        buf.push(ENVELOPE_VERSION);
        buf.extend_from_slice(&(device.len() as u16).to_le_bytes());
        buf.extend_from_slice(device.as_bytes());
        buf.extend_from_slice(&9u64.to_le_bytes());
        buf.extend_from_slice(&(WRAPPED_KEY_LEN as u16).to_le_bytes());
        buf.extend_from_slice(&[0xAB; WRAPPED_KEY_LEN]);

        let h = parse_header(&buf).unwrap();
        assert_eq!(h.device, device);
        assert_eq!(h.start_seq, 9);
        assert_eq!(h.len, buf.len() as u64);
        for len in 0..buf.len() {
            assert!(
                parse_header(&buf[..len]).is_err(),
                "a header prefix of {len} bytes must not parse"
            );
        }
    }
}
