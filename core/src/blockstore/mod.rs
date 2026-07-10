// The content-addressed encrypted blockstore (PR 1.4, D19/D20): payload
// bodies — mail bodies, file text, page captures, attachments — live here as
// encrypted blocks; log records carry a `BlobRef` (blob reference + wrapped
// content key). Destroying every wrapped copy of a blob's key IS the hard
// delete: the ciphertext blocks become noise (crypto-shredding, D19).
//
// THE LAYOUT AND DERIVATIONS BELOW ARE FROZEN (PR 1.4).
//
// ── On-disk layout ──────────────────────────────────────────────────────────
//
//   <data_dir>/blocks/<hh>/<block_id>
//
// where block_id is the lowercase blake3 hex (64 chars) of the ENCRYPTED
// block bytes exactly as stored, and <hh> is its first two hex chars (fan-out
// dirs). A block file is pure ciphertext+tag — no header, no nonce (nonces
// are derived, see below). Content addressing over ciphertext is what D20
// wants: sync can verify and dedup transfers without ever seeing plaintext.
//
// ── Keys (convergent-with-secret, D19 + D20) ────────────────────────────────
//
//   content_key = blake3::keyed_hash(
//       blake3::derive_key("hive-blob-v1", master),
//       plaintext_blake3)                       // 32 bytes, one per blob
//
// The key is a PRF of the WHOLE plaintext's blake3 under a master-derived
// subkey. Consequences, all deliberate:
//   - identical plaintext ⇒ identical key ⇒ identical ciphertext blocks:
//     dedup works within one master-key domain (v1's 30–60% mail-attachment
//     dedup carries over);
//   - nobody outside the master-key domain can mount the classic convergent
//     encryption confirmation-of-file attack (the secret keys the PRF);
//   - the wrapped copy of content_key in BlobRef is the blob's life: destroy
//     every wrapped copy and the blocks are unrecoverable noise.
// Dedup granularity is the whole blob: two DIFFERENT plaintexts never share
// a block (their content keys differ, so even a shared prefix encrypts
// differently). delete() therefore can never orphan another blob's chunks.
// (Chunk-level cross-blob dedup would need per-chunk keys — a different,
// rejected trade; see DIRECTION.md D19/D20.)
//
// ── Nonces (deterministic, no randomness in this module) ────────────────────
//
//   nonce_key = blake3::derive_key("hive-blob-nonce-v1", content_key)
//   nonce(i)  = blake3::keyed_hash(nonce_key, u64_le(i))[..24]   // chunk i
//   manifest uses the reserved index i = u64::MAX
//
// Why this is safe: XChaCha20-Poly1305 requires (key, nonce) uniqueness per
// distinct plaintext. content_key is unique per plaintext within a master
// domain, and each index is used for exactly one fixed chunk of that
// plaintext — so a repeated (key, nonce) pair can only ever recreate the
// identical ciphertext (that repetition is the dedup working as designed).
// Chunk counts are bounded far below u64::MAX (64 KiB minimum chunk size),
// so the manifest's reserved index cannot collide with a chunk index.
//
// ── Chunking ────────────────────────────────────────────────────────────────
//
// Payloads up to SINGLE_BLOCK_CUTOFF (256 KiB) are one chunk, no CDC pass.
// Larger payloads go through FastCDC (v2020 gear table) with
// min/avg/max = 64/256/1024 KiB — the classic ratios (avg/4, avg, avg*4)
// around a 256 KiB average chosen to keep manifests small while still giving
// sync resumable ~256 KiB units (D20/iroh alignment).
//
// ── Manifest (CBOR, frozen field order) ─────────────────────────────────────
//
// Every blob has a manifest — single-chunk blobs get a one-entry manifest so
// readers have exactly one shape. Manifest plaintext is a CBOR map:
//
//   "v"       u8     manifest version (=1)
//   "total"   u64    plaintext byte length
//   "mime"    text?  optional mime type (null when absent)
//   "chunks"  array  of maps: { "h": bytes32 block id, "n": u64 plaintext len }
//
// The manifest is itself a block: encrypted under content_key with the
// reserved nonce index and stored at blake3(ciphertext) like any chunk.
// BlobRef.manifest_hash is that block id.
//
// Same plaintext with a different mime shares every chunk block but gets its
// own manifest block (mime lives inside the manifest bytes).
//
// ── Determinism boundary ────────────────────────────────────────────────────
//
// Like the oplog, this module reads no clocks, no environment, no randomness
// (enforced by core/tests/determinism.rs). put() is a pure function of
// (master key, bytes, mime) — byte-identical BlobRef on every call — which is
// also what makes dedup detectable by simple equality.
//
// Synchronous std::fs on purpose: from PR 1.6 this sits behind the store's
// single writer thread.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use crate::keys::{self, KeySource};
use crate::oplog::{bytes32, bytesvec};

