// The landing side of a session (PLAN-v2.1 PR 4.4): where verbatim bytes go,
// and the rules they have to obey to get there.
//
// `DirVault` is the minimum receiver that makes the protocol testable and the
// M1 story true: a plain directory in exact store shape — `log/<device>/*.seg`
// plus `blocks/<hh>/<id>` — holding ciphertext and no keys, which is what
// restore then copies back and heals from (D29). The node's SegmentVault (PR
// 4.6) is this plus the bookkeeping a server needs: `node-meta.db`, quotas,
// the pending-forget queue, and integrity ALARMS where this one simply
// refuses. The refusals are deliberately the same shape, so 4.6 escalates
// rather than reinterprets:
//
//   * whole frames only — a chunk that ends mid-frame lands its whole-frame
//     prefix and nothing else, so every offset this vault reports is an
//     offset a sender can legally extend from;
//   * extend at the landed end or not at all — a write at any other offset is
//     refused, which is the keyless half of "a growing tail may only extend
//     with its existing byte prefix intact";
//   * the header must say what the sender said — the first chunk of a new
//     segment is parsed (keylessly) and its (device, start_seq) must match
//     the identity it was offered under, so bytes can never be filed under a
//     name that is not theirs;
//   * ids are the address — blocks land through `BlockStore::put_block`,
//     which re-hashes and refuses misaddressed content.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use hive_core::blockstore::BlockStore;
use hive_core::oplog::{self, HeadsSnapshot};

/// What a receiver already holds of one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandedSegment {
    /// Bytes on disk, always ending on a frame boundary — the offset the next
    /// `WantSegment` resumes from.
    pub len: u64,
    /// blake3 of exactly those bytes, so a complete segment can be checked
    /// against the sender's announced whole-file hash.
    pub file_hash: [u8; 32],
    /// Whole frames landed (diagnostics; the wire never carries it).
    pub frames: usize,
}

/// What the receiving half of a session lands.
#[async_trait]
pub trait SyncSink: Send + Sync {
    /// The resume point for one segment, or `None` for a segment never seen —
    /// a WHOLE-frame offset. Repairing a torn tail (a chunk boundary that fell
    /// mid-frame, then a session that never came back) is this call's job, not
    /// the session's, so what it reports is always an offset a sender can
    /// legally extend from.
    async fn landed(&self, device: &str, start_seq: u64) -> Result<Option<LandedSegment>>;

    /// Append verbatim bytes at `at_offset`, returning the segment's new
    /// length on disk. `at_offset` must equal the current length; anything
    /// else is a refusal, not a seek.
    ///
    /// The returned length is the raw one, not the whole-frame one, and that
    /// is deliberate: chunk boundaries have nothing to do with frame
    /// boundaries, so a transfer that only ever advanced by whole frames
    /// would stall outright on any record longer than one chunk. Bytes stream
    /// contiguously within a session; the whole-frame rule is what governs
    /// where the NEXT session picks up, which is exactly the frozen writer's
    /// own torn-tail recovery semantics.
    async fn extend_segment(
        &self,
        device: &str,
        start_seq: u64,
        at_offset: u64,
        bytes: Vec<u8>,
    ) -> Result<u64>;

    /// Whether this bare ciphertext id is already held.
    async fn has_block(&self, id: [u8; 32]) -> Result<bool>;

    /// Land one block under its bare id, verifying the content address.
    async fn put_block(&self, id: [u8; 32], bytes: Vec<u8>) -> Result<()>;
}

/// A directory holding verbatim segments and ciphertext blocks in store
/// shape. Holds no keys, opens no index, folds nothing.
#[derive(Clone)]
pub struct DirVault {
    inner: Arc<VaultDir>,
}

struct VaultDir {
    root: PathBuf,
    blocks: BlockStore,
}

impl DirVault {
    /// Open (creating if absent) a vault at `root`. The blocks half is the
    /// store's own `BlockStore`, so the directory a vault writes is the
    /// directory a store reads.
    pub fn open(root: impl Into<PathBuf>) -> Result<DirVault> {
        let root = root.into();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating vault root {}", root.display()))?;
        let blocks = BlockStore::open(&root)
            .with_context(|| format!("opening vault blockstore under {}", root.display()))?;
        Ok(DirVault {
            inner: Arc::new(VaultDir { root, blocks }),
        })
    }

    /// The directory this vault owns.
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    /// Where a given segment lands — the frozen naming rule, not a guess.
    pub fn segment_path(&self, device: &str, start_seq: u64) -> PathBuf {
        oplog::segment_path(&self.inner.root, device, start_seq)
    }

    /// Everything landed here, in the same snapshot shape a store reports.
    /// This is what makes a vault comparable to the device it backs up.
    pub fn heads(&self) -> Result<HeadsSnapshot> {
        oplog::heads(&self.inner.root)
    }
}

