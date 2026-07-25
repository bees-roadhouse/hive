// The core sync read surface (PLAN-v2.1 PR 4.3, D29): the async half of
// `oplog::keyless` plus the blockstore's bare-id block APIs, so the desktop's
// push loop (PR 4.8) and the hive-sync session (PR 4.4) drive replication
// through the same Store handle every other feature uses — no second door
// into the data dir, no second opinion about what is on disk.
//
// Read-only except `sync_put_block`, which is how a verbatim block lands
// coming back down. There is deliberately no segment-WRITE API: restore is
// verbatim files plus the store's own heal (D29), and foreign ingest arrives
// at PR 4.10 as a writer-thread op of its own.
//
// Everything rides `Store::run` — the single writer thread — for ORDERING,
// not access: that thread owns the log tail, so a job can never observe a
// half-appended batch. An enumeration racing `commit` from another thread
// could otherwise hash a segment prefix no fsync ever acknowledged.

use anyhow::Result;

use super::Store;
use crate::blockstore::BlobRef;
use crate::oplog::{self, HeadsSnapshot, SegmentInfo};

impl Store {
    /// Every device's segments in this data dir — the snapshot a session
    /// exchanges before deciding what to ask for.
    pub async fn sync_heads(&self) -> Result<HeadsSnapshot> {
        let data_dir = self.data_dir.clone();
        self.run(move |_core| oplog::heads(&data_dir)).await
    }

    /// One device's segments, oldest first: `(device, start_seq, len, sealed,
    /// blake3-of-file)` per file. `device` may be this store's own
    /// (`Store::device`) or any replica it has landed.
    pub async fn sync_segments(&self, device: &str) -> Result<Vec<SegmentInfo>> {
        let data_dir = self.data_dir.clone();
        let device = device.to_string();
        self.run(move |_core| oplog::list_segments(&data_dir, &device))
            .await
    }

    /// Every block id this store holds, sorted. Says nothing about which blob
    /// a block belongs to — that is `sync_blob_block_ids`.
    pub async fn sync_block_ids(&self) -> Result<Vec<[u8; 32]>> {
        self.run(|core| core.blocks.list_block_ids()).await
    }

    /// The block ids one blob is made of (chunks in order, manifest last).
    /// The keyed half of "is this blob complete?" — ask `sync_has_block`
    /// about each id; `BlockStore::has` alone only probes the manifest.
    pub async fn sync_blob_block_ids(&self, blob: BlobRef) -> Result<Vec<[u8; 32]>> {
        self.run(move |core| {
            let keys = core.keys.clone();
            core.blocks.blob_block_ids(keys.as_ref(), &blob)
        })
        .await
    }

    /// Existence probe for one bare ciphertext id.
    pub async fn sync_has_block(&self, id: [u8; 32]) -> Result<bool> {
        self.run(move |core| Ok(core.blocks.has_block(&id))).await
    }

    /// One block's exact ciphertext bytes, verified against its content
    /// address — what a peer stores verbatim under the same id.
    pub async fn sync_get_block(&self, id: [u8; 32]) -> Result<Vec<u8>> {
        self.run(move |core| core.blocks.get_block(&id)).await
    }

    /// Land one block by bare id, blake3-verify-on-write: bytes that do not
    /// hash to `id` are refused.
    pub async fn sync_put_block(&self, id: [u8; 32], bytes: Vec<u8>) -> Result<()> {
        self.run(move |core| core.blocks.put_block(&id, &bytes))
            .await
    }
}
