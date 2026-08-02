// The blind vault (PLAN-v2.1 PR 4.6, D29/D33): one domain's ciphertext, held
// by a node that cannot read a byte of it.
//
// On disk, `tenants/<tenant>/domains/<domain>/` is a device data dir's shape
// exactly — `log/<device>/<start_seq:016x>.seg` and `blocks/<hh>/<id>` — plus
// `node-meta.db`. That is not a coincidence or a convenience: restore is
// "copy the files back and open a Store over them" (PR 4.8), so any divergence
// in layout would be a divergence in the restore path, and D36's cheap
// multi-node story ("rsync the vault directory") depends on the tree being
// ordinary files with no hidden index.
//
// It is NOT a Store. `Store::new` resolves a master and heals at open by
// unwrapping segment headers; a blind node has no master, so it holds the
// files and folds nothing.
//
// The files half is hive-sync's `DirVault` verbatim, so a node lands bytes
// through the same code the loopback tests exercise. What this type adds is
// the part a server owes its operator:
//
//   * WRITE-ONCE, enforced against the bytes on disk. Every byte a sender
//     re-offers over a range this node already holds must match, byte for
//     byte. A differing re-upload — of a sealed segment or of the tail — is
//     an integrity ALARM and a refusal, never an overwrite. Equivocation, two
//     different histories under one (device, start_seq), is the attack this
//     catches, and a blind node catches it with no key at all because it is
//     comparing ciphertext to ciphertext.
//   * EXTENSION ONLY AT THE END, PREFIX INTACT. Bytes may follow what is
//     held; they may never replace it, and a write past the end (a hole) is
//     refused outright. Note that a SEALED segment may still be extended:
//     sealing means the file is final at its SOURCE, not that this node holds
//     all of it — see `check_write`, where refusing that would break the
//     protocol's own resume path.
//   * BOOKKEEPING. `node-meta.db` remembers lengths, hashes, sizes, pins,
//     epochs, the forget queue, and every alarm ever raised (meta.rs).
//
// Deliberate non-goals here: quotas (enforced at ingest, PR 4.9), enrollment
// (PR 4.7), and any notion of what a record MEANS. A refusal in this file is
// always about bytes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use hive_core::oplog::{self, HeadsSnapshot};
use hive_sync::{DirVault, LandedSegment, SyncSink};

use crate::meta::{AlarmKind, NodeMeta};

/// The tenants tree under the node root.
pub const TENANTS_DIR: &str = "tenants";

/// A tenant's domains, under `tenants/<tenant>/`.
pub const DOMAINS_DIR: &str = "domains";

/// Longest tenant/domain directory name. Same ceiling the frozen device-id
/// allowlist uses — these are path components built from operator config and
/// (at PR 4.7) from a session's declared domain, so they are checked where
/// they are made, not where they are used.
pub const MAX_NAME_LEN: usize = 64;

/// Whether `name` may become a tenant or domain directory. Deliberately
/// narrower than a filesystem allows: letters, digits, dot, dash, underscore,
/// no leading dot, and never `.` or `..`. A domain name like
/// `example.com` passes; anything that could climb out of the root does
/// not.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// One domain's vault: verbatim files plus the node's memory of them.
#[derive(Clone)]
pub struct SegmentVault {
    inner: Arc<VaultInner>,
}

struct VaultInner {
    tenant: String,
    domain: String,
    dir: PathBuf,
    files: DirVault,
    meta: NodeMeta,
}

impl std::fmt::Debug for SegmentVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentVault")
            .field("tenant", &self.inner.tenant)
            .field("domain", &self.inner.domain)
            .field("dir", &self.inner.dir)
            .finish_non_exhaustive()
    }
}

/// What the write-once check decided about one offered write.
#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    /// Bytes this node already holds, byte-identical. A re-push after a lost
    /// ack is normal and must not be an error — but it must not be a write
    /// either.
    Duplicate { len: u64 },
    /// Append `bytes[from..]` at `at`. `at` is always the current length.
    Append { at: u64, from: usize },
}

impl SegmentVault {
    /// Open (creating if absent) `<root>/tenants/<tenant>/domains/<domain>/`.
    pub fn open(root: &Path, tenant: &str, domain: &str) -> Result<SegmentVault> {
        if !name_ok(tenant) {
            bail!("refusing tenant name {tenant:?} (outside the allowlist)");
        }
        if !name_ok(domain) {
            bail!("refusing domain name {domain:?} (outside the allowlist)");
        }
        let dir = domain_dir(root, tenant, domain);
        let files = DirVault::open(&dir)
            .with_context(|| format!("opening vault files under {}", dir.display()))?;
        let meta = NodeMeta::open(&dir)
            .with_context(|| format!("opening node meta under {}", dir.display()))?;
        let vault = SegmentVault {
            inner: Arc::new(VaultInner {
                tenant: tenant.to_string(),
                domain: domain.to_string(),
                dir,
                files,
                meta,
            }),
        };
        vault.reconcile()?;
        Ok(vault)
    }

