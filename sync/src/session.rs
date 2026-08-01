// The session state machine (PLAN-v2.1 PR 4.4, D29): one domain, two halves,
// generic over any byte stream (the `serve_bridge_connection` discipline —
// `AsyncRead + AsyncWrite`, so mTLS-over-TCP today and QUIC or iroh later slot
// underneath with no wire change).
//
// Stage A is one-way backup, so the halves are asymmetric on purpose:
//
//   push     hello ⇄ hello
//            → heads, have_blocks*, done          the offer, in one shot
//            then answers wants until the peer says done
//
//   receive  hello ⇄ hello
//            reads the offer to its `done`
//            → want_segment / reads segment + payload / lands / → segment_landed
//              ... per segment, resuming at its own landed offset
//            → want_blocks / reads the whole batch of put_block + payload /
//              lands each / → block_landed per block, after the batch
//            → done
//
// The receiver drives because it is the only side that knows what it is
// missing, and every request it makes carries the offset it already holds —
// which is what makes a killed transfer cost one chunk instead of a segment.
// The pusher is frame-driven and stateless between requests: it serves what
// it is asked for, records the acks (they are the progress a push loop
// reports), and never decides anything about the receiver's storage.
//
// Two properties this file is responsible for:
//
//   * a frame that does not belong to the current state is a refusal (an
//     `error` frame, best-effort, then a hard stop) — never a panic, never a
//     partially-applied transfer;
//   * every loop terminates against peer-supplied data: segment rounds must
//     make strict progress, block batches are drained against a set of ids
//     the receiver itself asked for, and the offer is bounded by the caps in
//     `frame.rs`.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use hive_core::oplog::{device_id_ok, HeadsSnapshot};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::frame::{
    err_code, read_frame, read_payload, write_frame, write_payload, BlockIds, BlockLanded, Frame,
    Hello, ProtoError, PutBlock, Role, Segment, SegmentLanded, WantSegment, MAX_BLOCK_BYTES,
    MAX_BLOCK_IDS_PER_FRAME, MAX_SEGMENT_CHUNK, MIN_SEGMENT_CHUNK, PROTO_VERSION,
};
use crate::sink::SyncSink;
use crate::source::SyncSource;

/// Chunk rounds one segment may take in a single session before the receiver
/// gives up on it. Liveness only: rounds must already make strict progress,
/// so this bounds a peer that dribbles a frame at a time rather than any
/// honest transfer (4096 rounds is 4 GiB at the 1 MiB chunk cap, against an
/// 8 MiB rotation threshold).
const MAX_ROUNDS_PER_SEGMENT: usize = 4096;

/// One session's identity and knobs.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// The domain this session addresses. Both sides must name the same one;
    /// there is no frame that switches domains mid-session. Until the control
    /// log mints it (PR 4.0's `domain.genesis`), this is whatever the caller
    /// calls the domain — the node passes the `domains/<d>` it serves.
    pub domain: String,
    /// This side's own id (device id, node id) — announced, never trusted.
    pub peer: String,
    /// Bytes to serve per segment chunk, clamped to
    /// [`MIN_SEGMENT_CHUNK`]..=[`MAX_SEGMENT_CHUNK`].
    pub chunk_bytes: u64,
}

impl SessionConfig {
    pub fn new(domain: impl Into<String>, peer: impl Into<String>) -> SessionConfig {
        SessionConfig {
            domain: domain.into(),
            peer: peer.into(),
            chunk_bytes: MAX_SEGMENT_CHUNK,
        }
    }

    /// Smaller chunks for tests that need a transfer to span several rounds
    /// (and for a future rate/latency knob). Clamped to
    /// `MIN_SEGMENT_CHUNK..=MAX_SEGMENT_CHUNK` — a caller can neither raise
    /// the protocol cap nor ask for chunks too small to carry a segment
    /// header.
    pub fn with_chunk_bytes(mut self, bytes: u64) -> SessionConfig {
        self.chunk_bytes = bytes;
        self
    }

    fn chunk(&self) -> u64 {
        self.chunk_bytes.clamp(MIN_SEGMENT_CHUNK, MAX_SEGMENT_CHUNK)
    }
}

