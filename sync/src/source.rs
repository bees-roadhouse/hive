// The serving side of a session (PLAN-v2.1 PR 4.4): what a peer must be able
// to answer to push its log and blocks somewhere.
//
// It is a trait rather than a bare `&Store` for two reasons: the node's
// SegmentVault becomes a source too when the traffic reverses (restore, PR
// 4.8), and a test needs to be able to serve deliberately hostile answers
// without a store to back them.
//
// Everything here is READ-only and keyless. A blind receiver never sees a
// plaintext record, and this side never needs the master key to serve one:
// segment bytes go out verbatim (D29 — segment boundaries feed the frozen key
// derivation, so re-encoding is unsafe and byte-identical transfer is what
// preserves the determinism contract), and blocks go out under their bare
// ciphertext ids.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hive_core::oplog::{self, HeadsSnapshot};
use hive_core::store::Store;

use crate::frame::MAX_SEGMENT_CHUNK;

/// What the pushing half of a session serves.
#[async_trait]
pub trait SyncSource: Send + Sync {
    /// Every segment this peer holds — the offer a session opens with.
    async fn heads(&self) -> Result<HeadsSnapshot>;

    /// Verbatim bytes of one segment starting at `from_offset`, at most `max`
    /// of them. Fewer is fine and normal (that is a chunk); zero means there
    /// was nothing left to serve, which the session treats as a refusal.
    async fn segment_bytes(
        &self,
        device: &str,
        start_seq: u64,
        from_offset: u64,
        max: u64,
    ) -> Result<Vec<u8>>;

    /// Every block id this peer holds — the blocks half of the offer.
    async fn block_ids(&self) -> Result<Vec<[u8; 32]>>;

    /// One block's exact ciphertext bytes, by bare id.
    async fn block_bytes(&self, id: [u8; 32]) -> Result<Vec<u8>>;
}

/// A device serves its own store. Enumeration and block reads ride
/// `Store::sync_*` (PR 4.3) — the writer thread, for ordering — while segment
/// bytes are read straight off disk through the public `segment_path`:
/// appended bytes are immutable and fsynced before they are ever announced,
/// so serving a prefix of the tail cannot observe a half-written batch.
#[async_trait]
impl SyncSource for Store {
    async fn heads(&self) -> Result<HeadsSnapshot> {
        self.sync_heads().await
    }

    async fn segment_bytes(
        &self,
        device: &str,
        start_seq: u64,
        from_offset: u64,
        max: u64,
    ) -> Result<Vec<u8>> {
        // The device id arrives from the network and becomes a directory
        // name. Checked here as well as in the session: any SyncSource
        // implementation is one `segment_path` away from a traversal, so the
        // check belongs at the filesystem edge too.
        if !oplog::device_id_ok(device) {
            bail!("refusing a segment request for device id {device:?} (outside the allowlist)");
        }
        let path = oplog::segment_path(self.data_dir(), device, start_seq);
        let max = max.min(MAX_SEGMENT_CHUNK);
        tokio::task::spawn_blocking(move || read_range(&path, from_offset, max))
            .await
            .context("segment read task")?
    }

    async fn block_ids(&self) -> Result<Vec<[u8; 32]>> {
        self.sync_block_ids().await
    }

    async fn block_bytes(&self, id: [u8; 32]) -> Result<Vec<u8>> {
        self.sync_get_block(id).await
    }
}

/// Read up to `max` bytes of `path` from `from`. A request past EOF is an
/// error rather than an empty answer: it means the two sides disagree about
/// what exists, which is worth a loud session failure.
fn read_range(path: &Path, from: u64, max: u64) -> Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("opening segment {} to serve", path.display()))?;
    let len = file
        .metadata()
        .with_context(|| format!("sizing segment {}", path.display()))?
        .len();
    if from > len {
        bail!(
            "segment {} is {len} bytes; asked to resume at {from}",
            path.display()
        );
    }
    // The take is bounded by the cap first, so the buffer is sized by protocol
    // constant and file length — never by anything the peer said.
    let take = (len - from).min(max) as usize;
    file.seek(SeekFrom::Start(from))
        .with_context(|| format!("seeking segment {} to {from}", path.display()))?;
    let mut buf = vec![0u8; take];
    file.read_exact(&mut buf)
        .with_context(|| format!("reading {take} bytes of segment {}", path.display()))?;
    Ok(buf)
}

/// `<root>/log/<device>/<start_seq:016x>.seg`, re-exported so a caller that
/// holds a plain directory (a vault, a restore target) names segments exactly
/// the way the store does.
pub fn segment_path(root: &Path, device: &str, start_seq: u64) -> PathBuf {
    oplog::segment_path(root, device, start_seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_read_is_bounded_by_the_file_and_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bytes");
        std::fs::write(&path, (0u8..100).collect::<Vec<u8>>()).unwrap();

        assert_eq!(
            read_range(&path, 0, 10).unwrap(),
            (0u8..10).collect::<Vec<_>>()
        );
        assert_eq!(
            read_range(&path, 90, 1024).unwrap(),
            (90u8..100).collect::<Vec<_>>()
        );
        // Exactly at EOF: legal, and empty — the session reads that as
        // "nothing left", never as a silent success.
        assert!(read_range(&path, 100, 16).unwrap().is_empty());
        assert!(read_range(&path, 101, 16).is_err(), "past EOF is loud");
    }
}
