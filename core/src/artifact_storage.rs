// Where an artifact's BYTES live. The database row (store/artifacts.rs) IS the
// artifact; this is the driver that holds its content, and it is the only place
// that knows whether that content is a file, an S3 object, or anything else.
//
// ── Layout ──────────────────────────────────────────────────────────────────
//
//   <data_root>/artifacts/<org_id>/<hh>/<sha256>   published objects
//   <data_root>/artifacts/<org_id>/tmp/<uuid>.part uploads still streaming
//   <data_root>/artifacts/<org_id>/trash/<sha>.<uuid>  bytes a delete moved aside
//
// where sha256 is the lowercase hex of the bytes and <hh> is its first two hex
// chars (fan-out dirs, so no directory ever holds the whole corpus). Content
// addressed, so the object-storage swap is a driver change and not a data
// migration: the same relative string becomes the S3 key under a bucket prefix.
// "tmp" and "trash" cannot collide with a fan-out dir — those are two hex chars.
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
// ── Two-phase writes, because the lock is keyed on the content address ──────
//
// A write is sealed (`finish`) and published (`land`) separately. Sealing
// flushes, fsyncs, and hashes; it does NOT put anything at the content address.
// Publishing is a dedup re-check plus a rename, and the store calls it while
// holding the advisory lock for that address — see store/artifacts.rs. Fusing
// the two is what let a concurrent delete unlink bytes a row was about to point
// at: the upload dedup-hit against a file it held no lock on, the delete counted
// zero rows and unlinked, and the upload's INSERT then landed a row whose bytes
// were gone.
//
// ── Nothing is unlinked while a row might still need it ─────────────────────
//
// A delete moves bytes to `trash/` (a rename, atomic, reversible) under the same
// lock, and only unlinks them once the row's removal is committed. If that
// commit fails or the process dies in between, the bytes are still on disk and
// the sweeper puts them back or finishes removing them. The failure mode is
// litter, never a live row pointing at nothing.
//
// ── Streaming ───────────────────────────────────────────────────────────────
//
// Nothing here ever holds a whole artifact in memory. Writes arrive as chunks
// and go straight to a temp file that is hashed on the way past; reads hand
// back an `AsyncRead` over a byte range. A 200 MB video costs a buffer, not
// 200 MB of RSS, in both directions. The ceiling is enforced per chunk against
// the running total, so an endless stream is cut off at the limit rather than
// when the filesystem fills.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncSeekExt, AsyncWriteExt};
use uuid::Uuid;

const TMP_DIR: &str = "tmp";
const TRASH_DIR: &str = "trash";
const PART_SUFFIX: &str = ".part";

/// Ceiling on ONE artifact when nothing says otherwise. 512 MiB: big enough for
/// the 200 MB video docs/ARTIFACTS.md streams through this path and for a
/// full-resolution scan batch, small enough that a single hostile request
/// cannot decide how much disk this install has left. It is a per-upload cap,
/// not a quota — there is no per-org byte budget in this tree, and this comment
/// is not promising one.
const DEFAULT_MAX_ARTIFACT_BYTES: u64 = 512 * 1024 * 1024;

/// The per-upload ceiling: `$HIVE_MAX_ARTIFACT_BYTES`, else 512 MiB. An
/// unparseable or zero value falls back to the default rather than disabling
/// the cap — "no limit" is not a thing this accepts.
pub fn max_artifact_bytes() -> u64 {
    std::env::var("HIVE_MAX_ARTIFACT_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_MAX_ARTIFACT_BYTES)
}

/// A write that crossed the ceiling. Carried through `anyhow` so the HTTP layer
/// can `downcast_ref` it into a 413 instead of a 500.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLarge {
    pub limit: u64,
}

impl std::fmt::Display for TooLarge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "artifact exceeds the {} byte limit", self.limit)
    }
}

impl std::error::Error for TooLarge {}