/// What one push session did, from the pusher's side. The acks are the
/// receiver's own account of what it holds — the only progress a pusher can
/// honestly report (PR 4.8's push loop surfaces it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PushReport {
    pub segments_offered: usize,
    pub blocks_offered: usize,
    pub segment_chunks_sent: usize,
    pub segment_bytes_sent: u64,
    pub blocks_sent: usize,
    pub block_bytes_sent: u64,
    /// Acks received: `(device, start_seq, landed)`.
    pub segments_acked: Vec<(String, u64, u64)>,
    pub blocks_acked: usize,
}

/// What one receive session landed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReceiveReport {
    pub segments_offered: usize,
    pub blocks_offered: usize,
    /// Segments that reached the offered length in this session.
    pub segments_completed: usize,
    /// Segments already held in full when the offer arrived.
    pub segments_up_to_date: usize,
    pub segment_bytes_landed: u64,
    pub blocks_landed: usize,
    pub block_bytes_landed: u64,
}

/// Serve one push session: offer everything, answer wants, return what the
/// peer said it landed.
pub async fn push_session<R, W, S>(
    mut reader: R,
    mut writer: W,
    source: &S,
    cfg: &SessionConfig,
) -> Result<PushReport>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    S: SyncSource + ?Sized,
{
    let peer = handshake(&mut reader, &mut writer, cfg, Role::Push).await?;
    tracing::debug!(
        domain = %cfg.domain,
        peer = %peer.peer,
        "sync push session open"
    );
    let mut report = PushReport::default();

    // ── the offer ───────────────────────────────────────────────────────────
    let heads = source.heads().await.context("reading heads to offer")?;
    report.segments_offered = heads.segments.len();
    write_frame(&mut writer, &Frame::Heads(heads)).await?;
    let ids = source
        .block_ids()
        .await
        .context("reading blocks to offer")?;
    report.blocks_offered = ids.len();
    for batch in ids.chunks(MAX_BLOCK_IDS_PER_FRAME) {
        write_frame(
            &mut writer,
            &Frame::HaveBlocks(BlockIds {
                ids: batch.to_vec(),
            }),
        )
        .await?;
    }
    write_frame(&mut writer, &Frame::Done).await?;

    // ── serve ───────────────────────────────────────────────────────────────
    loop {
        match read_frame(&mut reader)
            .await
            .context("reading the peer's next request")?
        {
            Frame::WantSegment(want) => {
                let bytes = serve_segment(&mut writer, source, cfg, &want).await?;
                report.segment_chunks_sent += 1;
                report.segment_bytes_sent += bytes;
            }
            Frame::WantBlocks(want) => {
                for id in want.ids {
                    let bytes = serve_block(&mut writer, source, id).await?;
                    report.blocks_sent += 1;
                    report.block_bytes_sent += bytes;
                }
            }
            Frame::SegmentLanded(ack) => {
                report
                    .segments_acked
                    .push((ack.device, ack.start_seq, ack.landed));
            }
            Frame::BlockLanded(_) => report.blocks_acked += 1,
            Frame::Done => break,
            Frame::Error(e) => bail!("peer refused the session: {} ({})", e.message, e.code),
            other => {
                return refuse(
                    &mut writer,
                    err_code::UNEXPECTED,
                    format!(
                        "a {} frame has no place in a served session",
                        other.variant()
                    ),
                )
                .await
            }
        }
    }
    tracing::debug!(
        domain = %cfg.domain,
        chunks = report.segment_chunks_sent,
        blocks = report.blocks_sent,
        "sync push session complete"
    );
    Ok(report)
}

/// Run one receive session: read the offer, ask for what is missing, land it.
pub async fn receive_session<R, W, K>(
    mut reader: R,
    mut writer: W,
    sink: &K,
    cfg: &SessionConfig,
) -> Result<ReceiveReport>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    K: SyncSink + ?Sized,
{
    let hello = read_hello(&mut reader, &mut writer).await?;
    receive_session_with_hello(reader, writer, sink, cfg, hello).await
}