    /// The tenant this vault belongs to. Never crosses into the data plane —
    /// it is a directory name and a log field, nothing more (D33).
    pub fn tenant(&self) -> &str {
        &self.inner.tenant
    }

    /// The domain this vault holds.
    pub fn domain(&self) -> &str {
        &self.inner.domain
    }

    /// The directory: store shape plus `node-meta.db`.
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// The node's memory of this domain (lengths, hashes, pins, epochs, the
    /// forget queue, the alarm log).
    pub fn meta(&self) -> &NodeMeta {
        &self.inner.meta
    }

    /// Everything held, in the same snapshot shape a device reports — this is
    /// what makes a vault comparable to the store it backs up, and what a
    /// restore serves.
    pub fn heads(&self) -> Result<HeadsSnapshot> {
        self.inner.files.heads()
    }

    /// Bring `node-meta.db` in line with the files. Runs at open because the
    /// tree is ordinary files: an operator may have restored it from a
    /// snapshot, rsynced it from another node (D36), or lost the database
    /// while keeping the segments. The files are the truth; the database is
    /// the memory of them.
    pub fn reconcile(&self) -> Result<usize> {
        let heads = self.heads()?;
        for seg in &heads.segments {
            self.inner.meta.record_segment(
                &seg.device,
                seg.start_seq,
                seg.len,
                &seg.file_hash,
                seg.sealed,
            )?;
        }
        Ok(heads.segments.len())
    }
}

impl VaultInner {
    fn segment_path(&self, device: &str, start_seq: u64) -> Result<PathBuf> {
        if !oplog::device_id_ok(device) {
            bail!("refusing vault access for device id {device:?} (outside the allowlist)");
        }
        Ok(oplog::segment_path(&self.dir, device, start_seq))
    }

