// Loopback sync: a real device store pushing into a real directory, over the
// real protocol (PLAN-v2.1 PR 4.4). Smoke tier — multi-component scenarios
// with a store, a vault, and two concurrent session tasks — so every test
// opens with `require_smoke!()` and the domain comes from the ONE-SEAM helper
// (AGENTS.md). No binaries and no sockets: the transport is an in-memory
// duplex, which is all the sessions ask for — the PR 4.5 TLS carrier
// (`hive_sync::tls`, exercised by tests/tls_loopback.rs) wraps the same
// `AsyncRead + AsyncWrite` without a session noticing.
//
// What these pin, beyond "it works":
//
//   * the transfer is VERBATIM — the vault's heads snapshot equals the
//     device's, file hash for file hash, so nothing was re-encoded on the way
//     across (D29: segment boundaries feed the frozen key derivation);
//   * a killed connection costs one chunk, not a segment — the receiver
//     resumes from its landed offset and the sender re-sends nothing that was
//     already verified;
//   * a growing tail extends, byte prefix intact;
//   * the result is a restore: drop the files back and the store heals into
//     byte-identical canonical state (the M1 claim, and the PR 4.8 CLI's
//     whole job).

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use hive_core::blockstore::BlockStore;
use hive_core::store::Store;
use hive_smoke_support::{require_smoke, test_domain, TestDomain};
use hive_sync::session::{PushReport, ReceiveReport};
use hive_sync::{push_session, receive_session, DirVault, SessionConfig};
use serde_json::json;
use tokio::io::AsyncWrite;

const DOMAIN: &str = "example.com";

/// Enough log that a transfer spans several chunks, and enough shape that a
/// broken restore cannot pass by accident (prose the FTS indexes, emerged
/// entities, a config row).
async fn seed(store: &Store, entries: usize) {
    for i in 0..entries {
        store
            .journal_append(
                serde_json::from_value(json!({
                    "body": format!(
                        "Backup rehearsal {i} with @pia — [project: Replication] under \
                         [topic: Backups], [task: Verify chunk {i} lands whole]. The point of \
                         the exercise is to make the log long enough that a transfer has to \
                         take more than one bite at it."
                    ),
                    "tags": ["sync", "loopback"],
                }))
                .unwrap(),
                Some("nate"),
                Some("nate"),
            )
            .await
            .unwrap();
    }
    store.config_set("sync.loopback", "yes").await.unwrap();
}

/// Blocks, straight in by bare id. A blob would take the same path — the wire
/// only ever sees ciphertext under its blake3 — and putting them directly
/// keeps the sizes and the count exact, which the resume assertions need.
async fn seed_blocks(store: &Store, count: usize, size: usize) -> Vec<[u8; 32]> {
    let mut ids = Vec::new();
    for i in 0..count {
        let bytes: Vec<u8> = (0..size).map(|b| ((b + i * 7) % 251) as u8).collect();
        let id = *blake3::hash(&bytes).as_bytes();
        store.sync_put_block(id, bytes).await.unwrap();
        ids.push(id);
    }
    ids.sort_unstable();
    ids
}

/// One whole session, both halves in their own tasks.
///
/// Separate tasks and not `join!`: a task that finishes drops its half of the
/// duplex, which is what turns a dead sender into an EOF for the receiver
/// instead of a hang. `budget` kills the pushing side's writer after that
/// many bytes — the fault injection the resume tests need.
async fn run_session(
    source: Arc<Store>,
    vault: DirVault,
    chunk: u64,
    budget: Option<usize>,
) -> (anyhow::Result<PushReport>, anyhow::Result<ReceiveReport>) {
    run_session_on(source, vault, chunk, budget, 1024 * 1024).await
}

/// `run_session` with an explicit transport buffer — a small one models a
/// socket whose kernel buffer is far smaller than what a session wants to
/// have in flight.
async fn run_session_on(
    source: Arc<Store>,
    vault: DirVault,
    chunk: u64,
    budget: Option<usize>,
    buffer: usize,
) -> (anyhow::Result<PushReport>, anyhow::Result<ReceiveReport>) {
    let (a, b) = tokio::io::duplex(buffer);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let pushing = SessionConfig::new(DOMAIN, "dev-alpha").with_chunk_bytes(chunk);
    let receiving = SessionConfig::new(DOMAIN, "node-1");
    let push = tokio::spawn(async move {
        match budget {
            Some(n) => push_session(ar, DiesAfter::new(aw, n), &*source, &pushing).await,
            None => push_session(ar, aw, &*source, &pushing).await,
        }
    });
    let recv = tokio::spawn(async move { receive_session(br, bw, &vault, &receiving).await });
    (
        push.await.expect("push task"),
        recv.await.expect("receive task"),
    )
}