/// `receive_session` for a caller that has ALREADY read the opening frame.
///
/// The node's listener (PR 4.7) reads the first frame itself, because that
/// frame is what says whether the connection is a session or an enrollment
/// redemption — and because the domain a session may address follows from the
/// device's pin, which the listener resolved before any of this. Everything
/// after the hello is identical.
pub async fn receive_session_with_hello<R, W, K>(
    mut reader: R,
    mut writer: W,
    sink: &K,
    cfg: &SessionConfig,
    hello: Hello,
) -> Result<ReceiveReport>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    K: SyncSink + ?Sized,
{
    let peer = answer_hello(&mut writer, hello, cfg, Role::Receive).await?;
    tracing::debug!(
        domain = %cfg.domain,
        peer = %peer.peer,
        "sync receive session open"
    );
    let mut report = ReceiveReport::default();

    // ── read the offer ──────────────────────────────────────────────────────
    let mut heads: Option<HeadsSnapshot> = None;
    let mut offered_blocks: Vec<[u8; 32]> = Vec::new();
    loop {
        match read_frame(&mut reader)
            .await
            .context("reading the peer's offer")?
        {
            Frame::Heads(h) => {
                if heads.is_some() {
                    return refuse(
                        &mut writer,
                        err_code::UNEXPECTED,
                        "a second heads frame in one offer".to_string(),
                    )
                    .await;
                }
                heads = Some(h);
            }
            Frame::HaveBlocks(b) => offered_blocks.extend(b.ids),
            Frame::Done => break,
            Frame::Error(e) => bail!("peer refused the session: {} ({})", e.message, e.code),
            other => {
                return refuse(
                    &mut writer,
                    err_code::UNEXPECTED,
                    format!("a {} frame has no place in an offer", other.variant()),
                )
                .await
            }
        }
    }
    let heads = match heads {
        Some(h) => h,
        None => {
            return refuse(
                &mut writer,
                err_code::UNEXPECTED,
                "the offer ended without a heads frame".to_string(),
            )
            .await
        }
    };
    report.segments_offered = heads.segments.len();
    report.blocks_offered = offered_blocks.len();

    // ── segments ────────────────────────────────────────────────────────────
    for offered in &heads.segments {
        if !device_id_ok(&offered.device) {
            return refuse(
                &mut writer,
                err_code::REFUSED,
                format!(
                    "offered segment names device {:?}, outside the allowlist",
                    offered.device
                ),
            )
            .await;
        }
        // The resume point is a WHOLE-frame offset (the sink repairs any torn
        // tail on the way to answering). Inside the loop below `have` becomes
        // the raw length instead: bytes stream contiguously within a session,
        // and only the handover between sessions has to sit on a frame
        // boundary.
        let mut have = sink
            .landed(&offered.device, offered.start_seq)
            .await
            .with_context(|| {
                format!(
                    "checking what is landed of {}/{}",
                    offered.device, offered.start_seq
                )
            })?
            .map_or(0, |l| l.len);
        if have >= offered.len {
            report.segments_up_to_date += 1;
            verify_complete(sink, offered, have).await?;
            continue;
        }

        let mut rounds = 0usize;
        while have < offered.len {
            rounds += 1;
            if rounds > MAX_ROUNDS_PER_SEGMENT {
                bail!(
                    "segment {}/{} took more than {MAX_ROUNDS_PER_SEGMENT} chunks without \
                     finishing — the peer is dribbling",
                    offered.device,
                    offered.start_seq
                );
            }
            write_frame(
                &mut writer,
                &Frame::WantSegment(WantSegment {
                    device: offered.device.clone(),
                    start_seq: offered.start_seq,
                    from_offset: have,
                }),
            )
            .await?;
            let chunk = match read_frame(&mut reader)
                .await
                .context("reading the answer to a segment want")?
            {
                Frame::Segment(s) => s,
                Frame::Error(e) => {
                    bail!("peer refused a segment want: {} ({})", e.message, e.code)
                }
                other => {
                    return refuse(
                        &mut writer,
                        err_code::UNEXPECTED,
                        format!("a {} frame does not answer a want_segment", other.variant()),
                    )
                    .await
                }
            };
            // The answer has to be the answer to THIS want; anything else and
            // the payload would land against the wrong segment.
            if chunk.device != offered.device
                || chunk.start_seq != offered.start_seq
                || chunk.from_offset != have
            {
                return refuse(
                    &mut writer,
                    err_code::UNEXPECTED,
                    format!(
                        "asked for {}/{} at {have}, was answered {}/{} at {}",
                        offered.device,
                        offered.start_seq,
                        chunk.device,
                        chunk.start_seq,
                        chunk.from_offset
                    ),
                )
                .await;
            }
            if chunk.len == 0 {
                bail!(
                    "peer answered a want for {}/{} at {have} with zero bytes",
                    offered.device,
                    offered.start_seq
                );
            }
            let bytes = read_payload(&mut reader, chunk.len, MAX_SEGMENT_CHUNK).await?;
            let now = sink
                .extend_segment(&offered.device, offered.start_seq, have, bytes)
                .await
                .with_context(|| {
                    format!(
                        "landing {} bytes of {}/{} at {have}",
                        chunk.len, offered.device, offered.start_seq
                    )
                })?;
            report.segment_bytes_landed += now.saturating_sub(have);
            write_frame(
                &mut writer,
                &Frame::SegmentLanded(SegmentLanded {
                    device: offered.device.clone(),
                    start_seq: offered.start_seq,
                    landed: now,
                }),
            )
            .await?;
            if now <= have {
                // A sink that took bytes without growing. Nothing to resume
                // from, so stop rather than ask the same question forever;
                // the round cap above is the backstop for a slow dribble,
                // this is the backstop for no progress at all.
                break;
            }
            have = now;
        }
        if have >= offered.len {
            report.segments_completed += 1;
            verify_complete(sink, offered, have).await?;
        }
    }

    // ── blocks ──────────────────────────────────────────────────────────────
    let mut want: Vec<[u8; 32]> = Vec::new();
    for id in offered_blocks {
        if !sink.has_block(id).await? {
            want.push(id);
        }
    }
    // The offer is foreign input: a repeated id would leave a request
    // outstanding forever, so it is deduplicated before anything is asked for.
    want.sort_unstable();
    want.dedup();
    for batch in want.chunks(MAX_BLOCK_IDS_PER_FRAME) {
        write_frame(
            &mut writer,
            &Frame::WantBlocks(BlockIds {
                ids: batch.to_vec(),
            }),
        )
        .await?;
        let mut pending: HashSet<[u8; 32]> = batch.iter().copied().collect();
        // Acks wait for the end of the batch instead of going out between
        // blocks. The pusher answers a whole batch before it reads anything,
        // so acks written mid-stream pile up unread in the socket — a batch's
        // worth of them outgrows a small kernel buffer, the receiver blocks
        // writing, stops reading, and both sides wedge. Deferred, the pusher
        // is already back at its read loop when they arrive.
        let mut landed: Vec<[u8; 32]> = Vec::with_capacity(batch.len());
        while !pending.is_empty() {
            let put = match read_frame(&mut reader)
                .await
                .context("reading the answer to a block want")?
            {
                Frame::PutBlock(p) => p,
                Frame::Error(e) => bail!("peer refused a block want: {} ({})", e.message, e.code),
                other => {
                    return refuse(
                        &mut writer,
                        err_code::UNEXPECTED,
                        format!("a {} frame does not answer a want_blocks", other.variant()),
                    )
                    .await
                }
            };
            if !pending.remove(&put.id) {
                return refuse(
                    &mut writer,
                    err_code::UNEXPECTED,
                    format!(
                        "pushed block {} was not requested",
                        data_encoding::HEXLOWER.encode(&put.id)
                    ),
                )
                .await;
            }
            let bytes = read_payload(&mut reader, put.len, MAX_BLOCK_BYTES).await?;
            // put_block re-hashes: misaddressed content is refused here, which
            // is the whole integrity story on the keyless side.
            sink.put_block(put.id, bytes).await.with_context(|| {
                format!("landing block {}", data_encoding::HEXLOWER.encode(&put.id))
            })?;
            report.blocks_landed += 1;
            report.block_bytes_landed += put.len;
            landed.push(put.id);
        }
        for id in landed {
            write_frame(&mut writer, &Frame::BlockLanded(BlockLanded { id })).await?;
        }
    }

    write_frame(&mut writer, &Frame::Done).await?;
    tracing::debug!(
        domain = %cfg.domain,
        segments = report.segments_completed,
        blocks = report.blocks_landed,
        "sync receive session complete"
    );
    Ok(report)
}