/// Payloads at or below this many bytes skip CDC and become a single chunk.
pub const SINGLE_BLOCK_CUTOFF: usize = 256 * 1024;
/// FastCDC bounds (see module header for the rationale).
pub const CDC_MIN: usize = 64 * 1024;
pub const CDC_AVG: usize = 256 * 1024;
pub const CDC_MAX: usize = 1024 * 1024;

/// blake3 derive_key contexts (frozen).
const BLOB_KEY_CONTEXT: &str = "hive-blob-v1";
const BLOB_NONCE_CONTEXT: &str = "hive-blob-nonce-v1";

/// Nonce index reserved for the manifest block.
const MANIFEST_NONCE_INDEX: u64 = u64::MAX;

/// Everything needed to find, decrypt, and verify one blob. This is what log
/// records carry (CBOR-friendly: hashes and the wrapped key encode as byte
/// strings). Destroying every stored copy of `wrapped_key` is the hard
/// delete (D19).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlobRef {
    /// Block id of the encrypted manifest (blake3 of ciphertext).
    #[serde(with = "bytes32")]
    pub manifest_hash: [u8; 32],
    /// content_key wrapped under the master key (keys::wrap_key, 72 bytes).
    #[serde(with = "bytesvec")]
    pub wrapped_key: Vec<u8>,
    /// Plaintext byte length.
    pub size: u64,
    /// Optional mime type (also inside the encrypted manifest).
    pub mime: Option<String>,
    /// blake3 of the plaintext; get() verifies against it.
    #[serde(with = "bytes32")]
    pub plaintext_hash: [u8; 32],
}

/// Manifest plaintext (see module header). Private: BlobRef is the API.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    v: u8,
    total: u64,
    mime: Option<String>,
    chunks: Vec<ChunkEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkEntry {
    /// Block id (blake3 of the encrypted chunk).
    #[serde(with = "bytes32")]
    h: [u8; 32],
    /// Plaintext length of this chunk.
    n: u64,
}

const MANIFEST_VERSION: u8 = 1;

pub struct BlockStore {
    root: PathBuf,
}

impl BlockStore {
    /// Open (creating if absent) the blockstore at `<data_dir>/blocks/`.
    pub fn open(data_dir: &Path) -> Result<BlockStore> {
        let root = data_dir.join("blocks");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating blockstore root {}", root.display()))?;
        Ok(BlockStore { root })
    }

    /// Store `bytes` as an encrypted, content-addressed blob. Deterministic
    /// and idempotent: the same (master key, bytes, mime) always produces the
    /// same blocks and the byte-identical `BlobRef`; blocks that already
    /// exist are not rewritten (that is the dedup).
    pub fn put(&self, keys: &dyn KeySource, bytes: &[u8], mime: Option<&str>) -> Result<BlobRef> {
        let master = keys.master_key()?;
        let plaintext_hash = *blake3::hash(bytes).as_bytes();
        let content_key = derive_content_key(&master, &plaintext_hash);
        let nonce_key = blake3::derive_key(BLOB_NONCE_CONTEXT, &content_key);

        let mut entries = Vec::new();
        for (i, chunk) in chunk_spans(bytes).into_iter().enumerate() {
            let ct = aead_seal(&content_key, &nonce_at(&nonce_key, i as u64), chunk)?;
            let id = *blake3::hash(&ct).as_bytes();
            self.write_block(&id, &ct)?;
            entries.push(ChunkEntry {
                h: id,
                n: chunk.len() as u64,
            });
        }

        let manifest = Manifest {
            v: MANIFEST_VERSION,
            total: bytes.len() as u64,
            mime: mime.map(str::to_string),
            chunks: entries,
        };
        let mut manifest_cbor = Vec::with_capacity(64 + 48 * manifest.chunks.len());
        ciborium::into_writer(&manifest, &mut manifest_cbor).context("encoding manifest")?;
        let manifest_ct = aead_seal(
            &content_key,
            &nonce_at(&nonce_key, MANIFEST_NONCE_INDEX),
            &manifest_cbor,
        )?;
        let manifest_hash = *blake3::hash(&manifest_ct).as_bytes();
        self.write_block(&manifest_hash, &manifest_ct)?;

        Ok(BlobRef {
            manifest_hash,
            wrapped_key: keys::wrap_key(&master, &content_key)?,
            size: bytes.len() as u64,
            mime: mime.map(str::to_string),
            plaintext_hash,
        })
    }

