// `node-meta.db` (PLAN-v2.1 PR 4.6): the bookkeeping a server owes its
// operator, beside a vault that is otherwise nothing but verbatim files.
//
// PLAIN SQLite, deliberately. rusqlite here is the same SQLCipher build the
// index uses, but this connection is never keyed — an unkeyed SQLCipher
// connection writes an ordinary database — because encrypting it would imply
// the node holds a key, and the whole claim of the blind tier is that it does
// not (D29). Nothing in this file is domain plaintext: lengths, hashes,
// sizes, public keys, epochs, and queues. If any of it ever needed
// encrypting, that would mean the schema had grown something it must not have.
//
// What is here, and who fills it:
//
//   segments      lengths + whole-file hashes + sealed  — the write-once
//                 evidence (PR 4.6: a differing re-upload is checked
//                 against these rows, not against a memory of the session)
//   blocks        ciphertext block sizes                — quota accounting (4.9)
//   device_pins   enrolled devices' pinned public keys  — enrollment (4.7)
//   control       the monotonic per-domain control epoch — enrollment (4.7)
//   enroll_codes  one-time enrollment codes, HASHED     — enrollment (4.7)
//   auth_audit    every enrollment and every auth failure — enrollment (4.7)
//   forget_queue  block ids the keyed pusher asked to shred, until acked (4.9)
//   alarms        integrity ALARMS, permanently retained — PR 4.6
//
// The later columns land ahead of their consumers because the vault's file
// shape and this schema are what an operator backs up: growing it under a
// running deployment is a migration, and there is nothing here worth migrating
// for.
//
// It is NOT rebuildable-by-replay the way the fold's index is. The segment
// and block rows are (rescan the tree), but pins, epochs, codes, the audit
// trail, the forget queue, and the alarm history are the node's own memory and
// exist nowhere else — which is why a version mismatch here REFUSES rather
// than dropping.
//
// Two things are deliberately NOT in the clear here, on a database that is
// otherwise entirely metadata: an enrollment code (only its blake3 hash, D30's
// "hashed at rest" — a stolen node-meta.db must not yield a live credential)
// and, forever, anything a domain wrote.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// The database file inside a domain's vault directory.
pub const META_DB_FILE: &str = "node-meta.db";

/// Seconds since the epoch, in the form every column here stores.
///
/// ONE converter, beside the columns it feeds, because there were three and
/// they disagreed at exactly the edge that matters. The saturating version
/// answered 0 for a clock at or before 1970, and `live_codes(0)` matches
/// `expires_at > 0` — every code ever minted — so a node whose RTC had not
/// been set yet reported an OPEN ENROLLMENT WINDOW indefinitely and its
/// listener admitted any unpinned key. The comment on that version claimed
/// precisely the opposite ("should refuse every code, and 0 does that").
///
/// So this REFUSES rather than clamps. A node that cannot say what time it is
/// cannot decide whether a ten-minute credential has expired, and the safe
/// answer to a question you cannot evaluate is to decline it.
pub fn unix_seconds(at: std::time::SystemTime) -> Result<i64> {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .context("a clock before 1970 cannot express an enrollment TTL")?
        .as_secs();
    i64::try_from(secs).context("a clock past year 292277026596")
}

/// Auth-audit rows kept per domain. Big enough that a real incident is still
/// legible days later, small enough that an unauthenticated stranger holding a
/// socket open cannot grow the file without bound (the alarms table has no
/// such cap because only a real integrity failure can write to it).
pub const AUTH_AUDIT_KEEP: i64 = 10_000;

/// Schema version, stamped in `PRAGMA user_version`. Bumping it is a real
/// migration (see the module header): this database holds state that exists
/// nowhere else.
///
/// 2 (PLAN-v2.1 PR 4.7) added `enroll_codes` and `auth_audit`. Purely
/// additive — `CREATE TABLE IF NOT EXISTS` carries a v1 file forward with
/// every row intact — but the stamp still moves, because a v1 build opening a
/// v2 file would silently ignore pins it cannot see the provenance of.
pub const META_VERSION: u32 = 2;

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS segments (
    device      TEXT    NOT NULL,
    start_seq   INTEGER NOT NULL,
    len         INTEGER NOT NULL,
    file_hash   BLOB    NOT NULL,
    sealed      INTEGER NOT NULL DEFAULT 0,
    first_seen  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    updated_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (device, start_seq)
);

