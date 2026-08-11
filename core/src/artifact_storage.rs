// Where an artifact's BYTES live. The database row (store/artifacts.rs) IS the
// artifact; this is the driver that holds its content, and it is the only place
// that knows whether that content is a file, an S3 object, or anything else.
//
// ── Layout ──────────────────────────────────────────────────────────────────
//
//   <data_root>/artifacts/<org_id>/<hh>/<sha256>
//
// where sha256 is the lowercase hex of the bytes and <hh> is its first two hex
// chars (fan-out dirs, so no directory ever holds the whole corpus). Content
// addressed, so the object-storage swap is a driver change and not a data
// migration: the same relative string becomes the S3 key under a bucket prefix.
//
// ── Why the org is IN the address ───────────────────────────────────────────
//
// Dedup is keyed (org_id, sha256) and `artifacts` is RLS-scoped on org_id, so
// the question a delete has to answer — "is anything still referencing these
// bytes?" — is only answerable WITHIN the acting org. A globally shared address
// would make that count wrong in the dangerous direction: org A deleting its
// last row would unlink bytes org B still holds, and getting the right answer
// would need a SECURITY DEFINER escape hatch out of the very policy that is
// supposed to be unbypassable. Putting the org in the address makes the
// RLS-visible refcount exactly the right one and puts a tenant boundary between
// one org's DELETE and another org's bytes. Dedup inside a household (the same
// photo imported twice, one attachment on two mails) is where the wins actually
// are and is untouched by this.
//
// ── Streaming ───────────────────────────────────────────────────────────────
//
// Nothing here ever holds a whole artifact in memory. Writes arrive as chunks
// and go straight to a temp file that is hashed on the way past; reads hand
// back an `AsyncRead` over a byte range. A 200 MB video costs a buffer, not
// 200 MB of RSS, in both directions.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

/// What one committed write produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// Lowercase hex sha256 of everything written.
    pub sha256: String,
    pub bytes: u64,
    /// False when the content address already held these bytes — a dedup hit,
    /// nothing new was written.
    pub deduped: bool,
}

pub type ByteReader = Box<dyn AsyncRead + Send + Unpin>;

/// The storage driver. Local path today, object storage later behind exactly
/// this surface.
#[async_trait]
pub trait ArtifactStorage: Send + Sync + 'static {
    /// Start a streamed write for `org`. The bytes land at their content
    /// address on `commit`, not before, so a write abandoned halfway leaves no
    /// addressable object.
    async fn begin(&self, org: Uuid) -> Result<Box<dyn ArtifactWrite>>;

    /// Byte length of the stored object, or None when it is absent.
    async fn size(&self, org: Uuid, sha256: &str) -> Result<Option<u64>>;

    /// A reader over `[offset, offset + len)`. Callers clamp to the object's
    /// size first; a range past the end simply yields fewer bytes.
    async fn read_range(
        &self,
        org: Uuid,
        sha256: &str,
        offset: u64,
        len: u64,
    ) -> Result<ByteReader>;

    /// Remove the object. Idempotent — an absent object is Ok.
    async fn remove(&self, org: Uuid, sha256: &str) -> Result<()>;
}

/// One in-flight write. Dropped without `commit`, it cleans up after itself on
/// a best-effort basis and leaves nothing at a content address.
#[async_trait]
pub trait ArtifactWrite: Send {
    async fn write(&mut self, chunk: &[u8]) -> Result<()>;
    /// Land the bytes at their content address and report what they were.
    async fn commit(self: Box<Self>) -> Result<Stored>;
}

/// `<data_root>` — `$HIVE_DATA_DIR`, else `./data`.
pub fn data_root() -> PathBuf {
    std::env::var("HIVE_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data"))
}

/// The process-wide driver the HTTP layer uses. Resolved once from the
/// environment; the store methods take `&dyn ArtifactStorage` instead so tests
/// can hand in their own root.
pub fn storage() -> &'static dyn ArtifactStorage {
    static S: OnceLock<LocalArtifactStorage> = OnceLock::new();
    S.get_or_init(|| LocalArtifactStorage::new(data_root().join("artifacts")))
}

// ---- local path driver ----

pub struct LocalArtifactStorage {
    root: PathBuf,
}

impl LocalArtifactStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `<root>/<org>/<hh>/<sha256>`. Both components are validated by their
    /// types (`Uuid`) or by `object_path`'s hex check, so nothing a caller
    /// supplies can escape the root.
    fn object_path(&self, org: Uuid, sha256: &str) -> Result<PathBuf> {
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("not a sha256 content address: {sha256:?}");
        }
        Ok(self
            .org_root(org)
            .join(&sha256[..2])
            .join(sha256.to_ascii_lowercase()))
    }

    fn org_root(&self, org: Uuid) -> PathBuf {
        self.root.join(org.simple().to_string())
    }
}

#[async_trait]
impl ArtifactStorage for LocalArtifactStorage {
    async fn begin(&self, org: Uuid) -> Result<Box<dyn ArtifactWrite>> {
        // "tmp" cannot collide with a fan-out dir (those are two hex chars).
        let dir = self.org_root(org).join("tmp");
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating artifact temp dir {}", dir.display()))?;
        let tmp = dir.join(format!("{}.part", Uuid::new_v4().simple()));
        let file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("creating {}", tmp.display()))?;
        Ok(Box::new(LocalWrite {
            file: Some(file),
            tmp,
            org,
            root: self.root.clone(),
            hasher: Sha256::new(),
            bytes: 0,
        }))
    }

    async fn size(&self, org: Uuid, sha256: &str) -> Result<Option<u64>> {
        let path = self.object_path(org, sha256)?;
        match tokio::fs::metadata(&path).await {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("stat {}", path.display())),
        }
    }

    async fn read_range(
        &self,
        org: Uuid,
        sha256: &str,
        offset: u64,
        len: u64,
    ) -> Result<ByteReader> {
        let path = self.object_path(org, sha256)?;
        let mut file = tokio::fs::File::open(&path)
            .await
            .with_context(|| format!("opening {}", path.display()))?;
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .with_context(|| format!("seeking to {offset} in {}", path.display()))?;
        }
        Ok(Box::new(tokio::io::AsyncReadExt::take(file, len)))
    }

    async fn remove(&self, org: Uuid, sha256: &str) -> Result<()> {
        let path = self.object_path(org, sha256)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
}