/// The dialing side: announce, then check what came back. A mismatch is
/// refused rather than negotiated — version, domain, and role are the three
/// things a session cannot proceed without agreeing on.
async fn handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    cfg: &SessionConfig,
    mine: Role,
) -> Result<Hello>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_frame(
        writer,
        &Frame::Hello(Hello {
            proto: PROTO_VERSION,
            domain: cfg.domain.clone(),
            peer: cfg.peer.clone(),
            role: mine,
        }),
    )
    .await?;
    let hello = read_hello(reader, writer).await?;
    check_hello(writer, &hello, cfg, mine).await?;
    Ok(hello)
}

/// The listening side: read the peer's hello, check it, THEN announce.
///
/// The order is the listener's, and it is the reason the two halves are split:
/// a node cannot name a domain in its own hello until it knows which domain
/// the connection is for, and a connection might not be a session at all
/// ([`Frame::Enroll`] arrives in exactly this slot).
async fn answer_hello<W>(
    writer: &mut W,
    hello: Hello,
    cfg: &SessionConfig,
    mine: Role,
) -> Result<Hello>
where
    W: AsyncWrite + Unpin,
{
    check_hello(writer, &hello, cfg, mine).await?;
    write_frame(
        writer,
        &Frame::Hello(Hello {
            proto: PROTO_VERSION,
            domain: cfg.domain.clone(),
            peer: cfg.peer.clone(),
            role: mine,
        }),
    )
    .await?;
    Ok(hello)
}