CREATE TABLE IF NOT EXISTS blocks (
    id       BLOB    PRIMARY KEY,
    size     INTEGER NOT NULL,
    landed_at TEXT   NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE TABLE IF NOT EXISTS device_pins (
    device     TEXT PRIMARY KEY,
    ed25519_pk BLOB NOT NULL,
    x25519_pk  BLOB NOT NULL,
    pinned_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    revoked_at TEXT
);

CREATE TABLE IF NOT EXISTS control (
    id    INTEGER PRIMARY KEY CHECK (id = 1),
    epoch INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS enroll_codes (
    code_hash  BLOB    PRIMARY KEY,
    minted_at  TEXT    NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    expires_at INTEGER NOT NULL,
    redeemed_at TEXT,
    redeemed_by TEXT
);

CREATE TABLE IF NOT EXISTS auth_audit (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    kind   TEXT NOT NULL,
    peer   TEXT,
    pin    TEXT,
    detail TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS forget_queue (
    block_id  BLOB PRIMARY KEY,
    queued_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    attempts  INTEGER NOT NULL DEFAULT 0,
    acked_at  TEXT
);

CREATE TABLE IF NOT EXISTS alarms (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    raised_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    kind      TEXT NOT NULL,
    device    TEXT,
    start_seq INTEGER,
    detail    TEXT NOT NULL
);
"#;

/// One segment as the node remembers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentRow {
    pub device: String,
    pub start_seq: u64,
    pub len: u64,
    pub file_hash: [u8; 32],
    /// True once a higher-numbered segment for the same device exists: from
    /// then on these bytes are final forever (the frozen rotation rule —
    /// writer.rs only ever adds a higher start_seq).
    pub sealed: bool,
}

/// One enrolled device's pinned transport keys (PR 4.7 writes these; the
/// listener reads them). A revoked device keeps its row: revocations are
/// permanently retained tombstones, never deletions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePin {
    pub device: String,
    pub ed25519_pk: [u8; 32],
    pub x25519_pk: [u8; 32],
    pub revoked: bool,
}

/// What happened when a code was redeemed (PR 4.7). The three refusals are
/// deliberately distinguished HERE and collapsed into one wire error: the
/// operator's audit trail is where "wrong" and "expired" differ, and a peer
/// that could tell them apart could probe the code space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeVerdict {
    /// Live, unredeemed, and now marked used — atomically, so two racing
    /// redemptions cannot both win.
    Redeemed,
    /// No such code. (Also what a code someone made up looks like.)
    Unknown,
    /// Past its TTL. Still on file: an expired code that shows up is worth
    /// seeing in the audit trail.
    Expired,
    /// Already used, once. Single-use is what makes a code a credential for
    /// one device rather than for whoever else saw the operator's terminal.
    AlreadyRedeemed,
}

/// A security-relevant event on this domain, for the operator's trail
/// (PR 4.7's auth-failure audit logging). Closed set, like [`AlarmKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthEventKind {
    /// An operator minted a one-time enrollment code.
    CodeMinted,
    /// A device redeemed one and is now pinned.
    Enrolled,
    /// A redemption was refused — the interesting half of the trail.
    EnrollRefused,
    /// A session opened from a pinned, unrevoked device.
    SessionOpened,
    /// A connection was refused after the handshake: unpinned, revoked,
    /// wrong domain, or a frame that has no business opening a connection.
    SessionRefused,
    /// A device was revoked. Paired with a permanent tombstone in
    /// `device_pins` and a bump of the control epoch.
    DeviceRevoked,
}

impl AuthEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuthEventKind::CodeMinted => "code_minted",
            AuthEventKind::Enrolled => "enrolled",
            AuthEventKind::EnrollRefused => "enroll_refused",
            AuthEventKind::SessionOpened => "session_opened",
            AuthEventKind::SessionRefused => "session_refused",
            AuthEventKind::DeviceRevoked => "device_revoked",
        }
    }

    /// Whether this event is a refusal. Refusals log at WARN; the rest at
    /// INFO — an operator watching the journal should see failures without
    /// having to filter successes out.
    fn is_refusal(self) -> bool {
        matches!(
            self,
            AuthEventKind::EnrollRefused | AuthEventKind::SessionRefused
        )
    }
}

/// One audited security event, as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEvent {
    pub id: i64,
    pub at: String,
    pub kind: String,
    /// The peer's own claim about who it is (a device id from a hello), or
    /// none when nothing was claimed. Never trusted — it is evidence, not
    /// identity.
    pub peer: Option<String>,
    /// The SPKI pin the connection actually presented, lowercase hex. THIS is
    /// the identity half, and it is what an operator greps.
    pub pin: Option<String>,
    pub detail: String,
}

/// What an integrity alarm is about. The set is closed: a new kind is a new
/// thing the node can catch, and it should be named here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlarmKind {
    /// A re-upload of an existing segment carried DIFFERENT bytes for a range
    /// this node already holds. The single most important thing a blind vault
    /// can notice: it means one of the two ends is lying about a log that is
    /// supposed to be write-once and single-writer — and a node with no key
    /// can still catch it, because it is comparing ciphertext to ciphertext.
    SegmentDivergence,
}

impl AlarmKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlarmKind::SegmentDivergence => "segment_divergence",
        }
    }
}

/// One raised alarm, as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alarm {
    pub id: i64,
    pub raised_at: String,
    pub kind: String,
    pub device: Option<String>,
    pub start_seq: Option<u64>,
    pub detail: String,
}

/// A domain's `node-meta.db`.
pub struct NodeMeta {
    conn: Mutex<Connection>,
    path: PathBuf,
}