/// What one published write produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stored {
    /// Lowercase hex sha256 of everything written.
    pub sha256: String,
    pub bytes: u64,
    /// False when the content address already held these bytes — a dedup hit,
    /// nothing new was written.
    pub deduped: bool,
}

/// Bytes moved out of their content address by a delete that has not finished.
/// They are still on disk and can go either way: back to the address, or gone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trashed {
    /// The content address these bytes belong at.
    pub sha256: String,
    /// Driver-private handle to where they are now.
    pub key: String,
}

/// One thing the sweeper may act on. The driver reports; the store decides,
/// because only the store can ask whether a row still references the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sweepable {
    /// A partial upload nobody finished — a dropped connection, a rejected
    /// oversize stream, a process that died mid-write.
    Temp { key: String, age: Duration },
    /// Bytes a delete moved aside and never finished disposing of.
    Trashed(Trashed),
    /// An object sitting at its content address. Possibly orphaned: an upload
    /// that landed its bytes and then failed to commit its row leaves one.
    Object { sha256: String, age: Duration },
}

pub type ByteReader = Box<dyn AsyncRead + Send + Unpin>;

/// The storage driver. Local path today, object storage later behind exactly
/// this surface.
#[async_trait]
pub trait ArtifactStorage: Send + Sync + 'static {
    /// Start a streamed write for `org`. The bytes land at their content
    /// address on `land`, not before, so a write abandoned halfway leaves no
    /// addressable object.
    async fn begin(&self, org: Uuid) -> Result<Box<dyn ArtifactWrite>>;

    /// Byte length of the stored object, or None when it is absent.
    async fn size(&self, org: Uuid, sha256: &str) -> Result<Option<u64>>;

    /// A reader over `[offset, offset + len)`, or None when the object is gone
    /// — a delete that raced this read is a 404, not a 500. Callers clamp to
    /// the object's size first; a range past the end simply yields fewer bytes.
    async fn read_range(
        &self,
        org: Uuid,
        sha256: &str,
        offset: u64,
        len: u64,
    ) -> Result<Option<ByteReader>>;

    /// Move the object out of its content address, atomically. None when there
    /// was nothing there, so this is idempotent. The bytes survive until
    /// `purge`, which is what makes a delete recoverable.
    async fn trash(&self, org: Uuid, sha256: &str) -> Result<Option<Trashed>>;

    /// Unlink trashed bytes for good. Idempotent.
    async fn purge(&self, org: Uuid, trashed: &Trashed) -> Result<()>;

    /// Put trashed bytes back at their content address. Idempotent, and a
    /// no-op-plus-purge when something is already there — content addressing
    /// means whatever is there is these same bytes.
    async fn restore(&self, org: Uuid, trashed: &Trashed) -> Result<()>;

    /// Drop an abandoned partial upload by its key. Idempotent.
    async fn discard_temp(&self, org: Uuid, key: &str) -> Result<()>;

    /// Everything in one org's subtree the sweeper might act on.
    async fn sweepable(&self, org: Uuid) -> Result<Vec<Sweepable>>;

    /// Every org holding a subtree here — the sweeper's entry point, and the
    /// only way to find bytes belonging to an org whose rows are all gone.
    async fn orgs(&self) -> Result<Vec<Uuid>>;
}

/// One in-flight write. Dropped without `finish`, it cleans up after itself on
/// a best-effort basis and leaves nothing at a content address.
#[async_trait]
pub trait ArtifactWrite: Send {
    /// Append a chunk. Errors with [`TooLarge`] once the running total would
    /// cross the driver's ceiling — checked BEFORE the chunk is written, so
    /// nothing past the limit ever reaches the disk.
    async fn write(&mut self, chunk: &[u8]) -> Result<()>;

    /// Seal the bytes and compute their content address WITHOUT publishing
    /// them. Cheap to abandon: the temp goes with the returned handle.
    async fn finish(self: Box<Self>) -> Result<Box<dyn StagedArtifact>>;
}