/// A writer that dies after a fixed number of bytes — a connection cut at a
/// deterministic point, with no timing in the test.
struct DiesAfter<W> {
    inner: W,
    budget: usize,
}

impl<W> DiesAfter<W> {
    fn new(inner: W, budget: usize) -> DiesAfter<W> {
        DiesAfter { inner, budget }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for DiesAfter<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let me = self.get_mut();
        if me.budget == 0 {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "test: the connection died mid-transfer",
            )));
        }
        let take = buf.len().min(me.budget);
        match Pin::new(&mut me.inner).poll_write(cx, &buf[..take]) {
            Poll::Ready(Ok(n)) => {
                me.budget -= n;
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn vault_at(domain: &TestDomain, name: &str) -> DirVault {
    DirVault::open(domain.device_dir(name)).expect("vault opens")
}

fn segment_bytes(dir: &std::path::Path, device: &str, start_seq: u64) -> Vec<u8> {
    std::fs::read(hive_core::oplog::segment_path(dir, device, start_seq))
        .expect("segment file is readable")
}

/// The two-dir push: everything the device holds ends up in the vault,
/// byte-identical, and a store opened on those files serves the same state.
#[tokio::test]
async fn a_push_session_replicates_a_device_into_an_empty_dir() {
    require_smoke!();
    let domain = test_domain();
    let alpha = Arc::new(domain.open_device("alpha"));
    seed(&alpha, 8).await;
    let blocks = seed_blocks(&alpha, 4, 4096).await;
    let vault = vault_at(&domain, "vault");

    let (push, recv) = run_session(alpha.clone(), vault.clone(), 4096, None).await;
    let push = push.expect("push completes");
    let recv = recv.expect("receive completes");

    let device_heads = alpha.sync_heads().await.unwrap();
    assert_eq!(push.segments_offered, device_heads.segments.len());
    assert_eq!(push.blocks_offered, blocks.len());
    assert_eq!(recv.segments_completed, device_heads.segments.len());
    assert_eq!(recv.blocks_landed, blocks.len());
    assert_eq!(push.blocks_acked, blocks.len());
    assert!(
        !push.segments_acked.is_empty(),
        "the receiver reports what it landed, chunk by chunk"
    );

    // The oracle: the vault's own enumeration of what it holds is identical
    // to the device's, file hash included. Nothing was re-encoded.
    assert_eq!(
        vault.heads().unwrap(),
        device_heads,
        "a replicated dir must describe itself exactly as the device does"
    );
    for seg in &device_heads.segments {
        assert_eq!(
            segment_bytes(vault.root(), &seg.device, seg.start_seq),
            segment_bytes(alpha.data_dir(), &seg.device, seg.start_seq),
            "segment {}/{} is not verbatim",
            seg.device,
            seg.start_seq
        );
    }
    let mut landed_blocks = BlockStore::open(vault.root())
        .unwrap()
        .list_block_ids()
        .unwrap();
    landed_blocks.sort_unstable();
    assert_eq!(landed_blocks, blocks);

    // A second session has nothing to do — the have/want diff is the whole
    // point of the offer.
    let (push2, recv2) = run_session(alpha.clone(), vault.clone(), 4096, None).await;
    let (push2, recv2) = (push2.unwrap(), recv2.unwrap());
    assert_eq!(push2.segment_bytes_sent, 0, "nothing re-transfers");
    assert_eq!(push2.blocks_sent, 0);
    assert_eq!(recv2.segments_up_to_date, device_heads.segments.len());
    assert_eq!(recv2.blocks_landed, 0);

    // And it is a restore: the files plus the device id open as a store that
    // heals into the same canonical state (PR 4.8's CLI formalises the copy;
    // this pins what it has to produce).
    let dump = alpha.canonical_dump().await.unwrap();
    assert!(!dump.is_empty());
    std::fs::copy(
        domain.device_dir("alpha").join("device"),
        domain.device_dir("vault").join("device"),
    )
    .unwrap();
    let restored = domain.open_device("vault");
    assert_eq!(
        restored.canonical_dump().await.unwrap(),
        dump,
        "verbatim files plus heal must reproduce the device's state"
    );
    assert!(
        !restored.search("rehearsal", 10).await.unwrap().is_empty(),
        "the healed index must answer, not just hold rows"
    );
    restored.shutdown().await.unwrap();
    Arc::try_unwrap(alpha)
        .unwrap_or_else(|_| unreachable!("no other holder"))
        .shutdown()
        .await
        .unwrap();
}

/// A transport with a tiny buffer must not wedge the session.
///
/// The hazard is specific and was real: the pusher answers a whole block batch
/// before it reads anything, so a receiver that acked between blocks would
/// fill the socket with acks nobody was draining, block on its own write, stop
/// reading, and deadlock the pair. Acks are therefore deferred to the end of
/// the batch. The timeout is what turns a regression into a failure instead of
/// a hung CI job.
#[tokio::test]
async fn a_small_transport_buffer_does_not_wedge_a_block_batch() {
    require_smoke!();
    let domain = test_domain();
    let alpha = Arc::new(domain.open_device("alpha"));
    seed(&alpha, 2).await;
    let blocks = seed_blocks(&alpha, 40, 2048).await;
    let vault = vault_at(&domain, "vault");

    let run = run_session_on(alpha.clone(), vault.clone(), 4096, None, 1024);
    let (push, recv) = tokio::time::timeout(std::time::Duration::from_secs(20), run)
        .await
        .expect("a session must not deadlock on a small transport buffer");
    let (push, recv) = (
        push.expect("push completes"),
        recv.expect("receive completes"),
    );
    assert_eq!(recv.blocks_landed, blocks.len());
    assert_eq!(push.blocks_acked, blocks.len(), "every block is acked");

    let mut landed = BlockStore::open(vault.root())
        .unwrap()
        .list_block_ids()
        .unwrap();
    landed.sort_unstable();
    assert_eq!(landed, blocks);
    Arc::try_unwrap(alpha)
        .unwrap_or_else(|_| unreachable!("no other holder"))
        .shutdown()
        .await
        .unwrap();
}

/// A connection cut mid-segment: the next session resumes from the last WHOLE
/// frame the vault holds, re-sends exactly the rest, and never rewrites a byte
/// of what was already there.
///
/// The partial frame the cut left behind is dropped rather than kept — nobody
/// ever acknowledged it, and the frozen writer treats its own torn tail the
/// same way.
#[tokio::test]
async fn a_killed_segment_transfer_resumes_from_the_landed_offset() {
    require_smoke!();
    let domain = test_domain();
    let alpha = Arc::new(domain.open_device("alpha"));
    seed(&alpha, 16).await;
    let vault = vault_at(&domain, "vault");

    let device_heads = alpha.sync_heads().await.unwrap();
    let total = device_heads.total_len();
    let chunk = 4096;
    let budget = 12_000; // the offer plus about three chunks
    assert!(
        total > budget as u64 * 2,
        "the seed must be long enough that the cut lands mid-transfer ({total} bytes)"
    );

    let (push, recv) = run_session(alpha.clone(), vault.clone(), chunk, Some(budget)).await;
    assert!(push.is_err(), "the pushing side's connection died");
    assert!(recv.is_err(), "and the receiver saw it end mid-stream");

    let after_kill = vault.heads().unwrap();
    assert_eq!(after_kill.segments.len(), 1, "one segment, partly landed");
    let device = after_kill.segments[0].device.clone();
    let raw = after_kill.segments[0].len;
    assert!(
        raw > 0 && raw < total,
        "a partial landing is the point: {raw} of {total}"
    );
    // The cut fell wherever the chunk boundary did, so the file may end mid
    // frame. What survives is the whole-frame prefix — that is the resume
    // point, and it is a walkable segment on its own.
    let cut = segment_bytes(vault.root(), &device, 1);
    let walk = hive_core::oplog::walk_segment(&cut).expect("the landed bytes are a segment");
    let resume_at = walk.whole_end;
    assert!(
        !walk.frames.is_empty(),
        "at least one whole frame must have landed for this to test a resume"
    );

    let (push2, recv2) = run_session(alpha.clone(), vault.clone(), chunk, None).await;
    let (push2, recv2) = (
        push2.expect("resume completes"),
        recv2.expect("resume completes"),
    );
    assert_eq!(
        push2.segment_bytes_sent,
        total - resume_at,
        "a resume re-sends exactly what was not already whole, and nothing else"
    );
    assert_eq!(recv2.segments_completed, 1);
    assert_eq!(vault.heads().unwrap(), device_heads);

    let full = segment_bytes(vault.root(), &device, 1);
    assert_eq!(
        &full[..resume_at as usize],
        &cut[..resume_at as usize],
        "the frames that were already landed must not have been rewritten"
    );
    Arc::try_unwrap(alpha)
        .unwrap_or_else(|_| unreachable!("no other holder"))
        .shutdown()
        .await
        .unwrap();
}

/// The same for blocks: a block is all-or-nothing, so a cut mid-blob leaves
/// whole blocks landed and the rest simply absent — and the next session's
/// want-list is exactly the difference.
#[tokio::test]
async fn a_killed_blob_transfer_resumes_without_resending_landed_blocks() {
    require_smoke!();
    let domain = test_domain();
    let alpha = Arc::new(domain.open_device("alpha"));
    seed(&alpha, 2).await;
    let blocks = seed_blocks(&alpha, 6, 4096).await;
    let vault = vault_at(&domain, "vault");

    // Enough budget for the whole log plus a couple of blocks, so the cut
    // falls inside the block phase.
    let segments = alpha.sync_heads().await.unwrap().total_len();
    let budget = (segments + 4096 * 2 + 2048) as usize;
    let (push, recv) = run_session(alpha.clone(), vault.clone(), 1024 * 1024, Some(budget)).await;
    assert!(push.is_err() && recv.is_err(), "the connection died");

    let landed = BlockStore::open(vault.root())
        .unwrap()
        .list_block_ids()
        .unwrap();
    assert!(
        !landed.is_empty() && landed.len() < blocks.len(),
        "the cut must land some blocks and not others (landed {})",
        landed.len()
    );

    let (push2, recv2) = run_session(alpha.clone(), vault.clone(), 1024 * 1024, None).await;
    let (push2, recv2) = (
        push2.expect("resume completes"),
        recv2.expect("resume completes"),
    );
    assert_eq!(
        push2.blocks_sent,
        blocks.len() - landed.len(),
        "only the blocks that were missing move"
    );
    assert_eq!(recv2.blocks_landed, blocks.len() - landed.len());

    let mut all = BlockStore::open(vault.root())
        .unwrap()
        .list_block_ids()
        .unwrap();
    all.sort_unstable();
    assert_eq!(all, blocks, "every block ends up in the vault");
    Arc::try_unwrap(alpha)
        .unwrap_or_else(|_| unreachable!("no other holder"))
        .shutdown()
        .await
        .unwrap();
}

/// The active tail is just a segment whose length grows. A later session
/// extends it from the landed end — it never re-sends, and never rewrites
/// the prefix it is extending.
#[tokio::test]
async fn a_growing_tail_extends_with_its_prefix_intact() {
    require_smoke!();
    let domain = test_domain();
    let alpha = Arc::new(domain.open_device("alpha"));
    seed(&alpha, 4).await;
    let vault = vault_at(&domain, "vault");

    let (push, recv) = run_session(alpha.clone(), vault.clone(), 4096, None).await;
    push.expect("first push");
    recv.expect("first receive");
    let first = alpha.sync_heads().await.unwrap();
    assert_eq!(vault.heads().unwrap(), first);
    let device = first.segments[0].device.clone();
    let prefix = segment_bytes(vault.root(), &device, 1);
    assert_eq!(prefix.len() as u64, first.segments[0].len);

    // The device keeps working; the tail grows in place.
    seed(&alpha, 5).await;
    let grown = alpha.sync_heads().await.unwrap();
    assert_eq!(grown.segments.len(), 1, "still one segment, just longer");
    assert!(grown.segments[0].len > first.segments[0].len);

    // A chunk big enough for the whole extension, so the byte count is exact:
    // a chunk boundary inside a frame costs that frame's bytes a second time
    // on the following round, which is correct but not what this asserts.
    let (push2, recv2) = run_session(alpha.clone(), vault.clone(), 1024 * 1024, None).await;
    let (push2, recv2) = (push2.expect("second push"), recv2.expect("second receive"));
    assert_eq!(
        push2.segment_bytes_sent,
        grown.segments[0].len - first.segments[0].len,
        "only the extension travels"
    );
    assert_eq!(recv2.segments_completed, 1);
    assert_eq!(
        vault.heads().unwrap(),
        grown,
        "the extended tail hashes to what the device announced"
    );

    let extended = segment_bytes(vault.root(), &device, 1);
    assert_eq!(
        &extended[..prefix.len()],
        &prefix[..],
        "an extension may not touch a single byte of what it extends"
    );
    Arc::try_unwrap(alpha)
        .unwrap_or_else(|_| unreachable!("no other holder"))
        .shutdown()
        .await
        .unwrap();
}