impl std::fmt::Debug for NodeMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeMeta")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl NodeMeta {
    /// Open (creating if absent) the meta database for one domain vault.
    pub fn open(vault_dir: &Path) -> Result<NodeMeta> {
        std::fs::create_dir_all(vault_dir)
            .with_context(|| format!("creating vault dir {}", vault_dir.display()))?;
        let path = vault_dir.join(META_DB_FILE);
        let conn = Connection::open(&path)
            .with_context(|| format!("opening node meta db {}", path.display()))?;

        // No `PRAGMA key`: see the module header. WAL for the same reason the
        // index uses it — a crash mid-write must not cost the file.
        let _mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))
            .context("enabling WAL on node-meta.db")?;

        let on_disk: u32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("reading node-meta user_version")?;
        if on_disk > META_VERSION {
            bail!(
                "{} was written by a newer hive-node (schema {on_disk}, this build speaks \
                 {META_VERSION}) — refusing to open it rather than dropping state that exists \
                 nowhere else",
                path.display()
            );
        }
        conn.execute_batch(DDL)
            .context("applying node-meta schema")?;
        if on_disk != META_VERSION {
            conn.pragma_update(None, "user_version", META_VERSION)
                .context("stamping node-meta user_version")?;
        }

        Ok(NodeMeta {
            conn: Mutex::new(conn),
            path,
        })
    }

    /// The database file, for error messages and ops docs.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        // Poisoning means a previous caller panicked mid-statement. The
        // connection itself is still usable (SQLite is transactional), and a
        // node that refuses every write afterwards is strictly worse than one
        // that carries on, so the guard is taken either way.
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ── segments ────────────────────────────────────────────────────────────

    /// Record what this node now holds of one segment. Idempotent by
    /// (device, start_seq): the row IS the current state, not a history.
    pub fn record_segment(
        &self,
        device: &str,
        start_seq: u64,
        len: u64,
        file_hash: &[u8; 32],
        sealed: bool,
    ) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO segments (device, start_seq, len, file_hash, sealed)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(device, start_seq) DO UPDATE SET
                     len = excluded.len,
                     file_hash = excluded.file_hash,
                     sealed = excluded.sealed,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
                params![device, start_seq, len, &file_hash[..], sealed],
            )
            .with_context(|| format!("recording segment {device}/{start_seq}"))?;
        Ok(())
    }

    /// Mark every segment of `device` below `tail_start_seq` sealed. Called
    /// when a higher-numbered segment appears, which is the only event that
    /// seals anything.
    pub fn seal_below(&self, device: &str, tail_start_seq: u64) -> Result<usize> {
        let n = self
            .conn()
            .execute(
                "UPDATE segments SET sealed = 1,
                     updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE device = ?1 AND start_seq < ?2 AND sealed = 0",
                params![device, tail_start_seq],
            )
            .with_context(|| format!("sealing segments below {device}/{tail_start_seq}"))?;
        Ok(n)
    }

    pub fn segment(&self, device: &str, start_seq: u64) -> Result<Option<SegmentRow>> {
        let row = self
            .conn()
            .query_row(
                "SELECT device, start_seq, len, file_hash, sealed FROM segments
                 WHERE device = ?1 AND start_seq = ?2",
                params![device, start_seq],
                segment_row,
            )
            .optional()
            .with_context(|| format!("reading segment {device}/{start_seq}"))?;
        row.transpose()
    }

    /// Every segment, ordered by (device, start_seq) — the same order the
    /// keyless heads snapshot uses, so the two are comparable.
    pub fn segments(&self) -> Result<Vec<SegmentRow>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT device, start_seq, len, file_hash, sealed FROM segments
                 ORDER BY device, start_seq",
            )
            .context("preparing segment scan")?;
        let rows = stmt
            .query_map([], segment_row)
            .context("scanning segments")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("scanning segments")??);
        }
        Ok(out)
    }

    /// Log bytes plus block bytes — what a quota (PR 4.9) counts.
    pub fn stored_bytes(&self) -> Result<u64> {
        let conn = self.conn();
        let segments: i64 = conn
            .query_row("SELECT COALESCE(SUM(len), 0) FROM segments", [], |r| {
                r.get(0)
            })
            .context("summing segment bytes")?;
        let blocks: i64 = conn
            .query_row("SELECT COALESCE(SUM(size), 0) FROM blocks", [], |r| {
                r.get(0)
            })
            .context("summing block bytes")?;
        Ok(segments as u64 + blocks as u64)
    }

    // ── blocks ──────────────────────────────────────────────────────────────

    /// Record a landed ciphertext block's size. The bytes live in the
    /// blockstore under their bare id; this row is the accounting.
    pub fn record_block(&self, id: &[u8; 32], size: u64) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO blocks (id, size) VALUES (?1, ?2)
                 ON CONFLICT(id) DO UPDATE SET size = excluded.size",
                params![&id[..], size],
            )
            .with_context(|| format!("recording block {}", hex(id)))?;
        Ok(())
    }

    /// Forget a block's accounting row (the bytes are the vault's business).
    pub fn drop_block(&self, id: &[u8; 32]) -> Result<()> {
        self.conn()
            .execute("DELETE FROM blocks WHERE id = ?1", params![&id[..]])
            .with_context(|| format!("dropping block row {}", hex(id)))?;
        Ok(())
    }

    pub fn block_size(&self, id: &[u8; 32]) -> Result<Option<u64>> {
        let size: Option<i64> = self
            .conn()
            .query_row(
                "SELECT size FROM blocks WHERE id = ?1",
                params![&id[..]],
                |r| r.get(0),
            )
            .optional()
            .with_context(|| format!("reading block row {}", hex(id)))?;
        Ok(size.map(|s| s as u64))
    }

    // ── device pins (PR 4.7) ────────────────────────────────────────────────

    /// Pin an enrolled device's transport keys. Re-pinning the SAME keys is
    /// idempotent; the epoch, not this table, is what makes a change ordered.
    pub fn pin_device(
        &self,
        device: &str,
        ed25519_pk: &[u8; 32],
        x25519_pk: &[u8; 32],
    ) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO device_pins (device, ed25519_pk, x25519_pk) VALUES (?1, ?2, ?3)
                 ON CONFLICT(device) DO UPDATE SET
                     ed25519_pk = excluded.ed25519_pk,
                     x25519_pk = excluded.x25519_pk",
                params![device, &ed25519_pk[..], &x25519_pk[..]],
            )
            .with_context(|| format!("pinning device {device}"))?;
        Ok(())
    }

    /// Pin a device ONLY IF this domain still has no enrolled one, deciding and
    /// writing inside a single transaction.
    ///
    /// `enroll::redeem` checks the same thing beforehand, and that check is
    /// what produces a refusal an operator can read. This is the one that makes
    /// it TRUE: the pre-check is a read, the pin is a later write, and two
    /// redemptions of two DIFFERENT live codes run on their own blocking
    /// threads. Each spends its own `enroll_codes` row, so the single-use
    /// UPDATE cannot serialize them against each other — both would observe an
    /// empty table and both would pin, leaving a domain with two devices
    /// enrolled node-solo and no short-auth-string ever compared. That is the
    /// state v1 refuses precisely because PR 4.14's ceremony does not ship.
    ///
    /// IMMEDIATE so the write lock is taken at BEGIN rather than at the INSERT:
    /// a deferred transaction would let both readers in and turn the loser into
    /// SQLITE_BUSY at commit, which is a refusal for the wrong reason.
    pub fn pin_first_device(
        &self,
        device: &str,
        ed25519_pk: &[u8; 32],
        x25519_pk: &[u8; 32],
    ) -> Result<bool> {
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .context("opening the enrollment transaction")?;
        let taken: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM device_pins WHERE revoked_at IS NULL",
                [],
                |r| r.get(0),
            )
            .context("counting enrolled devices")?;
        if taken > 0 {
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO device_pins (device, ed25519_pk, x25519_pk) VALUES (?1, ?2, ?3)
             ON CONFLICT(device) DO UPDATE SET
                 ed25519_pk = excluded.ed25519_pk,
                 x25519_pk = excluded.x25519_pk",
            params![device, &ed25519_pk[..], &x25519_pk[..]],
        )
        .with_context(|| format!("pinning device {device}"))?;
        tx.commit().context("committing the enrollment")?;
        Ok(true)
    }

    /// Tombstone a device. The row stays forever — a deleted revocation is a
    /// re-admitted device.
    pub fn revoke_device(&self, device: &str) -> Result<bool> {
        let n = self
            .conn()
            .execute(
                "UPDATE device_pins
                 SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE device = ?1 AND revoked_at IS NULL",
                params![device],
            )
            .with_context(|| format!("revoking device {device}"))?;
        Ok(n > 0)
    }

    pub fn device_pin(&self, device: &str) -> Result<Option<DevicePin>> {
        let row = self
            .conn()
            .query_row(
                "SELECT device, ed25519_pk, x25519_pk, revoked_at FROM device_pins
                 WHERE device = ?1",
                params![device],
                device_pin_row,
            )
            .optional()
            .with_context(|| format!("reading device pin {device}"))?;
        row.transpose()
    }

    /// Every pin this domain has ever issued, revoked ones included, ordered
    /// by device. The listener snapshots this per connection (PR 4.7) — a
    /// household's worth of rows, re-read every time so a `device revoke`
    /// takes effect on the NEXT handshake with no restart.
    pub fn device_pins(&self) -> Result<Vec<DevicePin>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT device, ed25519_pk, x25519_pk, revoked_at FROM device_pins
                 ORDER BY device",
            )
            .context("preparing device-pin scan")?;
        let rows = stmt
            .query_map([], device_pin_row)
            .context("scanning device pins")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("scanning device pins")??);
        }
        Ok(out)
    }

    // ── enrollment codes (PR 4.7) ───────────────────────────────────────────

    /// Record a minted code by its HASH and its expiry (unix seconds). The
    /// code itself is never written anywhere: the operator's terminal is the
    /// only place it exists, which is what "hashed at rest" buys.
    pub fn mint_code(&self, code_hash: &[u8; 32], expires_at: i64) -> Result<()> {
        self.conn()
            .execute(
                "INSERT INTO enroll_codes (code_hash, expires_at) VALUES (?1, ?2)
                 ON CONFLICT(code_hash) DO NOTHING",
                params![&code_hash[..], expires_at],
            )
            .context("recording a minted enrollment code")?;
        Ok(())
    }

    /// Spend a code, in ONE statement.
    ///
    /// The update is conditional on the row still being unredeemed and unexpired
    /// and reports whether it changed a row, so "check then mark" cannot be
    /// raced by two redemptions of the same code — the loser sees
    /// [`CodeVerdict::AlreadyRedeemed`], which is exactly right.
    ///
    /// `now` is passed in rather than read from the clock so the TTL is
    /// testable without waiting ten minutes.
    pub fn redeem_code(&self, code_hash: &[u8; 32], now: i64, device: &str) -> Result<CodeVerdict> {
        let conn = self.conn();
        let spent = conn
            .execute(
                "UPDATE enroll_codes
                 SET redeemed_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'), redeemed_by = ?3
                 WHERE code_hash = ?1 AND redeemed_at IS NULL AND expires_at > ?2",
                params![&code_hash[..], now, device],
            )
            .context("redeeming an enrollment code")?;
        if spent > 0 {
            return Ok(CodeVerdict::Redeemed);
        }
        // It did not apply. Say why, for the audit trail only.
        let row: Option<(i64, Option<String>)> = conn
            .query_row(
                "SELECT expires_at, redeemed_at FROM enroll_codes WHERE code_hash = ?1",
                params![&code_hash[..]],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .context("reading an enrollment code")?;
        Ok(match row {
            None => CodeVerdict::Unknown,
            Some((_, Some(_))) => CodeVerdict::AlreadyRedeemed,
            Some(_) => CodeVerdict::Expired,
        })
    }

    /// How many codes are live at `now` — unredeemed and unexpired.
    ///
    /// The listener asks this per connection, and the answer is a door: while
    /// it is above zero, an UNPINNED key may complete a handshake (there is no
    /// other way to enroll a key nobody has seen). At zero, the pin set is the
    /// whole guest list and an unpinned peer is refused by rustls itself.
    pub fn live_codes(&self, now: i64) -> Result<u64> {
        let n: i64 = self
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM enroll_codes
                 WHERE redeemed_at IS NULL AND expires_at > ?1",
                params![now],
                |r| r.get(0),
            )
            .context("counting live enrollment codes")?;
        Ok(n as u64)
    }

    /// Whether this domain ever minted the code behind `code_hash`.
    ///
    /// ROUTING only, and deliberately blind to expiry and to redemption: a
    /// spent or stale code must still find its way to the domain it was minted
    /// for, so that its refusal lands in THAT domain's audit trail instead of
    /// nowhere. Whether it is still spendable is [`NodeMeta::redeem_code`]'s
    /// atomic business.
    pub fn knows_code(&self, code_hash: &[u8; 32]) -> Result<bool> {
        let n: i64 = self
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM enroll_codes WHERE code_hash = ?1",
                params![&code_hash[..]],
                |r| r.get(0),
            )
            .context("looking up an enrollment code")?;
        Ok(n > 0)
    }

    // ── auth audit (PR 4.7) ─────────────────────────────────────────────────

    /// Record a security event, and log it — a refusal at WARN, everything
    /// else at INFO. Both surfaces matter: the journal is what an operator
    /// watches live, the table is what they read afterwards.
    ///
    /// Bounded, unlike `alarms`. An alarm needs a deliberate integrity failure
    /// to raise; an auth failure needs only a stranger with a socket, so the
    /// trail keeps the most recent [`AUTH_AUDIT_KEEP`] events and lets the
    /// rest go rather than handing anyone a disk-fill.
    pub fn record_auth_event(
        &self,
        kind: AuthEventKind,
        peer: Option<&str>,
        pin: Option<&str>,
        detail: &str,
    ) -> Result<i64> {
        if kind.is_refusal() {
            tracing::warn!(
                event = kind.as_str(),
                peer = peer.unwrap_or("-"),
                pin = pin.unwrap_or("-"),
                "{detail}"
            );
        } else {
            tracing::info!(
                event = kind.as_str(),
                peer = peer.unwrap_or("-"),
                pin = pin.unwrap_or("-"),
                "{detail}"
            );
        }
        let id = {
            let conn = self.conn();
            conn.execute(
                "INSERT INTO auth_audit (kind, peer, pin, detail) VALUES (?1, ?2, ?3, ?4)",
                params![kind.as_str(), peer, pin, detail],
            )
            .context("recording an auth event")?;
            conn.last_insert_rowid()
        };
        self.trim_auth_audit(AUTH_AUDIT_KEEP)?;
        Ok(id)
    }

    /// Drop all but the newest `keep` audit rows, returning how many went.
    ///
    /// The cutoff is the id of the `keep`-th newest row rather than
    /// `last_id - keep`, so it stays right when the ids are not dense — which
    /// they are in production and are not the moment anything else has touched
    /// the table. `keep` is a parameter rather than the constant so the bound
    /// is testable without ten thousand inserts (the same reason the
    /// enrollment TTL takes its clock as an argument).
    pub fn trim_auth_audit(&self, keep: i64) -> Result<usize> {
        let n = self
            .conn()
            .execute(
                "DELETE FROM auth_audit WHERE id <= (
                     SELECT id FROM auth_audit ORDER BY id DESC LIMIT 1 OFFSET ?1
                 )",
                params![keep],
            )
            .context("trimming the auth audit")?;
        Ok(n)
    }

    /// The retained audit trail, oldest first.
    pub fn auth_events(&self) -> Result<Vec<AuthEvent>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare("SELECT id, at, kind, peer, pin, detail FROM auth_audit ORDER BY id")
            .context("preparing auth-audit scan")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AuthEvent {
                    id: r.get(0)?,
                    at: r.get(1)?,
                    kind: r.get(2)?,
                    peer: r.get(3)?,
                    pin: r.get(4)?,
                    detail: r.get(5)?,
                })
            })
            .context("scanning the auth audit")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("scanning the auth audit")?);
        }
        Ok(out)
    }

    // ── control epoch (PR 4.7) ──────────────────────────────────────────────

    /// The domain's control epoch. 0 before anything has been enrolled.
    pub fn control_epoch(&self) -> Result<u64> {
        let epoch: Option<i64> = self
            .conn()
            .query_row("SELECT epoch FROM control WHERE id = 1", [], |r| r.get(0))
            .optional()
            .context("reading control epoch")?;
        Ok(epoch.unwrap_or(0) as u64)
    }

    /// Raise the epoch to `epoch`, refusing any regression. Monotonic is the
    /// whole point: a replayed control statement carrying an older epoch must
    /// not be able to un-revoke a device (PR 4.7's stale-epoch refusal).
    pub fn set_control_epoch(&self, epoch: u64) -> Result<u64> {
        let current = self.control_epoch()?;
        if epoch < current {
            bail!("refusing to regress the control epoch from {current} to {epoch}");
        }
        self.conn()
            .execute(
                "INSERT INTO control (id, epoch) VALUES (1, ?1)
                 ON CONFLICT(id) DO UPDATE SET epoch = excluded.epoch",
                params![epoch],
            )
            .context("writing control epoch")?;
        Ok(epoch)
    }

    // ── forget queue (PR 4.9) ───────────────────────────────────────────────

    /// Queue block ids the keyed pusher asked this node to shred. Persistent
    /// because the point is to survive a crash mid-drain and be re-sent.
    pub fn queue_forget(&self, ids: &[[u8; 32]]) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction().context("opening forget-queue tx")?;
        let mut queued = 0;
        for id in ids {
            queued += tx
                .execute(
                    "INSERT INTO forget_queue (block_id) VALUES (?1)
                     ON CONFLICT(block_id) DO NOTHING",
                    params![&id[..]],
                )
                .with_context(|| format!("queueing forget for {}", hex(id)))?;
        }
        tx.commit().context("committing forget-queue tx")?;
        Ok(queued)
    }

    /// Block ids still waiting to be shredded, oldest first.
    pub fn pending_forget(&self) -> Result<Vec<[u8; 32]>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT block_id FROM forget_queue WHERE acked_at IS NULL
                 ORDER BY queued_at, block_id",
            )
            .context("preparing forget-queue scan")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, Vec<u8>>(0))
            .context("scanning forget queue")?;
        let mut out = Vec::new();
        for row in rows {
            let raw = row.context("scanning forget queue")?;
            let id: [u8; 32] = raw
                .as_slice()
                .try_into()
                .map_err(|_| anyhow::anyhow!("forget-queue row holds a {}-byte id", raw.len()))?;
            out.push(id);
        }
        Ok(out)
    }

    /// Mark one queued forget done. Ack-deduped by construction: acking twice
    /// is a no-op, which is what makes "re-sent until acked" safe.
    pub fn ack_forget(&self, id: &[u8; 32]) -> Result<bool> {
        let n = self
            .conn()
            .execute(
                "UPDATE forget_queue
                 SET acked_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
                 WHERE block_id = ?1 AND acked_at IS NULL",
                params![&id[..]],
            )
            .with_context(|| format!("acking forget for {}", hex(id)))?;
        Ok(n > 0)
    }

    // ── alarms ──────────────────────────────────────────────────────────────

    /// Record an integrity alarm. Also logs at ERROR: an alarm nobody sees is
    /// not an alarm, and the operator's journal is the surface they watch.
    pub fn raise_alarm(
        &self,
        kind: AlarmKind,
        device: Option<&str>,
        start_seq: Option<u64>,
        detail: &str,
    ) -> Result<i64> {
        tracing::error!(
            alarm = kind.as_str(),
            device = device.unwrap_or("-"),
            start_seq,
            "integrity alarm: {detail}"
        );
        let conn = self.conn();
        conn.execute(
            "INSERT INTO alarms (kind, device, start_seq, detail) VALUES (?1, ?2, ?3, ?4)",
            params![kind.as_str(), device, start_seq, detail],
        )
        .context("recording integrity alarm")?;
        Ok(conn.last_insert_rowid())
    }

    /// Every alarm ever raised here, oldest first. Retained permanently: an
    /// operator's evidence, and the reason the vault refuses rather than
    /// silently repairing.
    pub fn alarms(&self) -> Result<Vec<Alarm>> {
        let conn = self.conn();
        let mut stmt = conn
            .prepare(
                "SELECT id, raised_at, kind, device, start_seq, detail FROM alarms ORDER BY id",
            )
            .context("preparing alarm scan")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(Alarm {
                    id: r.get(0)?,
                    raised_at: r.get(1)?,
                    kind: r.get(2)?,
                    device: r.get(3)?,
                    start_seq: r.get::<_, Option<i64>>(4)?.map(|s| s as u64),
                    detail: r.get(5)?,
                })
            })
            .context("scanning alarms")?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.context("scanning alarms")?);
        }
        Ok(out)
    }
}