/// Sealed bytes with a known content address, not yet at it. Dropped without
/// `land`, the temp file goes with it.
#[async_trait]
pub trait StagedArtifact: Send {
    /// Lowercase hex sha256 — the key the store's advisory lock is taken on.
    fn sha256(&self) -> &str;
    fn bytes(&self) -> u64;

    /// Publish at the content address, re-checking for a dedup hit as it goes.
    /// The caller MUST already hold the blob lock for `sha256()`: that re-check
    /// is only meaningful if a delete cannot run between it and the rename.
    async fn land(self: Box<Self>) -> Result<Stored>;
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
    max_bytes: u64,
}

impl LocalArtifactStorage {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_bytes: max_artifact_bytes(),
        }
    }

    /// Override the per-upload ceiling. Tests want a small one; production
    /// takes it from the environment via [`max_artifact_bytes`].
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    fn org_root(&self, org: Uuid) -> PathBuf {
        self.root.join(org.simple().to_string())
    }

    /// `<root>/<org>/<hh>/<sha256>`. Every path in this module goes through
    /// [`normalized_sha`] first, so nothing a caller supplies can escape the
    /// root and the fan-out dir can never disagree with the filename on case.
    fn object_path(&self, org: Uuid, sha256: &str) -> Result<PathBuf> {
        object_path_in(&self.root, org, sha256)
    }

    fn tmp_root(&self, org: Uuid) -> PathBuf {
        self.org_root(org).join(TMP_DIR)
    }

    fn trash_root(&self, org: Uuid) -> PathBuf {
        self.org_root(org).join(TRASH_DIR)
    }

    /// `<root>/<org>/trash/<sha256>.<uuid>`, validated back out of the key so a
    /// sweeper acting on driver-reported names still cannot leave the subtree.
    fn trash_path(&self, org: Uuid, key: &str) -> Result<PathBuf> {
        let (sha, suffix) = key
            .split_once('.')
            .ok_or_else(|| anyhow::anyhow!("not a trash key: {key:?}"))?;
        let sha = normalized_sha(sha)?;
        if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("not a trash key: {key:?}");
        }
        Ok(self.trash_root(org).join(format!("{sha}.{suffix}")))
    }

    /// `<root>/<org>/tmp/<uuid>.part`, validated the same way.
    fn temp_path(&self, org: Uuid, key: &str) -> Result<PathBuf> {
        let stem = key
            .strip_suffix(PART_SUFFIX)
            .ok_or_else(|| anyhow::anyhow!("not a temp key: {key:?}"))?;
        if stem.len() != 32 || !stem.bytes().all(|b| b.is_ascii_hexdigit()) {
            anyhow::bail!("not a temp key: {key:?}");
        }
        Ok(self.tmp_root(org).join(key))
    }
}

/// 64 hex characters, lowercased. The one chokepoint: a content address that is
/// not exactly this never becomes a path component.
fn normalized_sha(sha256: &str) -> Result<String> {
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("not a sha256 content address: {sha256:?}");
    }
    Ok(sha256.to_ascii_lowercase())
}

fn object_path_in(root: &Path, org: Uuid, sha256: &str) -> Result<PathBuf> {
    let sha = normalized_sha(sha256)?;
    Ok(root
        .join(org.simple().to_string())
        .join(&sha[..2])
        .join(&sha))
}