    /// Fetch and decrypt a blob, verifying every block id, every chunk
    /// length, and finally the whole plaintext against
    /// `blob.plaintext_hash` and `blob.size`.
    pub fn get(&self, keys: &dyn KeySource, blob: &BlobRef) -> Result<Vec<u8>> {
        let (content_key, manifest) = self.read_manifest(keys, blob)?;
        let nonce_key = blake3::derive_key(BLOB_NONCE_CONTEXT, &content_key);
        let mut out = Vec::with_capacity(manifest.total as usize);
        for (i, entry) in manifest.chunks.iter().enumerate() {
            let ct = self
                .read_block(&entry.h)
                .with_context(|| format!("chunk {i} of blob"))?;
            let pt = aead_open(&content_key, &nonce_at(&nonce_key, i as u64), &ct)
                .with_context(|| format!("decrypting chunk {i}"))?;
            if pt.len() as u64 != entry.n {
                bail!(
                    "chunk {i} decrypts to {} bytes, manifest says {}",
                    pt.len(),
                    entry.n
                );
            }
            out.extend_from_slice(&pt);
        }
        if out.len() as u64 != blob.size {
            bail!(
                "blob reassembles to {} bytes, BlobRef says {}",
                out.len(),
                blob.size
            );
        }
        if *blake3::hash(&out).as_bytes() != blob.plaintext_hash {
            bail!("blob plaintext hash mismatch — blocks corrupted or wrong BlobRef");
        }
        Ok(out)
    }

    /// True when the blob's manifest block is present (the cheap existence
    /// probe used by sync-style have/want checks).
    pub fn has(&self, blob: &BlobRef) -> bool {
        self.block_path(&blob.manifest_hash).is_file()
    }

    /// Remove a blob's blocks from disk: chunks first, manifest last, dir
    /// entries fsynced. Idempotent: a missing manifest is a no-op Ok.
    ///
    /// Needs the key source (a deliberate widening of the bare
    /// `delete(&BlobRef)` sketch): the chunk list lives inside the encrypted
    /// manifest, and enumerating it requires the content key. Block removal
    /// is the belt-and-suspenders half of deletion — the load-bearing half
    /// is destroying the wrapped key (D19), which works even where a block
    /// file outlives us (a copied disk, a synced peer): without the key the
    /// bytes are noise.
    pub fn delete(&self, keys: &dyn KeySource, blob: &BlobRef) -> Result<()> {
        if !self.has(blob) {
            return Ok(());
        }
        let (_, manifest) = self.read_manifest(keys, blob)?;
        for entry in &manifest.chunks {
            self.remove_block(&entry.h)?;
        }
        self.remove_block(&blob.manifest_hash)?;
        Ok(())
    }

    fn read_manifest(&self, keys: &dyn KeySource, blob: &BlobRef) -> Result<([u8; 32], Manifest)> {
        let master = keys.master_key()?;
        let content_key =
            keys::unwrap_key(&master, &blob.wrapped_key).context("unwrapping blob content key")?;
        let nonce_key = blake3::derive_key(BLOB_NONCE_CONTEXT, &content_key);
        let ct = self
            .read_block(&blob.manifest_hash)
            .context("reading manifest block")?;
        let pt = aead_open(
            &content_key,
            &nonce_at(&nonce_key, MANIFEST_NONCE_INDEX),
            &ct,
        )
        .context("decrypting manifest")?;
        let manifest: Manifest =
            ciborium::from_reader(pt.as_slice()).context("decoding manifest CBOR")?;
        if manifest.v != MANIFEST_VERSION {
            bail!("unsupported manifest version {}", manifest.v);
        }
        if manifest.total != blob.size {
            bail!(
                "manifest total {} disagrees with BlobRef size {}",
                manifest.total,
                blob.size
            );
        }
        Ok((content_key, manifest))
    }

    /// `<root>/<hh>/<64-hex>` for a block id.
    fn block_path(&self, id: &[u8; 32]) -> PathBuf {
        let hex = data_encoding::HEXLOWER.encode(id);
        self.root.join(&hex[..2]).join(hex)
    }