fn segment_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<SegmentRow>> {
    let hash: Vec<u8> = r.get(3)?;
    let device: String = r.get(0)?;
    let start_seq: i64 = r.get(1)?;
    let len: i64 = r.get(2)?;
    let sealed: bool = r.get(4)?;
    Ok(match <[u8; 32]>::try_from(hash.as_slice()) {
        Ok(file_hash) => Ok(SegmentRow {
            device,
            start_seq: start_seq as u64,
            len: len as u64,
            file_hash,
            sealed,
        }),
        Err(_) => Err(anyhow::anyhow!(
            "segment row {device}/{start_seq} holds a {}-byte hash",
            hash.len()
        )),
    })
}

fn device_pin_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<DevicePin>> {
    let device: String = r.get(0)?;
    let ed: Vec<u8> = r.get(1)?;
    let x: Vec<u8> = r.get(2)?;
    let revoked_at: Option<String> = r.get(3)?;
    Ok(
        match (
            <[u8; 32]>::try_from(ed.as_slice()),
            <[u8; 32]>::try_from(x.as_slice()),
        ) {
            (Ok(ed25519_pk), Ok(x25519_pk)) => Ok(DevicePin {
                device,
                ed25519_pk,
                x25519_pk,
                revoked: revoked_at.is_some(),
            }),
            _ => Err(anyhow::anyhow!(
                "device pin {device} holds {}/{}-byte keys",
                ed.len(),
                x.len()
            )),
        },
    )
}