    fn raw_len(path: &Path) -> Result<u64> {
        match std::fs::metadata(path) {
            Ok(m) => Ok(m.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => Err(e).with_context(|| format!("sizing {}", path.display())),
        }
    }

    /// True when a HIGHER-numbered segment exists for this device, on disk or
    /// in the node's memory. That is the whole definition of sealed: the
    /// frozen writer rotates by adding a higher start_seq and never returns
    /// (oplog/writer.rs), so a successor is proof this file is final.
    ///
    /// Filenames only — no file is read. The frozen naming is zero-padded
    /// lowercase hex precisely so lexicographic order is numeric order, and
    /// the name to compare against comes from `oplog::segment_path` rather
    /// than from a format string of our own.
    fn is_sealed(&self, device: &str, start_seq: u64) -> Result<bool> {
        if let Some(row) = self.meta.segment(device, start_seq)? {
            if row.sealed {
                return Ok(true);
            }
        }
        let path = self.segment_path(device, start_seq)?;
        let mine = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("segment path {} has no file name", path.display()))?
            .to_string();
        let dir = path.parent().expect("segment path has a parent");
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e).with_context(|| format!("reading log dir {}", dir.display())),
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.ends_with(".seg") && *name > *mine {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// The write-once decision, taken against the bytes on disk (never
    /// against a memory of the session — a node that trusted its own
    /// bookkeeping here would be trusting the thing an attacker is trying to
    /// desynchronize).
    fn check_write(&self, device: &str, start_seq: u64, at: u64, bytes: &[u8]) -> Result<Verdict> {
        let path = self.segment_path(device, start_seq)?;
        let have = Self::raw_len(&path)?;
        let sealed = self.is_sealed(device, start_seq)?;

        if at > have {
            // A gap. Not an alarm: a confused or resumed sender, and the
            // frozen format has no way to fill a hole later.
            bail!(
                "refusing to write segment {device}/{start_seq} at offset {at}: \
                 the file is {have} bytes, and a log has no holes"
            );
        }

        // The overlap: every byte re-offered over a range already held must
        // match. This is the equivocation check, and it runs before anything
        // is written.
        let mut from = 0usize;
        if at < have {
            let overlap = std::cmp::min(bytes.len() as u64, have - at) as usize;
            let held = read_range(&path, at, overlap)?;
            if held != bytes[..overlap] {
                let offset = at + first_difference(&held, &bytes[..overlap]) as u64;
                self.meta.raise_alarm(
                    AlarmKind::SegmentDivergence,
                    Some(device),
                    Some(start_seq),
                    &format!(
                        "a re-upload of {}/{}/{start_seq} differs from the bytes already held, \
                         first at byte {offset}",
                        self.tenant, self.domain
                    ),
                )?;
                bail!(
                    "refusing a differing re-upload of segment {device}/{start_seq}: \
                     byte {offset} does not match what this vault already holds \
                     (integrity alarm raised)"
                );
            }
            from = overlap;
        }

        if from == bytes.len() {
            return Ok(Verdict::Duplicate { len: have });
        }

        // Past here the write GROWS the segment, with its prefix verified
        // intact above.
        //
        // Growth is allowed even on a SEALED segment, and that is a decision
        // worth stating: sealing says the file is final at its SOURCE, not
        // that this node holds all of it. A session killed mid-segment leaves
        // a prefix here; if the device rotates before the next session, the
        // resume that completes it is a write to a segment that is by then
        // sealed. Refusing it would break the protocol's own resume path and
        // strand those bytes forever on a machine whose job is to hold them.
        //
        // What actually protects a sealed segment is therefore the prefix
        // check above (plus the recorded-hash check in `after_write`), not a
        // ban on appending. The stronger form — "this node holds the WHOLE
        // segment, so nothing may follow" — needs the source's announced
        // length, which the sink trait does not carry today; it belongs with
        // the session that knows it (PR 4.7+).
        if sealed {
            tracing::debug!(
                device,
                start_seq,
                have,
                "completing a sealed segment this node holds only a prefix of"
            );
        }
        Ok(Verdict::Append { at: have, from })
    }

    /// After the bytes land: check them against what this node last attested,
    /// then refresh its memory and seal whatever the write sealed.
    ///
    /// The recorded-prefix check is the belt to `check_write`'s braces. That
    /// one compares re-offered bytes against the file; this one compares the
    /// file against the hash in `node-meta.db`, which is the only thing that
    /// still catches a segment whose bytes VANISHED from disk (a lost file, a
    /// silently truncated one) and came back different. It can never misfire
    /// on honest growth: appending leaves the first `row.len` bytes exactly as
    /// they were, so their hash is unchanged by construction.
    ///
    /// The whole-file hash is recomputed per landed chunk. That is a
    /// deliberate cost: the meta row is the evidence an alarm cites and the
    /// thing a later scan compares against, so it must describe the bytes
    /// that are there now. Segments rotate at 8 MiB, which bounds it.
    fn after_write(&self, device: &str, start_seq: u64) -> Result<u64> {
        let path = self.segment_path(device, start_seq)?;
        let bytes =
            std::fs::read(&path).with_context(|| format!("reading back {}", path.display()))?;
        let len = bytes.len() as u64;
        // What this node ATTESTS is the whole-frame prefix, never the raw file.
        // A chunk boundary lands inside a frame all the time, and those
        // trailing bytes are not yet anything the sender can be held to — the
        // sender describes the same prefix (`oplog::list_segments`) and
        // `DirVault::landed` cuts to the same place. One meaning for the
        // attestation across all three is what lets `landed` below treat ANY
        // shortfall as divergence instead of having to guess whether a drop
        // was an honest trim.
        let attested = match hive_core::oplog::walk_segment(&bytes) {
            Ok(walk) => &bytes[..walk.whole_end as usize],
            // An unparseable header is corruption, not a torn tail: describe
            // what is there and let the comparison below judge it.
            Err(_) => &bytes[..],
        };
        let attested_len = attested.len() as u64;
        let hash = *blake3::hash(attested).as_bytes();

        if let Some(row) = self.meta.segment(device, start_seq)? {
            // Two ways the file can fail to reproduce what we attested, and
            // BOTH are divergence:
            //
            //   * it still covers `row.len` bytes but they hash differently —
            //     the bytes were substituted;
            //   * it no longer reaches `row.len` at all — the attested bytes
            //     are simply gone.
            //
            // The short file is the case the length guard used to skip, which
            // meant a truncated segment fell through to `record_segment` and
            // was silently re-attested at its new, shorter content: the node
            // would forget it had ever vouched for the missing bytes. A guard
            // that only keeps the slice below in bounds must not double as the
            // decision about whether to check at all.
            let diverged = if attested_len < row.len {
                true
            } else {
                *blake3::hash(&attested[..row.len as usize]).as_bytes() != row.file_hash
            };
            if diverged {
                self.meta.raise_alarm(
                    AlarmKind::SegmentDivergence,
                    Some(device),
                    Some(start_seq),
                    &format!(
                        "{}/{}/{start_seq} no longer reproduces the {} bytes this node attested \
                         (now {len} bytes)",
                        self.tenant, self.domain, row.len
                    ),
                )?;
                // What is on disk is not what we vouched for: keep none of it.
                // Truncating to zero leaves the segment in the one state that
                // is honestly "absent", so the next session re-sends it whole.
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .and_then(|f| f.set_len(0))
                    .with_context(|| format!("discarding {}", path.display()))?;
                bail!(
                    "refusing to keep segment {device}/{start_seq}: its first {} bytes no longer \
                     match the hash this node recorded (integrity alarm raised)",
                    row.len
                );
            }
        }

        let sealed = self.is_sealed(device, start_seq)?;
        self.meta
            .record_segment(device, start_seq, attested_len, &hash, sealed)?;
        // A new tail seals everything below it — the only event that ever
        // seals anything.
        self.meta.seal_below(device, start_seq)?;
        // The RAW length is what the write path resumes from; only the
        // attestation is trimmed to whole frames.
        Ok(len)
    }
}

#[async_trait]
impl SyncSink for SegmentVault {
    async fn landed(&self, device: &str, start_seq: u64) -> Result<Option<LandedSegment>> {
        let landed = self.inner.files.landed(device, start_seq).await?;
        if let Some(landed) = &landed {
            let inner = self.inner.clone();
            let device_owned = device.to_string();
            let (len, hash) = (landed.len, landed.file_hash);
            let (tenant, domain) = (self.tenant().to_string(), self.domain().to_string());
            tokio::task::spawn_blocking(move || -> Result<()> {
                let device = device_owned;
                // This runs BEFORE the transfer, on whatever is on disk right
                // now. It must therefore never overwrite the attestation it is
                // about to be checked against: doing so let a tampered file
                // re-describe itself, and `after_write` then compared the new
                // bytes against a hash derived from those same new bytes and
                // matched trivially. The divergence alarm could not fire.
                //
                // Both sides now describe the whole-frame prefix, so a row that
                // exists is directly comparable:
                match inner.meta.segment(&device, start_seq)? {
                    // Nothing vouched for yet — this is the first sighting.
                    None => {
                        let sealed = inner.is_sealed(&device, start_seq)?;
                        inner
                            .meta
                            .record_segment(&device, start_seq, len, &hash, sealed)?;
                    }
                    // Exactly what we vouched for. Nothing to write.
                    Some(row) if row.len == len && row.file_hash == hash => {}
                    // The attested bytes are gone or were replaced in place.
                    // Neither is something an honest peer can do to a file this
                    // node already holds.
                    Some(row) if len <= row.len => {
                        inner.meta.raise_alarm(
                            AlarmKind::SegmentDivergence,
                            Some(&device),
                            Some(start_seq),
                            &format!(
                                "{tenant}/{domain}/{start_seq} was attested at {} bytes but the \
                                 file on disk now offers {len} — the bytes this node vouched for \
                                 are not there",
                                row.len
                            ),
                        )?;
                        bail!(
                            "refusing to resume segment {device}/{start_seq}: it no longer \
                             reproduces the {} bytes this node attested (integrity alarm raised)",
                            row.len
                        );
                    }
                    // Longer than we attested. That is what honest growth looks
                    // like, but the prefix cannot be checked without the bytes,
                    // and re-reading them here would double the cost of every
                    // session. So leave the attestation ALONE: `after_write`
                    // holds the bytes and does the verified update on the write
                    // path, which is the only path that should move it.
                    Some(_) => {}
                }
                Ok(())
            })
            .await
            .context("vault landed-bookkeeping task")??;
        }
        Ok(landed)
    }

