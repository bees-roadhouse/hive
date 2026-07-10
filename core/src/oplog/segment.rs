// Byte-level segment primitives shared by LogWriter and LogReader: header
// encode/parse, frame seal/open, and the frozen key/nonce derivations. The
// authoritative layout documentation lives in the module header (mod.rs);
// this file implements it and nothing else.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

use super::{ENVELOPE_VERSION, MAX_FRAME_CIPHERTEXT, SEGMENT_MAGIC};
use crate::keys;

/// blake3 derive_key contexts (frozen; see mod.rs "Cryptography").
const SEGMENT_KEY_CONTEXT: &str = "hive-oplog-segment-key-v1";
const FRAME_NONCE_CONTEXT: &str = "hive-oplog-frame-nonce-v1";

/// Fixed header length for a given device id length.
pub(crate) fn header_len(device_len: usize) -> u64 {
    (8 + 1 + 2 + device_len + 8 + 2 + keys::WRAPPED_KEY_LEN) as u64
}

/// `<data_dir>/log/<device>` — the device's segment directory.
pub(crate) fn device_dir(data_dir: &Path, device: &str) -> PathBuf {
    data_dir.join("log").join(device)
}

/// `<start_seq:016x>.seg` inside the device dir.
pub(crate) fn segment_path(data_dir: &Path, device: &str, start_seq: u64) -> PathBuf {
    device_dir(data_dir, device).join(format!("{start_seq:016x}.seg"))
}

/// Derive the per-segment content key (frozen derivation, see mod.rs):
/// keyed blake3 of `u16_le(len(device)) ‖ device ‖ u64_le(start_seq)` under a
/// master-derived subkey. Unique per segment because (device, start_seq) is.
pub(crate) fn derive_segment_key(master: &[u8; 32], device: &str, start_seq: u64) -> [u8; 32] {
    let subkey = blake3::derive_key(SEGMENT_KEY_CONTEXT, master);
    let mut input = Vec::with_capacity(2 + device.len() + 8);
    input.extend_from_slice(&(device.len() as u16).to_le_bytes());
    input.extend_from_slice(device.as_bytes());
    input.extend_from_slice(&start_seq.to_le_bytes());
    *blake3::keyed_hash(&subkey, &input).as_bytes()
}

/// Deterministic SIV-style frame nonce: PRF of the record plaintext under a
/// segment-key-derived subkey (why this is safe: mod.rs "Cryptography").
fn frame_nonce(segment_key: &[u8; 32], record_bytes: &[u8]) -> [u8; 24] {
    let nonce_key = blake3::derive_key(FRAME_NONCE_CONTEXT, segment_key);
    let digest = blake3::keyed_hash(&nonce_key, record_bytes);
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&digest.as_bytes()[..24]);
    nonce
}

/// Encode a segment header (see mod.rs for the byte-by-byte layout).
pub(crate) fn encode_header(
    master: &[u8; 32],
    device: &str,
    start_seq: u64,
    segment_key: &[u8; 32],
) -> Result<Vec<u8>> {
    let wrapped = keys::wrap_key(master, segment_key)?;
    debug_assert_eq!(wrapped.len(), keys::WRAPPED_KEY_LEN);
    let mut out = Vec::with_capacity(header_len(device.len()) as usize);
    out.extend_from_slice(&SEGMENT_MAGIC);
    out.push(ENVELOPE_VERSION);
    out.extend_from_slice(&(device.len() as u16).to_le_bytes());
    out.extend_from_slice(device.as_bytes());
    out.extend_from_slice(&start_seq.to_le_bytes());
    out.extend_from_slice(&(wrapped.len() as u16).to_le_bytes());
    out.extend_from_slice(&wrapped);
    Ok(out)
}

/// A parsed segment header.
pub(crate) struct Header {
    pub start_seq: u64,
    pub segment_key: [u8; 32],
    /// Total header length in bytes (frame 0 starts here).
    pub len: u64,
}

/// Outcome of reading a header from the front of a segment file.
pub(crate) enum HeaderRead {
    Ok(Header),
    /// The file ends inside the header. Distinguished from `Ok`+garbage so
    /// the writer can treat a crash-during-creation tail file as disposable.
    Torn,
}