/// One hello off the wire, with anything else turned into a refusal.
async fn read_hello<R, W>(reader: &mut R, writer: &mut W) -> Result<Hello>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    match read_frame(reader)
        .await
        .context("waiting for the peer's hello")?
    {
        Frame::Hello(h) => Ok(h),
        Frame::Error(e) => bail!("peer refused the session: {} ({})", e.message, e.code),
        other => {
            refuse(
                writer,
                err_code::UNEXPECTED,
                format!("a session opens with a hello, not a {}", other.variant()),
            )
            .await
        }
    }
}

/// The three agreements, checked identically whichever side asked first.
async fn check_hello<W>(
    writer: &mut W,
    hello: &Hello,
    cfg: &SessionConfig,
    mine: Role,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if hello.proto != PROTO_VERSION {
        return refuse(
            writer,
            err_code::VERSION,
            format!(
                "peer speaks sync proto v{}, this build serves v{PROTO_VERSION}",
                hello.proto
            ),
        )
        .await;
    }
    if hello.domain != cfg.domain {
        return refuse(
            writer,
            err_code::DOMAIN,
            format!(
                "peer's session is scoped to domain {:?}, not {:?}",
                hello.domain, cfg.domain
            ),
        )
        .await;
    }
    if hello.role != mine.counterpart() {
        return refuse(
            writer,
            err_code::ROLE,
            format!(
                "peer claims role {:?} against this side's {mine:?}",
                hello.role
            ),
        )
        .await;
    }
    Ok(())
}

/// Answer one `WantSegment`, returning the bytes served.
async fn serve_segment<W, S>(
    writer: &mut W,
    source: &S,
    cfg: &SessionConfig,
    want: &WantSegment,
) -> Result<u64>
where
    W: AsyncWrite + Unpin,
    S: SyncSource + ?Sized,
{
    // Checked at the protocol edge as well as inside the source: the device
    // id is peer-supplied and every implementation turns it into a path.
    if !device_id_ok(&want.device) {
        return refuse(
            writer,
            err_code::REFUSED,
            format!(
                "want_segment names device {:?}, outside the allowlist",
                want.device
            ),
        )
        .await;
    }
    let bytes = match source
        .segment_bytes(&want.device, want.start_seq, want.from_offset, cfg.chunk())
        .await
    {
        Ok(b) if !b.is_empty() => b,
        Ok(_) => {
            return refuse(
                writer,
                err_code::REFUSED,
                format!(
                    "nothing left to serve of {}/{} past {}",
                    want.device, want.start_seq, want.from_offset
                ),
            )
            .await
        }
        Err(e) => {
            return refuse(
                writer,
                err_code::REFUSED,
                format!(
                    "cannot serve {}/{} from {}: {e:#}",
                    want.device, want.start_seq, want.from_offset
                ),
            )
            .await
        }
    };
    write_frame(
        writer,
        &Frame::Segment(Segment {
            device: want.device.clone(),
            start_seq: want.start_seq,
            from_offset: want.from_offset,
            len: bytes.len() as u64,
        }),
    )
    .await?;
    write_payload(writer, &bytes).await?;
    Ok(bytes.len() as u64)
}