fn hex(id: &[u8; 32]) -> String {
    data_encoding::HEXLOWER.encode(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> (tempfile::TempDir, NodeMeta) {
        let dir = tempfile::tempdir().unwrap();
        let meta = NodeMeta::open(dir.path()).unwrap();
        (dir, meta)
    }

    #[test]
    fn segments_are_current_state_not_history() {
        let (_dir, meta) = meta();
        meta.record_segment("alpha", 1, 100, &[1u8; 32], false)
            .unwrap();
        meta.record_segment("alpha", 1, 200, &[2u8; 32], false)
            .unwrap();
        let row = meta.segment("alpha", 1).unwrap().unwrap();
        assert_eq!(row.len, 200);
        assert_eq!(row.file_hash, [2u8; 32]);
        assert_eq!(meta.segments().unwrap().len(), 1);
    }

    #[test]
    fn a_higher_segment_seals_everything_below_it() {
        let (_dir, meta) = meta();
        meta.record_segment("alpha", 1, 10, &[1u8; 32], false)
            .unwrap();
        meta.record_segment("alpha", 9, 10, &[2u8; 32], false)
            .unwrap();
        assert_eq!(meta.seal_below("alpha", 9).unwrap(), 1);
        assert!(meta.segment("alpha", 1).unwrap().unwrap().sealed);
        assert!(!meta.segment("alpha", 9).unwrap().unwrap().sealed);
        assert_eq!(meta.seal_below("alpha", 9).unwrap(), 0, "idempotent");
    }

    #[test]
    fn stored_bytes_counts_log_and_blocks() {
        let (_dir, meta) = meta();
        meta.record_segment("alpha", 1, 500, &[1u8; 32], true)
            .unwrap();
        meta.record_block(&[7u8; 32], 25).unwrap();
        meta.record_block(&[8u8; 32], 25).unwrap();
        assert_eq!(meta.stored_bytes().unwrap(), 550);
        meta.drop_block(&[8u8; 32]).unwrap();
        assert_eq!(meta.stored_bytes().unwrap(), 525);
        assert_eq!(meta.block_size(&[7u8; 32]).unwrap(), Some(25));
        assert_eq!(meta.block_size(&[8u8; 32]).unwrap(), None);
    }

    #[test]
    fn a_revoked_device_keeps_its_row() {
        let (_dir, meta) = meta();
        meta.pin_device("laptop", &[3u8; 32], &[4u8; 32]).unwrap();
        assert!(!meta.device_pin("laptop").unwrap().unwrap().revoked);
        assert!(meta.revoke_device("laptop").unwrap());
        let pin = meta.device_pin("laptop").unwrap().unwrap();
        assert!(pin.revoked, "a tombstone, not a deletion");
        assert_eq!(pin.ed25519_pk, [3u8; 32]);
        assert!(!meta.revoke_device("laptop").unwrap(), "already revoked");
    }

    /// The listener snapshots every pin per connection, tombstones included —
    /// it is the revoked ones that have to stay visible.
    #[test]
    fn every_pin_is_listed_including_the_tombstones() {
        let (_dir, meta) = meta();
        assert!(meta.device_pins().unwrap().is_empty());
        meta.pin_device("laptop", &[3u8; 32], &[4u8; 32]).unwrap();
        meta.pin_device("desktop", &[5u8; 32], &[6u8; 32]).unwrap();
        meta.revoke_device("laptop").unwrap();

        let pins = meta.device_pins().unwrap();
        assert_eq!(pins.len(), 2);
        assert_eq!(pins[0].device, "desktop", "ordered by device");
        assert!(!pins[0].revoked);
        assert_eq!(pins[1].device, "laptop");
        assert!(pins[1].revoked, "a revoked device is still listed");
    }

    /// Single-use, TTL-bounded, and never stored in the clear — the three
    /// properties D30 asks of an enrollment code, in one test.
    #[test]
    fn an_enrollment_code_is_hashed_single_use_and_short_lived() {
        let (dir, meta) = meta();
        let hash = blake3::derive_key("hive-node-enroll-code-v1", b"K4RT9X2M");
        meta.mint_code(&hash, 1_000).unwrap();
        assert_eq!(meta.live_codes(999).unwrap(), 1);

        // Expired: the TTL is a comparison against the caller's clock, so the
        // test does not have to wait ten minutes for it.
        assert_eq!(meta.live_codes(1_000).unwrap(), 0, "expiry is exclusive");
        assert_eq!(
            meta.redeem_code(&hash, 1_001, "laptop").unwrap(),
            CodeVerdict::Expired
        );
        // Still live a second earlier — and spendable exactly once.
        assert_eq!(
            meta.redeem_code(&hash, 999, "laptop").unwrap(),
            CodeVerdict::Redeemed
        );
        assert_eq!(
            meta.redeem_code(&hash, 999, "laptop").unwrap(),
            CodeVerdict::AlreadyRedeemed
        );
        assert_eq!(meta.live_codes(999).unwrap(), 0, "spent, not merely used");
        assert_eq!(
            meta.redeem_code(&[0xff; 32], 999, "laptop").unwrap(),
            CodeVerdict::Unknown
        );

        // Hashed at rest: the plaintext code appears nowhere in the file.
        let raw = std::fs::read(dir.path().join(META_DB_FILE)).unwrap();
        assert!(
            !raw.windows(8).any(|w| w == b"K4RT9X2M"),
            "a stolen node-meta.db must not yield a live credential"
        );
    }

    #[test]
    fn the_audit_trail_records_who_and_is_bounded() {
        let (_dir, meta) = meta();
        meta.record_auth_event(
            AuthEventKind::SessionRefused,
            Some("laptop"),
            Some("ab00"),
            "unpinned device key",
        )
        .unwrap();
        let events = meta.auth_events().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "session_refused");
        assert_eq!(events[0].peer.as_deref(), Some("laptop"));
        assert_eq!(events[0].pin.as_deref(), Some("ab00"));
        assert!(events[0].detail.contains("unpinned"));
        assert_eq!(events[0].at.len(), 24, "the store's ISO shape");

        // Bounded: an unauthenticated stranger with a socket must not be able
        // to grow this file without limit. The trim runs on every insert
        // against [`AUTH_AUDIT_KEEP`]; the bound itself is exercised at a size
        // a test can hold, which is why `keep` is a parameter.
        for i in 0..3 {
            meta.record_auth_event(AuthEventKind::SessionRefused, None, None, &format!("n{i}"))
                .unwrap();
        }
        assert_eq!(meta.auth_events().unwrap().len(), 4);
        assert_eq!(
            meta.trim_auth_audit(2).unwrap(),
            2,
            "all but the newest two"
        );
        let kept: Vec<String> = meta
            .auth_events()
            .unwrap()
            .into_iter()
            .map(|e| e.detail)
            .collect();
        assert_eq!(kept, vec!["n1", "n2"], "the NEWEST survive");
        assert_eq!(
            meta.trim_auth_audit(2).unwrap(),
            0,
            "idempotent under the cap"
        );
        assert_eq!(
            meta.trim_auth_audit(AUTH_AUDIT_KEEP).unwrap(),
            0,
            "a trail under the real cap loses nothing"
        );
    }

    #[test]
    fn the_control_epoch_never_regresses() {
        let (_dir, meta) = meta();
        assert_eq!(meta.control_epoch().unwrap(), 0);
        meta.set_control_epoch(4).unwrap();
        assert_eq!(meta.control_epoch().unwrap(), 4);
        let err = meta.set_control_epoch(3).unwrap_err();
        assert!(format!("{err:#}").contains("regress"), "{err:#}");
        assert_eq!(meta.control_epoch().unwrap(), 4);
        meta.set_control_epoch(4).unwrap();
    }

    #[test]
    fn the_forget_queue_is_persistent_and_ack_deduped() {
        let dir = tempfile::tempdir().unwrap();
        {
            let meta = NodeMeta::open(dir.path()).unwrap();
            assert_eq!(meta.queue_forget(&[[1u8; 32], [2u8; 32]]).unwrap(), 2);
            assert_eq!(meta.queue_forget(&[[1u8; 32]]).unwrap(), 0, "deduped");
        }
        // The crash: a fresh process, the same file.
        let meta = NodeMeta::open(dir.path()).unwrap();
        assert_eq!(meta.pending_forget().unwrap().len(), 2);
        assert!(meta.ack_forget(&[1u8; 32]).unwrap());
        assert!(!meta.ack_forget(&[1u8; 32]).unwrap(), "acked exactly once");
        assert_eq!(meta.pending_forget().unwrap(), vec![[2u8; 32]]);
    }

    #[test]
    fn alarms_are_retained_with_their_evidence() {
        let (_dir, meta) = meta();
        meta.raise_alarm(
            AlarmKind::SegmentDivergence,
            Some("alpha"),
            Some(1),
            "byte 40 differs",
        )
        .unwrap();
        let alarms = meta.alarms().unwrap();
        assert_eq!(alarms.len(), 1);
        assert_eq!(alarms[0].kind, "segment_divergence");
        assert_eq!(alarms[0].device.as_deref(), Some("alpha"));
        assert_eq!(alarms[0].start_seq, Some(1));
        assert!(alarms[0].detail.contains("byte 40"));
        assert_eq!(alarms[0].raised_at.len(), 24, "the store's ISO shape");
    }

    /// State that exists nowhere else must not be dropped by a version
    /// mismatch — the opposite of the fold index's drop-and-replay.
    #[test]
    fn a_newer_schema_is_refused_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        {
            let meta = NodeMeta::open(dir.path()).unwrap();
            meta.pin_device("laptop", &[3u8; 32], &[4u8; 32]).unwrap();
        }
        let conn = Connection::open(dir.path().join(META_DB_FILE)).unwrap();
        conn.pragma_update(None, "user_version", META_VERSION + 1)
            .unwrap();
        drop(conn);

        let err = NodeMeta::open(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("newer hive-node"), "{err:#}");
    }

    #[test]
    fn reopening_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        {
            let meta = NodeMeta::open(dir.path()).unwrap();
            meta.record_segment("alpha", 1, 10, &[1u8; 32], true)
                .unwrap();
            meta.set_control_epoch(2).unwrap();
        }
        let meta = NodeMeta::open(dir.path()).unwrap();
        assert_eq!(meta.segments().unwrap().len(), 1);
        assert_eq!(meta.control_epoch().unwrap(), 2);
    }
}