/// Parse a segment header from `r`. `expect_device` must match the header's
/// device field (a segment can't be smuggled between device dirs).
///
/// Errors are *structural* (wrong magic, wrong device, bad wrap): real
/// corruption or misuse. A merely-truncated header comes back as
/// `HeaderRead::Torn`.
pub(crate) fn read_header(
    r: &mut impl Read,
    master: &[u8; 32],
    expect_device: &str,
) -> Result<HeaderRead> {
    let mut magic = [0u8; 8];
    if !read_exact_or_eof(r, &mut magic)? {
        return Ok(HeaderRead::Torn);
    }
    if magic != SEGMENT_MAGIC {
        bail!("bad segment magic {:02x?}", magic);
    }
    let mut v = [0u8; 1];
    if !read_exact_or_eof(r, &mut v)? {
        return Ok(HeaderRead::Torn);
    }
    if v[0] != ENVELOPE_VERSION {
        bail!("unsupported segment envelope version {}", v[0]);
    }
    let mut dlen = [0u8; 2];
    if !read_exact_or_eof(r, &mut dlen)? {
        return Ok(HeaderRead::Torn);
    }
    let dlen = u16::from_le_bytes(dlen) as usize;
    if dlen == 0 || dlen > 64 {
        bail!("segment header device length {dlen} out of range");
    }
    let mut device = vec![0u8; dlen];
    if !read_exact_or_eof(r, &mut device)? {
        return Ok(HeaderRead::Torn);
    }
    let device = String::from_utf8(device).context("segment header device id is not UTF-8")?;
    if device != expect_device {
        bail!("segment header names device {device:?}, expected {expect_device:?}");
    }
    let mut sseq = [0u8; 8];
    if !read_exact_or_eof(r, &mut sseq)? {
        return Ok(HeaderRead::Torn);
    }
    let start_seq = u64::from_le_bytes(sseq);
    let mut wlen = [0u8; 2];
    if !read_exact_or_eof(r, &mut wlen)? {
        return Ok(HeaderRead::Torn);
    }
    let wlen = u16::from_le_bytes(wlen) as usize;
    if wlen != keys::WRAPPED_KEY_LEN {
        bail!(
            "segment header wrapped-key length {wlen}, expected {}",
            keys::WRAPPED_KEY_LEN
        );
    }
    let mut wrapped = vec![0u8; wlen];
    if !read_exact_or_eof(r, &mut wrapped)? {
        return Ok(HeaderRead::Torn);
    }
    let segment_key = keys::unwrap_key(master, &wrapped)
        .context("unwrapping segment key — wrong master key or corrupted header")?;
    Ok(HeaderRead::Ok(Header {
        start_seq,
        segment_key,
        len: header_len(dlen),
    }))
}

/// Seal one record into frame bytes: `u32_le(C) ‖ nonce(24) ‖ ciphertext(C)`.
/// Returns the frame bytes and their blake3 (the frame hash the next record
/// chains to).
pub(crate) fn seal_frame(
    segment_key: &[u8; 32],
    record_bytes: &[u8],
) -> Result<(Vec<u8>, [u8; 32])> {
    let nonce = frame_nonce(segment_key, record_bytes);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(segment_key));
    let ct = cipher
        .encrypt(XNonce::from_slice(&nonce), record_bytes)
        .map_err(|e| anyhow!("frame encryption failed: {e}"))?;
    let clen = u32::try_from(ct.len()).context("frame ciphertext exceeds u32")?;
    if clen > MAX_FRAME_CIPHERTEXT {
        bail!("record encodes to {clen} ciphertext bytes, above MAX_FRAME_CIPHERTEXT");
    }
    let mut frame = Vec::with_capacity(4 + 24 + ct.len());
    frame.extend_from_slice(&clen.to_le_bytes());
    frame.extend_from_slice(&nonce);
    frame.extend_from_slice(&ct);
    let hash = *blake3::hash(&frame).as_bytes();
    Ok((frame, hash))
}

/// Result of pulling one frame off a segment stream.
pub(crate) enum FrameRead {
    /// A structurally complete frame: raw bytes + hash + decrypted record bytes.
    Ok {
        record_bytes: Vec<u8>,
        frame_hash: [u8; 32],
        /// Total frame length on disk (4 + 24 + C).
        frame_len: u64,
    },
    /// Clean end of segment: EOF exactly on a frame boundary.
    Eof,
    /// The file ends mid-frame (or declares a length past sanity/EOF): the
    /// canonical torn-write shape.
    Torn,
}