    /// Write a block if absent: temp file in the fan-out dir, fsync, rename
    /// to the content address, fsync the dir. The temp name is derived from
    /// the block id (single-writer store; a stale temp from a crash just
    /// gets overwritten and renamed next time).
    fn write_block(&self, id: &[u8; 32], bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let path = self.block_path(id);
        if path.is_file() {
            return Ok(()); // dedup hit
        }
        let dir = path.parent().expect("block path has a fan-out dir");
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating block dir {}", dir.display()))?;
        let tmp = path.with_extension("tmp");
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(bytes).context("writing block")?;
        f.sync_all().context("fsync block")?;
        drop(f);
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming block into place at {}", path.display()))?;
        sync_dir(dir)?;
        Ok(())
    }

    fn read_block(&self, id: &[u8; 32]) -> Result<Vec<u8>> {
        let path = self.block_path(id);
        let bytes =
            std::fs::read(&path).with_context(|| format!("reading block {}", path.display()))?;
        if *blake3::hash(&bytes).as_bytes() != *id {
            bail!(
                "block {} fails its content address (bit rot?)",
                path.display()
            );
        }
        Ok(bytes)
    }

    fn remove_block(&self, id: &[u8; 32]) -> Result<()> {
        let path = self.block_path(id);
        match std::fs::remove_file(&path) {
            Ok(()) => {
                if let Some(dir) = path.parent() {
                    sync_dir(dir)?;
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing block {}", path.display())),
        }
    }
}

/// Frozen content-key derivation (see module header).
fn derive_content_key(master: &[u8; 32], plaintext_hash: &[u8; 32]) -> [u8; 32] {
    let subkey = blake3::derive_key(BLOB_KEY_CONTEXT, master);
    *blake3::keyed_hash(&subkey, plaintext_hash).as_bytes()
}

/// Frozen nonce derivation (see module header).
fn nonce_at(nonce_key: &[u8; 32], index: u64) -> [u8; 24] {
    let digest = blake3::keyed_hash(nonce_key, &index.to_le_bytes());
    let mut nonce = [0u8; 24];
    nonce.copy_from_slice(&digest.as_bytes()[..24]);
    nonce
}

/// Split a payload into chunk spans: one span up to the cutoff, FastCDC
/// (64/256/1024 KiB) above it. A zero-length payload is one empty chunk, so
/// every blob has at least one block and manifests have one shape.
fn chunk_spans(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.len() <= SINGLE_BLOCK_CUTOFF {
        return vec![bytes];
    }
    fastcdc::v2020::FastCDC::new(bytes, CDC_MIN, CDC_AVG, CDC_MAX)
        .map(|c| &bytes[c.offset..c.offset + c.length])
        .collect()
}

fn aead_seal(key: &[u8; 32], nonce: &[u8; 24], plaintext: &[u8]) -> Result<Vec<u8>> {
    XChaCha20Poly1305::new(Key::from_slice(key))
        .encrypt(XNonce::from_slice(nonce), plaintext)
        .map_err(|e| anyhow!("block encryption failed: {e}"))
}

fn aead_open(key: &[u8; 32], nonce: &[u8; 24], ciphertext: &[u8]) -> Result<Vec<u8>> {
    XChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| anyhow!("block authentication failed — corrupted block or wrong key"))
}

/// fsync a directory (no-op on non-Unix). Same rationale as the oplog's.
fn sync_dir(dir: &Path) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_spans_cutoff_boundary() {
        let small = vec![7u8; SINGLE_BLOCK_CUTOFF];
        assert_eq!(chunk_spans(&small).len(), 1);
        let empty: Vec<u8> = Vec::new();
        assert_eq!(chunk_spans(&empty).len(), 1);
        let big = vec![7u8; SINGLE_BLOCK_CUTOFF + 1];
        let spans = chunk_spans(&big);
        assert!(!spans.is_empty());
        assert_eq!(spans.iter().map(|s| s.len()).sum::<usize>(), big.len());
        for s in &spans {
            assert!(s.len() <= CDC_MAX);
        }
    }

    #[test]
    fn nonce_indexes_do_not_collide_with_manifest() {
        let key = [3u8; 32];
        assert_ne!(nonce_at(&key, 0), nonce_at(&key, 1));
        assert_ne!(nonce_at(&key, 0), nonce_at(&key, MANIFEST_NONCE_INDEX));
    }
}