/// Unlink off the async runtime. `Drop` is synchronous and cannot await, and a
/// blocking `remove_file` inside it stalls whichever executor thread it lands
/// on. Anything this misses is litter the sweeper collects.
fn remove_in_background(path: PathBuf) {
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            rt.spawn_blocking(move || {
                let _ = std::fs::remove_file(path);
            });
        }
        Err(_) => {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// How long ago a directory entry was last written, saturating at zero for a
/// clock that moved backwards.
fn age_of(meta: &std::fs::Metadata) -> Duration {
    meta.modified()
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
        .unwrap_or_default()
}

#[async_trait]
impl ArtifactStorage for LocalArtifactStorage {
    async fn begin(&self, org: Uuid) -> Result<Box<dyn ArtifactWrite>> {
        let dir = self.tmp_root(org);
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating artifact temp dir {}", dir.display()))?;
        let tmp = dir.join(format!("{}{PART_SUFFIX}", Uuid::new_v4().simple()));
        let file = tokio::fs::File::create(&tmp)
            .await
            .with_context(|| format!("creating {}", tmp.display()))?;
        Ok(Box::new(LocalWrite {
            file: Some(file),
            tmp: Some(tmp),
            org,
            root: self.root.clone(),
            hasher: Sha256::new(),
            bytes: 0,
            max_bytes: self.max_bytes,
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
    ) -> Result<Option<ByteReader>> {
        let path = self.object_path(org, sha256)?;
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("opening {}", path.display())),
        };
        if offset > 0 {
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .with_context(|| format!("seeking to {offset} in {}", path.display()))?;
        }
        Ok(Some(Box::new(tokio::io::AsyncReadExt::take(file, len))))
    }

    async fn trash(&self, org: Uuid, sha256: &str) -> Result<Option<Trashed>> {
        let sha = normalized_sha(sha256)?;
        let from = self.object_path(org, &sha)?;
        let dir = self.trash_root(org);
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("creating artifact trash dir {}", dir.display()))?;
        let key = format!("{sha}.{}", Uuid::new_v4().simple());
        let to = dir.join(&key);
        // The rename IS the existence check: no window between looking and
        // moving, so two callers cannot both believe they took the bytes.
        match tokio::fs::rename(&from, &to).await {
            Ok(()) => Ok(Some(Trashed { sha256: sha, key })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e)
                .with_context(|| format!("moving {} aside to {}", from.display(), to.display())),
        }
    }

    async fn purge(&self, org: Uuid, trashed: &Trashed) -> Result<()> {
        let path = self.trash_path(org, &trashed.key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    async fn restore(&self, org: Uuid, trashed: &Trashed) -> Result<()> {
        let from = self.trash_path(org, &trashed.key)?;
        let to = self.object_path(org, &trashed.sha256)?;
        // Something is already at the address: content addressing says it is
        // these same bytes, so the trashed copy is redundant rather than newer.
        // Purging beats overwriting, which would yank the file out from under
        // an in-flight read.
        if tokio::fs::try_exists(&to).await.unwrap_or(false) {
            return self.purge(org, trashed).await;
        }
        let dir = to.parent().expect("object path has a fan-out dir");
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating artifact dir {}", dir.display()))?;
        match tokio::fs::rename(&from, &to).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => {
                Err(e).with_context(|| format!("restoring {} to {}", from.display(), to.display()))
            }
        }
    }

    async fn discard_temp(&self, org: Uuid, key: &str) -> Result<()> {
        let path = self.temp_path(org, key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }

    async fn sweepable(&self, org: Uuid) -> Result<Vec<Sweepable>> {
        let mut out = Vec::new();

        // Abandoned partial uploads.
        if let Some(mut rd) = read_dir_opt(&self.tmp_root(org)).await? {
            while let Some(entry) = rd.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if meta.is_file() && self.temp_path(org, &name).is_ok() {
                    out.push(Sweepable::Temp {
                        key: name,
                        age: age_of(&meta),
                    });
                }
            }
        }

        // Bytes a delete moved aside and did not finish disposing of.
        if let Some(mut rd) = read_dir_opt(&self.trash_root(org)).await? {
            while let Some(entry) = rd.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Some((sha, _)) = name.split_once('.') else {
                    continue;
                };
                let Ok(sha) = normalized_sha(sha) else {
                    continue;
                };
                if self.trash_path(org, &name).is_ok() {
                    out.push(Sweepable::Trashed(Trashed {
                        sha256: sha,
                        key: name,
                    }));
                }
            }
        }

        // Published objects, one fan-out dir at a time. O(corpus) per sweep,
        // which is why this runs on a long interval rather than per request.
        let Some(mut fanout) = read_dir_opt(&self.org_root(org)).await? else {
            return Ok(out);
        };
        while let Some(dir) = fanout.next_entry().await? {
            let dir_name = dir.file_name().to_string_lossy().into_owned();
            if dir_name.len() != 2 || !dir_name.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue; // tmp, trash, or something nothing here wrote
            }
            let Some(mut objects) = read_dir_opt(&dir.path()).await? else {
                continue;
            };
            while let Some(entry) = objects.next_entry().await? {
                let name = entry.file_name().to_string_lossy().into_owned();
                let Ok(sha) = normalized_sha(&name) else {
                    continue;
                };
                if !sha.starts_with(&dir_name.to_ascii_lowercase()) {
                    continue; // not at its own address; not ours to reason about
                }
                let Ok(meta) = entry.metadata().await else {
                    continue;
                };
                if meta.is_file() {
                    out.push(Sweepable::Object {
                        sha256: sha,
                        age: age_of(&meta),
                    });
                }
            }
        }
        Ok(out)
    }

    async fn orgs(&self) -> Result<Vec<Uuid>> {
        let Some(mut rd) = read_dir_opt(&self.root).await? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        while let Some(entry) = rd.next_entry().await? {
            if let Ok(org) = Uuid::parse_str(&entry.file_name().to_string_lossy()) {
                out.push(org);
            }
        }
        Ok(out)
    }
}