    async fn extend_segment(
        &self,
        device: &str,
        start_seq: u64,
        at_offset: u64,
        bytes: Vec<u8>,
    ) -> Result<u64> {
        // Check, then write. The pair is not atomic, and deliberately does
        // not need to be: two sessions racing the same segment can only make
        // the loser's offset stale, and a stale offset is refused by the
        // files half (`at_offset` must equal the length) rather than applied.
        // The failure mode is a refused chunk and a resumed transfer, never a
        // byte written past a check that did not see it.
        //
        // Arc rather than a clone: the chunk is up to a megabyte and the
        // check reads it on a blocking thread before the writer half does.
        let bytes = Arc::new(bytes);
        let verdict = {
            let inner = self.inner.clone();
            let device = device.to_string();
            let bytes = bytes.clone();
            tokio::task::spawn_blocking(move || {
                inner.check_write(&device, start_seq, at_offset, &bytes)
            })
            .await
            .context("vault write-once check task")??
        };

        match verdict {
            Verdict::Duplicate { len } => {
                tracing::debug!(
                    device,
                    start_seq,
                    at_offset,
                    len,
                    "re-offered segment bytes already held, byte for byte"
                );
                return Ok(len);
            }
            Verdict::Append { at, from } => {
                // The files half does the writing, header check and all: one
                // implementation of "how bytes land", shared with the
                // loopback tests.
                self.inner
                    .files
                    .extend_segment(device, start_seq, at, bytes[from..].to_vec())
                    .await?;
            }
        }

        let inner = self.inner.clone();
        let device = device.to_string();
        tokio::task::spawn_blocking(move || inner.after_write(&device, start_seq))
            .await
            .context("vault segment-bookkeeping task")?
    }