impl VaultDir {
    fn landed(&self, device: &str, start_seq: u64) -> Result<Option<LandedSegment>> {
        let path = self.checked_path(device, start_seq)?;
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let walk = oplog::walk_segment(&bytes)
            .with_context(|| format!("walking landed segment {}", path.display()))?;
        if walk.partial_tail > 0 {
            // A previous session stopped on a chunk boundary that fell inside
            // a frame. Those bytes were never part of anything the sender can
            // be held to, so they are cut before the offset is reported: what
            // this returns is always a whole-frame resume point.
            truncate(&path, walk.whole_end)?;
        }
        let landed = &bytes[..walk.whole_end as usize];
        Ok(Some(LandedSegment {
            len: walk.whole_end,
            file_hash: *blake3::hash(landed).as_bytes(),
            frames: walk.frames.len(),
        }))
    }

    fn extend_segment(
        &self,
        device: &str,
        start_seq: u64,
        at_offset: u64,
        bytes: &[u8],
    ) -> Result<u64> {
        let path = self.checked_path(device, start_seq)?;
        let have = self.raw_len(&path)?;
        if at_offset != have {
            bail!(
                "refusing to write segment {device}/{start_seq} at offset {at_offset}: \
                 the file is {have} bytes, and a tail may only be extended at its end"
            );
        }
        if bytes.is_empty() {
            return Ok(have);
        }

        if have == 0 {
            // A new segment: the bytes must SAY they are this segment before
            // a file is created under this name.
            let header = oplog::parse_header(bytes).with_context(|| {
                format!("parsing the first chunk of segment {device}/{start_seq}")
            })?;
            if header.device != device || header.start_seq != start_seq {
                bail!(
                    "segment bytes carry the header of {}/{}, but were offered as {device}/{start_seq}",
                    header.device,
                    header.start_seq
                );
            }
            let dir = path.parent().expect("segment path has a parent");
            std::fs::create_dir_all(dir)
                .with_context(|| format!("creating vault log dir {}", dir.display()))?;
            // Written whole and renamed into place: a segment file never
            // exists with a partial HEADER, so `landed` can treat an
            // unparseable header as real corruption rather than a maybe. A
            // partial frame at the end is a different thing entirely, and
            // `landed` cuts that one.
            write_atomic(&path, bytes)?;
        } else {
            append(&path, bytes)?;
        }
        self.raw_len(&path)
    }

    /// Bytes on disk, 0 for a segment not yet seen.
    fn raw_len(&self, path: &Path) -> Result<u64> {
        match std::fs::metadata(path) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e).with_context(|| format!("sizing {}", path.display())),
        }
    }

    /// The segment path for a device id that has been checked against the
    /// writer's allowlist — the string arrives from the network and becomes a
    /// directory name.
    fn checked_path(&self, device: &str, start_seq: u64) -> Result<PathBuf> {
        if !oplog::device_id_ok(device) {
            bail!("refusing vault access for device id {device:?} (outside the allowlist)");
        }
        Ok(oplog::segment_path(&self.root, device, start_seq))
    }
}

#[async_trait]
impl SyncSink for DirVault {
    async fn landed(&self, device: &str, start_seq: u64) -> Result<Option<LandedSegment>> {
        let inner = self.inner.clone();
        let device = device.to_string();
        tokio::task::spawn_blocking(move || inner.landed(&device, start_seq))
            .await
            .context("vault landed-length task")?
    }

    async fn extend_segment(
        &self,
        device: &str,
        start_seq: u64,
        at_offset: u64,
        bytes: Vec<u8>,
    ) -> Result<u64> {
        let inner = self.inner.clone();
        let device = device.to_string();
        tokio::task::spawn_blocking(move || {
            inner.extend_segment(&device, start_seq, at_offset, &bytes)
        })
        .await
        .context("vault segment-extend task")?
    }

    async fn has_block(&self, id: [u8; 32]) -> Result<bool> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || Ok(inner.blocks.has_block(&id)))
            .await
            .context("vault block-probe task")?
    }

    async fn put_block(&self, id: [u8; 32], bytes: Vec<u8>) -> Result<()> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.blocks.put_block(&id, &bytes))
            .await
            .context("vault block-write task")?
    }
}

fn append(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .with_context(|| format!("opening {} to extend", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("appending {} bytes to {}", bytes.len(), path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsyncing {}", path.display()))?;
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("fsyncing {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} into place", tmp.display()))?;
    sync_dir(path.parent().expect("segment path has a parent"))
}

fn truncate(path: &Path, len: u64) -> Result<()> {
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to truncate", path.display()))?;
    file.set_len(len)
        .with_context(|| format!("truncating {} to {len}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("fsyncing {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> Result<()> {
    std::fs::File::open(dir)
        .and_then(|f| f.sync_all())
        .with_context(|| format!("fsyncing dir {}", dir.display()))
}

/// Windows cannot open a directory handle this way; the rename is already
/// atomic there, so durability of the directory entry is the filesystem's.
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}