/// `read_dir`, with "there is no such directory" as None rather than an error —
/// an org that has never stored anything is normal, not a failure.
async fn read_dir_opt(path: &Path) -> Result<Option<tokio::fs::ReadDir>> {
    match tokio::fs::read_dir(path).await {
        Ok(rd) => Ok(Some(rd)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("listing {}", path.display())),
    }
}

struct LocalWrite {
    /// Taken by `finish`, which is the only thing that closes the handle.
    file: Option<tokio::fs::File>,
    /// Ownership of the temp file. Moves to the staged handle as the LAST step
    /// of `finish`, after every fallible step, so a failure anywhere in there
    /// still leaves `Drop` holding the cleanup. (The previous shape used
    /// `file.is_some()` as the was-abandoned signal and took the file first, so
    /// an ENOSPC on flush or fsync leaked the `.part` — a disk-full event
    /// turning itself into permanent litter that keeps the disk full.)
    tmp: Option<PathBuf>,
    org: Uuid,
    root: PathBuf,
    hasher: Sha256,
    bytes: u64,
    max_bytes: u64,
}

#[async_trait]
impl ArtifactWrite for LocalWrite {
    async fn write(&mut self, chunk: &[u8]) -> Result<()> {
        // Checked against the running total BEFORE the write, so the limit is
        // the most that ever reaches the disk. Enforcing after the stream ends
        // would be no enforcement at all: the stream is the attack.
        let after = self.bytes.saturating_add(chunk.len() as u64);
        if after > self.max_bytes {
            return Err(anyhow::Error::new(TooLarge {
                limit: self.max_bytes,
            }));
        }
        let Self { file, tmp, .. } = &mut *self;
        let file = file
            .as_mut()
            .expect("write after finish is not reachable: finish consumes self");
        file.write_all(chunk).await.with_context(|| match tmp {
            Some(p) => format!("writing {}", p.display()),
            None => "writing artifact temp file".to_string(),
        })?;
        self.hasher.update(chunk);
        self.bytes = after;
        Ok(())
    }

    async fn finish(mut self: Box<Self>) -> Result<Box<dyn StagedArtifact>> {
        let mut file = self.file.take().expect("finish consumes self exactly once");
        file.flush().await.context("flushing artifact temp file")?;
        file.sync_all().await.context("fsync artifact temp file")?;
        drop(file);

        let sha256 = hex::encode(std::mem::take(&mut self.hasher).finalize());
        let path = object_path_in(&self.root, self.org, &sha256)?;
        let tmp = self.tmp.take().expect("finish consumes self exactly once");
        Ok(Box::new(StagedLocal {
            tmp: Some(tmp),
            path,
            sha256,
            bytes: self.bytes,
        }))
    }
}