    async fn has_block(&self, id: [u8; 32]) -> Result<bool> {
        self.inner.files.has_block(id).await
    }

    async fn put_block(&self, id: [u8; 32], bytes: Vec<u8>) -> Result<()> {
        // Blocks are content-addressed and `DirVault::put_block` re-hashes
        // before it writes, so a second put of the same id is the same bytes
        // by construction — there is no write-once question to ask here, and
        // deliberately no alarm to raise.
        let size = bytes.len() as u64;
        self.inner.files.put_block(id, bytes).await?;
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.meta.record_block(&id, size))
            .await
            .context("vault block-bookkeeping task")?
    }
}

/// `<root>/tenants/<tenant>/domains/<domain>`.
pub fn domain_dir(root: &Path, tenant: &str, domain: &str) -> PathBuf {
    tenant_dir(root, tenant).join(DOMAINS_DIR).join(domain)
}

/// `<root>/tenants/<tenant>`.
pub fn tenant_dir(root: &Path, tenant: &str) -> PathBuf {
    root.join(TENANTS_DIR).join(tenant)
}

/// Read exactly `len` bytes at `at` — the range a re-upload claims to be
/// re-sending.
fn read_range(path: &Path, at: u64, len: usize) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    file.seek(SeekFrom::Start(at))
        .with_context(|| format!("seeking {} to {at}", path.display()))?;
    let mut buf = vec![0u8; len];
    file.read_exact(&mut buf)
        .with_context(|| format!("reading {len} bytes at {at} from {}", path.display()))?;
    Ok(buf)
}

/// Index of the first differing byte. Only called when the slices differ.
fn first_difference(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_components_are_checked_where_they_are_made() {
        assert!(name_ok("household"));
        assert!(name_ok("example.com"));
        assert!(name_ok("nate_1-2"));
        assert!(!name_ok(""));
        assert!(!name_ok("."));
        assert!(!name_ok(".."));
        assert!(!name_ok(".hidden"));
        assert!(!name_ok("a/b"));
        assert!(!name_ok("../etc"));
        assert!(!name_ok("a b"));
        assert!(!name_ok(&"x".repeat(MAX_NAME_LEN + 1)));
    }

    #[test]
    fn a_traversing_name_never_becomes_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let err = SegmentVault::open(root.path(), "../..", "d").unwrap_err();
        assert!(format!("{err:#}").contains("tenant name"), "{err:#}");
        let err = SegmentVault::open(root.path(), "t", "../../etc").unwrap_err();
        assert!(format!("{err:#}").contains("domain name"), "{err:#}");
    }

    #[test]
    fn the_layout_is_the_one_restore_expects() {
        let root = tempfile::tempdir().unwrap();
        let vault = SegmentVault::open(root.path(), "household", "example.com").unwrap();
        assert_eq!(
            vault.dir(),
            root.path()
                .join("tenants")
                .join("household")
                .join("domains")
                .join("example.com")
        );
        assert!(vault.dir().join("blocks").is_dir(), "store shape");
        assert!(vault.dir().join("node-meta.db").is_file());
        assert_eq!(vault.tenant(), "household");
        assert_eq!(vault.domain(), "example.com");
    }

    #[test]
    fn first_difference_finds_the_byte_an_alarm_cites() {
        assert_eq!(first_difference(b"abcd", b"abXd"), 2);
        assert_eq!(first_difference(b"abcd", b"Xbcd"), 0);
    }
}