struct LocalWrite {
    /// Taken by `commit`; `Drop` uses its absence to know the write landed.
    file: Option<tokio::fs::File>,
    tmp: PathBuf,
    org: Uuid,
    root: PathBuf,
    hasher: Sha256,
    bytes: u64,
}

#[async_trait]
impl ArtifactWrite for LocalWrite {
    async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        let file = self
            .file
            .as_mut()
            .expect("write after commit is not reachable: commit consumes self");
        file.write_all(chunk)
            .await
            .with_context(|| format!("writing {}", self.tmp.display()))?;
        self.hasher.update(chunk);
        self.bytes += chunk.len() as u64;
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<Stored> {
        let mut file = self.file.take().expect("commit consumes self exactly once");
        file.flush().await.context("flushing artifact temp file")?;
        file.sync_all().await.context("fsync artifact temp file")?;
        drop(file);

        let sha256 = hex::encode(std::mem::take(&mut self.hasher).finalize());
        let path = self
            .root
            .join(self.org.simple().to_string())
            .join(&sha256[..2])
            .join(&sha256);

        // Dedup: these exact bytes are already at this exact address. Keep what
        // is there (identical by construction) and drop the temp.
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let _ = tokio::fs::remove_file(&self.tmp).await;
            return Ok(Stored {
                sha256,
                bytes: self.bytes,
                deduped: true,
            });
        }

        let dir = path.parent().expect("object path has a fan-out dir");
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating artifact dir {}", dir.display()))?;
        tokio::fs::rename(&self.tmp, &path)
            .await
            .with_context(|| format!("landing artifact at {}", path.display()))?;
        Ok(Stored {
            sha256,
            bytes: self.bytes,
            deduped: false,
        })
    }
}

impl Drop for LocalWrite {
    fn drop(&mut self) {
        // Committed writes took the file handle; anything else was abandoned
        // (a failed upload, a dropped connection) and its temp goes with it.
        if self.file.is_some() {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// `<data_root>/artifacts` under an explicit root — the shape tests want.
pub fn local_storage_at(data_root: &Path) -> LocalArtifactStorage {
    LocalArtifactStorage::new(data_root.join("artifacts"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn tmp_root() -> PathBuf {
        std::env::temp_dir().join(format!("hive-artifact-storage-{}", Uuid::new_v4().simple()))
    }

    async fn read_all(mut r: ByteReader) -> Vec<u8> {
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        out
    }

    #[tokio::test]
    async fn round_trip_dedup_and_ranges() {
        let root = tmp_root();
        let s = LocalArtifactStorage::new(root.join("artifacts"));
        let org = Uuid::new_v4();

        let mut w = s.begin(org).await.unwrap();
        w.write(b"hello ").await.unwrap();
        w.write(b"world").await.unwrap();
        let first = w.commit().await.unwrap();
        assert_eq!(first.bytes, 11);
        assert!(!first.deduped);
        // sha256("hello world")
        assert_eq!(
            first.sha256,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(s.size(org, &first.sha256).await.unwrap(), Some(11));

        // Same bytes again: one stored file, reported as a dedup hit.
        let mut w = s.begin(org).await.unwrap();
        w.write(b"hello world").await.unwrap();
        let second = w.commit().await.unwrap();
        assert_eq!(second.sha256, first.sha256);
        assert!(second.deduped);

        // Ranges come back exactly.
        let all = read_all(s.read_range(org, &first.sha256, 0, 11).await.unwrap()).await;
        assert_eq!(all, b"hello world");
        let mid = read_all(s.read_range(org, &first.sha256, 6, 5).await.unwrap()).await;
        assert_eq!(mid, b"world");

        // Another org is a different address: its own bytes, its own delete.
        let other = Uuid::new_v4();
        assert_eq!(s.size(other, &first.sha256).await.unwrap(), None);

        s.remove(org, &first.sha256).await.unwrap();
        assert_eq!(s.size(org, &first.sha256).await.unwrap(), None);
        // Idempotent.
        s.remove(org, &first.sha256).await.unwrap();

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn abandoned_write_leaves_nothing() {
        let root = tmp_root();
        let s = LocalArtifactStorage::new(root.join("artifacts"));
        let org = Uuid::new_v4();

        let mut w = s.begin(org).await.unwrap();
        w.write(b"never committed").await.unwrap();
        drop(w);

        let tmp_dir = root
            .join("artifacts")
            .join(org.simple().to_string())
            .join("tmp");
        let leftovers = std::fs::read_dir(&tmp_dir).map(|d| d.count()).unwrap_or(0);
        assert_eq!(
            leftovers, 0,
            "an abandoned write must not leave a temp file"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_content_address_cannot_escape_the_root() {
        let s = LocalArtifactStorage::new("root");
        let org = Uuid::new_v4();
        assert!(s.object_path(org, "../../etc/passwd").is_err());
        assert!(s.object_path(org, "").is_err());
        assert!(s.object_path(org, &"z".repeat(64)).is_err());
        assert!(s.object_path(org, &"a".repeat(64)).is_ok());
    }
}