impl Drop for LocalWrite {
    fn drop(&mut self) {
        // Whatever still owns a temp path was abandoned — a failed upload, a
        // dropped connection, a stream cut off at the ceiling.
        if let Some(tmp) = self.tmp.take() {
            remove_in_background(tmp);
        }
    }
}

struct StagedLocal {
    /// Cleared once the bytes are at their address (or deliberately dropped).
    tmp: Option<PathBuf>,
    path: PathBuf,
    sha256: String,
    bytes: u64,
}

#[async_trait]
impl StagedArtifact for StagedLocal {
    fn sha256(&self) -> &str {
        &self.sha256
    }

    fn bytes(&self) -> u64 {
        self.bytes
    }

    async fn land(mut self: Box<Self>) -> Result<Stored> {
        // Dedup, re-checked under the caller's blob lock: these exact bytes are
        // already at this exact address, so keep what is there (identical by
        // construction) and drop the temp. Checking this WITHOUT the lock is
        // what used to lose data — a delete could unlink between the check and
        // the INSERT, leaving a row addressing nothing.
        if tokio::fs::try_exists(&self.path).await.unwrap_or(false) {
            if let Some(tmp) = self.tmp.take() {
                remove_in_background(tmp);
            }
            return Ok(Stored {
                sha256: std::mem::take(&mut self.sha256),
                bytes: self.bytes,
                deduped: true,
            });
        }

        let dir = self.path.parent().expect("object path has a fan-out dir");
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating artifact dir {}", dir.display()))?;
        let tmp = self.tmp.clone().expect("land consumes self exactly once");
        tokio::fs::rename(&tmp, &self.path)
            .await
            .with_context(|| format!("landing artifact at {}", self.path.display()))?;
        // Only now is the temp no longer ours: every fallible step above leaves
        // it owned, so `Drop` cleans up on any of them.
        self.tmp = None;
        Ok(Stored {
            sha256: std::mem::take(&mut self.sha256),
            bytes: self.bytes,
            deduped: false,
        })
    }
}