/// Read and open the next frame. AEAD failure on a structurally complete
/// frame is an error (corruption), not `Torn` — a torn write cannot produce
/// a full-length frame, only a short one.
pub(crate) fn read_frame(r: &mut impl Read, segment_key: &[u8; 32]) -> Result<FrameRead> {
    let mut lenb = [0u8; 4];
    match read_exact_or_eof_allow_empty(r, &mut lenb)? {
        ReadOutcome::Full => {}
        ReadOutcome::CleanEof => return Ok(FrameRead::Eof),
        ReadOutcome::Partial => return Ok(FrameRead::Torn),
    }
    let clen = u32::from_le_bytes(lenb);
    if !(16..=MAX_FRAME_CIPHERTEXT).contains(&clen) {
        // Shorter than an AEAD tag or absurdly long: either a torn length
        // word or corruption. The caller decides by position (writers treat
        // tail damage as torn; scan reports it).
        return Ok(FrameRead::Torn);
    }
    let mut nonce = [0u8; 24];
    if !read_exact_or_eof(r, &mut nonce)? {
        return Ok(FrameRead::Torn);
    }
    let mut ct = vec![0u8; clen as usize];
    if !read_exact_or_eof(r, &mut ct)? {
        return Ok(FrameRead::Torn);
    }
    let mut frame = Vec::with_capacity(4 + 24 + ct.len());
    frame.extend_from_slice(&lenb);
    frame.extend_from_slice(&nonce);
    frame.extend_from_slice(&ct);
    let frame_hash = *blake3::hash(&frame).as_bytes();
    let cipher = XChaCha20Poly1305::new(Key::from_slice(segment_key));
    let record_bytes = cipher
        .decrypt(XNonce::from_slice(&nonce), ct.as_slice())
        .map_err(|_| anyhow!("frame authentication failed — corrupted frame or wrong key"))?;
    Ok(FrameRead::Ok {
        record_bytes,
        frame_hash,
        frame_len: frame.len() as u64,
    })
}

enum ReadOutcome {
    Full,
    CleanEof,
    Partial,
}

/// read_exact, but EOF before the first byte is `CleanEof` and EOF mid-buffer
/// is `Partial` instead of an io error.
fn read_exact_or_eof_allow_empty(r: &mut impl Read, buf: &mut [u8]) -> Result<ReadOutcome> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = r.read(&mut buf[filled..]).context("reading segment")?;
        if n == 0 {
            return Ok(if filled == 0 {
                ReadOutcome::CleanEof
            } else {
                ReadOutcome::Partial
            });
        }
        filled += n;
    }
    Ok(ReadOutcome::Full)
}

/// read_exact, but any EOF (empty or mid-buffer) returns false instead of an
/// io error — used where "ran out of file" means torn, full stop.
fn read_exact_or_eof(r: &mut impl Read, buf: &mut [u8]) -> Result<bool> {
    match read_exact_or_eof_allow_empty(r, buf)? {
        ReadOutcome::Full => Ok(true),
        ReadOutcome::CleanEof | ReadOutcome::Partial => Ok(false),
    }
}

/// List a device's segment files sorted by start seq (filename order). A
/// missing device dir is an empty log, not an error. Non-`.seg` entries are
/// ignored; malformed `.seg` names are an error (nothing else may squat in
/// the log dir).
pub(crate) fn list_segments(data_dir: &Path, device: &str) -> Result<Vec<(u64, PathBuf)>> {
    let dir = device_dir(data_dir, device);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("reading log dir {}", dir.display())),
    };
    let mut segs = Vec::new();
    for entry in entries {
        let entry = entry.context("reading log dir entry")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".seg") else {
            continue;
        };
        if stem.len() != 16 {
            bail!("unexpected segment filename {name:?} in {}", dir.display());
        }
        let start_seq = u64::from_str_radix(stem, 16)
            .with_context(|| format!("unexpected segment filename {name:?}"))?;
        segs.push((start_seq, entry.path()));
    }
    segs.sort_by_key(|(s, _)| *s);
    Ok(segs)
}

/// fsync a directory so a just-created/renamed/removed entry name is durable.
/// No-op on non-Unix (Windows has no directory fsync).
pub(crate) fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let d = std::fs::File::open(dir)
            .with_context(|| format!("opening dir {} for fsync", dir.display()))?;
        d.sync_all()
            .with_context(|| format!("fsync dir {}", dir.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}