/// Answer one requested block id, returning the bytes served.
async fn serve_block<W, S>(writer: &mut W, source: &S, id: [u8; 32]) -> Result<u64>
where
    W: AsyncWrite + Unpin,
    S: SyncSource + ?Sized,
{
    let hex = data_encoding::HEXLOWER.encode(&id);
    let bytes = match source.block_bytes(id).await {
        Ok(b) => b,
        Err(e) => {
            return refuse(
                writer,
                err_code::REFUSED,
                format!("cannot serve block {hex}: {e:#}"),
            )
            .await
        }
    };
    if bytes.len() as u64 > MAX_BLOCK_BYTES {
        return refuse(
            writer,
            err_code::REFUSED,
            format!(
                "block {hex} is {} bytes, over the {MAX_BLOCK_BYTES}-byte block cap",
                bytes.len()
            ),
        )
        .await;
    }
    write_frame(
        writer,
        &Frame::PutBlock(PutBlock {
            id,
            len: bytes.len() as u64,
        }),
    )
    .await?;
    write_payload(writer, &bytes).await?;
    Ok(bytes.len() as u64)
}

/// A fully landed segment must hash to what was announced. The one integrity
/// check available to a holder with no keys, and the reason a tail extension
/// cannot quietly rewrite the prefix it extends.
async fn verify_complete<K>(
    sink: &K,
    offered: &hive_core::oplog::SegmentInfo,
    have: u64,
) -> Result<()>
where
    K: SyncSink + ?Sized,
{
    if have != offered.len {
        // The tail grew past the snapshot mid-session: nothing to compare
        // against, and the next session's offer describes the longer file.
        return Ok(());
    }
    let landed = sink
        .landed(&offered.device, offered.start_seq)
        .await?
        .with_context(|| {
            format!(
                "segment {}/{} vanished from the sink mid-session",
                offered.device, offered.start_seq
            )
        })?;
    if landed.file_hash != offered.file_hash {
        bail!(
            "landed segment {}/{} hashes to {}, but was offered as {} — the bytes on the two \
             sides diverge",
            offered.device,
            offered.start_seq,
            data_encoding::HEXLOWER.encode(&landed.file_hash),
            data_encoding::HEXLOWER.encode(&offered.file_hash)
        );
    }
    Ok(())
}

/// Tell the peer why, best effort, then fail. The write is allowed to fail
/// silently — the peer may already be gone, and the local error is the one
/// that matters.
async fn refuse<W, T>(writer: &mut W, code: &str, message: String) -> Result<T>
where
    W: AsyncWrite + Unpin,
{
    let _ = write_frame(
        writer,
        &Frame::Error(ProtoError {
            code: code.to_string(),
            message: message.clone(),
        }),
    )
    .await;
    bail!("refusing the session ({code}): {message}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chunk_size_is_clamped_to_the_protocol_cap() {
        let cfg = SessionConfig::new("domain", "peer");
        assert_eq!(cfg.chunk(), MAX_SEGMENT_CHUNK);
        assert_eq!(
            SessionConfig::new("d", "p")
                .with_chunk_bytes(MAX_SEGMENT_CHUNK * 4)
                .chunk(),
            MAX_SEGMENT_CHUNK,
            "a caller cannot raise the cap by config"
        );
        assert_eq!(
            SessionConfig::new("d", "p").with_chunk_bytes(0).chunk(),
            MIN_SEGMENT_CHUNK,
            "and cannot ask for a chunk too small to carry a segment header"
        );
        assert_eq!(
            SessionConfig::new("d", "p")
                .with_chunk_bytes(MIN_SEGMENT_CHUNK * 2)
                .chunk(),
            MIN_SEGMENT_CHUNK * 2
        );
    }
}