impl Drop for StagedLocal {
    fn drop(&mut self) {
        if let Some(tmp) = self.tmp.take() {
            remove_in_background(tmp);
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

    async fn read_all(r: Option<ByteReader>) -> Vec<u8> {
        let mut r = r.expect("object present");
        let mut out = Vec::new();
        r.read_to_end(&mut out).await.unwrap();
        out
    }

    /// Seal and publish in one step — what a caller that holds no lock would
    /// do, and what every test here except the interleaving ones needs.
    async fn store_bytes(s: &LocalArtifactStorage, org: Uuid, chunks: &[&[u8]]) -> Result<Stored> {
        let mut w = s.begin(org).await?;
        for c in chunks {
            w.write(c).await?;
        }
        w.finish().await?.land().await
    }

    #[tokio::test]
    async fn round_trip_dedup_and_ranges() {
        let root = tmp_root();
        let s = LocalArtifactStorage::new(root.join("artifacts"));
        let org = Uuid::new_v4();

        let first = store_bytes(&s, org, &[b"hello ", b"world"]).await.unwrap();
        assert_eq!(first.bytes, 11);
        assert!(!first.deduped);
        // sha256("hello world")
        assert_eq!(
            first.sha256,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
        assert_eq!(s.size(org, &first.sha256).await.unwrap(), Some(11));

        // Same bytes again: one stored file, reported as a dedup hit.
        let second = store_bytes(&s, org, &[b"hello world"]).await.unwrap();
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

        // Trash moves the bytes aside; purge finishes the job.
        let trashed = s.trash(org, &first.sha256).await.unwrap().expect("trashed");
        assert_eq!(s.size(org, &first.sha256).await.unwrap(), None);
        s.purge(org, &trashed).await.unwrap();
        // Both halves are idempotent.
        assert!(s.trash(org, &first.sha256).await.unwrap().is_none());
        s.purge(org, &trashed).await.unwrap();

        // A read of bytes that are gone is absence, not an error — the caller
        // needs to answer 404 rather than 500.
        assert!(s
            .read_range(org, &first.sha256, 0, 11)
            .await
            .unwrap()
            .is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn trashed_bytes_go_back_where_they_came_from() {
        let root = tmp_root();
        let s = LocalArtifactStorage::new(root.join("artifacts"));
        let org = Uuid::new_v4();

        let stored = store_bytes(&s, org, &[b"put me back"]).await.unwrap();
        let trashed = s
            .trash(org, &stored.sha256)
            .await
            .unwrap()
            .expect("trashed");
        assert_eq!(s.size(org, &stored.sha256).await.unwrap(), None);

        s.restore(org, &trashed).await.unwrap();
        assert_eq!(s.size(org, &stored.sha256).await.unwrap(), Some(11));
        let back = read_all(s.read_range(org, &stored.sha256, 0, 11).await.unwrap()).await;
        assert_eq!(back, b"put me back");

        // Restoring twice is fine, and restoring onto bytes that are already
        // back drops the redundant copy instead of overwriting a live file.
        s.restore(org, &trashed).await.unwrap();
        assert_eq!(s.size(org, &stored.sha256).await.unwrap(), Some(11));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn a_stream_is_cut_off_at_the_ceiling_not_after_it() {
        let root = tmp_root();
        let s = LocalArtifactStorage::new(root.join("artifacts")).with_max_bytes(1024);
        let org = Uuid::new_v4();

        let mut w = s.begin(org).await.unwrap();
        let chunk = vec![7u8; 256];
        // Four chunks fit exactly.
        for _ in 0..4 {
            w.write(&chunk).await.unwrap();
        }
        // The fifth is refused, and refused BEFORE it reaches the disk.
        let e = w.write(&chunk).await.expect_err("past the ceiling");
        assert_eq!(
            e.downcast_ref::<TooLarge>().copied(),
            Some(TooLarge { limit: 1024 })
        );

        // The refused chunk never reached the file: sealing what was accepted
        // yields exactly the ceiling, not the ceiling plus one chunk.
        let staged = w.finish().await.unwrap();
        assert_eq!(
            staged.bytes(),
            1024,
            "nothing past the ceiling was ever written"
        );
        assert_eq!(
            staged.sha256(),
            &hex::encode(Sha256::digest(vec![7u8; 1024])),
            "and what was written is exactly the accepted prefix"
        );
        drop(staged);

        // Even one byte over is over.
        let mut w = s.begin(org).await.unwrap();
        w.write(&vec![1u8; 1024]).await.unwrap();
        assert!(w.write(b"x").await.is_err());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Every failure path used to leak the `.part` file, because `commit` took
    /// the file handle before doing anything fallible and `Drop` read that
    /// handle as its was-abandoned signal.
    #[tokio::test]
    async fn nothing_leaks_a_part_file() {
        let root = tmp_root();
        let s = LocalArtifactStorage::new(root.join("artifacts")).with_max_bytes(64);
        let org = Uuid::new_v4();
        let tmp_dir = root
            .join("artifacts")
            .join(org.simple().to_string())
            .join(TMP_DIR);

        let leftovers = || {
            std::fs::read_dir(&tmp_dir)
                .map(|d| d.filter_map(|e| e.ok()).count())
                .unwrap_or(0)
        };

        // Abandoned mid-stream.
        let mut w = s.begin(org).await.unwrap();
        w.write(b"never finished").await.unwrap();
        drop(w);
        wait_until_empty(&tmp_dir).await;
        assert_eq!(leftovers(), 0, "an abandoned write must not leave a temp");

        // Refused at the ceiling.
        let mut w = s.begin(org).await.unwrap();
        assert!(w.write(&[0u8; 65]).await.is_err());
        drop(w);
        wait_until_empty(&tmp_dir).await;
        assert_eq!(leftovers(), 0, "a rejected upload must not leave a temp");

        // Sealed but never published.
        let mut w = s.begin(org).await.unwrap();
        w.write(b"sealed only").await.unwrap();
        let staged = w.finish().await.unwrap();
        assert_eq!(staged.bytes(), 11);
        drop(staged);
        wait_until_empty(&tmp_dir).await;
        assert_eq!(
            leftovers(),
            0,
            "an unpublished staging must not leave a temp"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `Drop` hands the unlink to `spawn_blocking` rather than blocking the
    /// executor, so the file goes away a beat later.
    async fn wait_until_empty(dir: &Path) {
        for _ in 0..200 {
            let n = std::fs::read_dir(dir)
                .map(|d| d.filter_map(|e| e.ok()).count())
                .unwrap_or(0);
            if n == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn the_sweeper_is_told_about_temps_trash_and_objects() {
        let root = tmp_root();
        let s = LocalArtifactStorage::new(root.join("artifacts"));
        let org = Uuid::new_v4();

        let kept = store_bytes(&s, org, &[b"kept"]).await.unwrap();
        let moved = store_bytes(&s, org, &[b"moved aside"]).await.unwrap();
        let trashed = s.trash(org, &moved.sha256).await.unwrap().expect("trashed");
        // A temp nobody will ever finish: written straight in, so no Drop
        // removes it out from under the assertion.
        let tmp_dir = root
            .join("artifacts")
            .join(org.simple().to_string())
            .join(TMP_DIR);
        std::fs::create_dir_all(&tmp_dir).unwrap();
        let orphan_part = format!("{}{PART_SUFFIX}", Uuid::new_v4().simple());
        std::fs::write(tmp_dir.join(&orphan_part), b"half an upload").unwrap();

        let found = s.sweepable(org).await.unwrap();
        assert!(
            found
                .iter()
                .any(|x| matches!(x, Sweepable::Temp { key, .. } if key == &orphan_part)),
            "{found:?}"
        );
        assert!(
            found.contains(&Sweepable::Trashed(trashed)),
            "the trash entry is reported with the address it belongs at: {found:?}"
        );
        assert!(
            found
                .iter()
                .any(|x| matches!(x, Sweepable::Object { sha256, .. } if sha256 == &kept.sha256)),
            "{found:?}"
        );
        assert!(
            !found
                .iter()
                .any(|x| matches!(x, Sweepable::Object { sha256, .. } if sha256 == &moved.sha256)),
            "trashed bytes are not at their address any more: {found:?}"
        );

        // And the org itself is discoverable without asking the database.
        assert!(s.orgs().await.unwrap().contains(&org));

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

        // Trash and temp keys are validated the same way, since the sweeper
        // feeds names it read off the filesystem straight back in.
        assert!(s.trash_path(org, "../../etc/passwd").is_err());
        assert!(s
            .trash_path(org, &format!("{}.nope", "a".repeat(64)))
            .is_err());
        assert!(s
            .trash_path(org, &format!("{}.{}", "a".repeat(64), "b".repeat(32)))
            .is_ok());
        assert!(s.temp_path(org, "../../etc/passwd").is_err());
        assert!(s.temp_path(org, "short.part").is_err());
        assert!(s
            .temp_path(org, &format!("{}.part", "c".repeat(32)))
            .is_ok());
    }

    /// The fan-out dir and the filename are both derived from one normalized
    /// string, so an uppercase address can never address a second file.
    #[test]
    fn a_content_address_is_normalized_end_to_end() {
        let s = LocalArtifactStorage::new("root");
        let org = Uuid::new_v4();
        let upper = "AB".to_string() + &"C".repeat(62);
        let lower = upper.to_ascii_lowercase();
        assert_eq!(
            s.object_path(org, &upper).unwrap(),
            s.object_path(org, &lower).unwrap()
        );
        assert!(s
            .object_path(org, &upper)
            .unwrap()
            .to_string_lossy()
            .contains("ab"));
    }
}
