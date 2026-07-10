// The writer core (PR 1.6, D18): ONE dedicated thread owns the rusqlite
// connection (via SqliteIndex), the op-log LogWriter, and the BlockStore.
// Store methods ship closures here over an mpsc channel and await a oneshot
// reply — the async surface of every store module is preserved while every
// read AND write serializes through this thread (single-writer simplicity;
// at personal scale the queue never matters, and "reads see every committed
// write" comes for free because nothing interleaves).
//
// Write discipline (each logical write = one public store fn):
//   1. the command layer (the store module, running inside its closure) mints
//      ids (new_id) and timestamps (now_iso), runs emergence, and pre-computes
//      EVERYTHING (inbox rows, emerged entity payloads) into Draft records per
//      the fold contract (core/src/fold);
//   2. `Core::commit` assigns seq/lc, appends the batch to the op log
//      (LogWriter::append_batch — durable at fsync), then applies each record
//      through fold::apply in ONE SQLite transaction (SqliteIndex::fold).
//
// A crash between (1) and (2) is healed at the next open: `heal` scans every
// device log from the fold watermark forward and folds the tail (the fold
// skips already-applied seqs, so replay is idempotent).
//
// Lamport clock: single-device in Phase 1, so `lc` = `seq` (monotonic and
// gapless). Multi-device lc maintenance arrives with sync (Phase 4).
//
// Device id: minted once per data dir into the `device` file (command layer,
// never the fold) — a nanoid under the oplog's [A-Za-z0-9._-] allowlist.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value as Json;

use crate::blockstore::BlockStore;
use crate::index::SqliteIndex;
use crate::keys::KeySource;
use crate::oplog::{LogReader, LogWriter, Record};

/// File inside the data dir holding this installation's device id.
pub const DEVICE_FILE: &str = "device";

/// Crash-heal replay batch size (bounds memory on a long unfolded tail).
const HEAL_BATCH: usize = 512;

/// Everything the writer thread owns.
pub(crate) struct Core {
    pub index: SqliteIndex,
    pub log: LogWriter,
    /// Payload bytes (today: mail attachments via store/mail.rs).
    pub blocks: BlockStore,
    pub keys: Arc<dyn KeySource + Send + Sync>,
    pub device: String,
}

/// One record-to-be: the command layer's output. `Core::commit` turns drafts
/// into sealed records (device/seq/lc assigned) in order.
pub(crate) struct Draft {
    pub kind: &'static str,
    pub actor: String,
    pub ts: String,
    pub payload: Json,
}

impl Draft {
    pub fn new(kind: &'static str, actor: &str, ts: &str, payload: Json) -> Draft {
        Draft {
            kind,
            actor: actor.to_string(),
            ts: ts.to_string(),
            payload,
        }
    }
}

impl Core {
    /// The one write path: append the batch to the op log (fsync), then fold
    /// it into the derived index in one transaction. A batch is a logical
    /// write — all of it becomes durable and visible together.
    pub fn commit(&mut self, drafts: Vec<Draft>) -> Result<()> {
        if drafts.is_empty() {
            return Ok(());
        }
        let base = self.log.last_seq();
        let records: Vec<Record> = drafts
            .iter()
            .enumerate()
            .map(|(i, d)| {
                let seq = base + 1 + i as u64;
                Record::new(
                    &self.device,
                    seq,
                    seq, // lc = seq while single-device (see header)
                    &d.ts,
                    &d.actor,
                    d.kind,
                    json_to_cbor(&d.payload),
                )
            })
            .collect();
        self.log
            .append_batch(&records)
            .context("appending record batch to the op log")?;
        self.index
            .fold(&records)
            .context("folding committed records into the index")?;
        Ok(())
    }

    /// Read access to the derived index's connection.
    pub fn conn(&self) -> &rusqlite::Connection {
        self.index.conn()
    }
}

/// Open (or create) everything under `data_dir` and heal any unfolded log
/// tail. Called once by `Store::new`, on the caller's thread; the result
/// moves onto the writer thread.
pub(crate) fn open_core(data_dir: &Path, keys: Arc<dyn KeySource + Send + Sync>) -> Result<Core> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating data dir {}", data_dir.display()))?;
    let device = read_or_mint_device(data_dir)?;
    let mut index = SqliteIndex::open(data_dir, keys.as_ref())?;
    // LogWriter::open runs the torn-tail repair for OUR device before the
    // heal scan reads it back.
    let log = LogWriter::open(data_dir, &device, keys.as_ref())?;
    heal(&mut index, data_dir, keys.as_ref())?;
    let blocks = BlockStore::open(data_dir)?;
    Ok(Core {
        index,
        log,
        blocks,
        keys,
        device,
    })
}

/// Replay every device log's unfolded tail into the index. Scans from the
/// start of each log (LogReader has no seek; decrypt-and-skip is cheap at
/// personal scale) and folds only records past the device's watermark.
fn heal(index: &mut SqliteIndex, data_dir: &Path, keys: &dyn KeySource) -> Result<()> {
    let log_root = data_dir.join("log");
    if !log_root.is_dir() {
        return Ok(());
    }
    let mut devices: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&log_root).context("listing device logs")? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            devices.push(entry.file_name().to_string_lossy().to_string());
        }
    }
    devices.sort();
    for device in devices {
        let watermark = index.applied_seq(&device)?.unwrap_or(0);
        let mut batch: Vec<Record> = Vec::with_capacity(HEAL_BATCH);
        for item in LogReader::scan(data_dir, &device, keys)? {
            let (rec, _hash) =
                item.with_context(|| format!("scanning device {device:?} log for crash heal"))?;
            if rec.seq <= watermark {
                continue; // already folded
            }
            batch.push(rec);
            if batch.len() >= HEAL_BATCH {
                index.fold(&batch).context("folding healed tail batch")?;
                batch.clear();
            }
        }
        if !batch.is_empty() {
            index.fold(&batch).context("folding healed tail batch")?;
        }
    }
    Ok(())
}

/// The per-data-dir device id: read `device`, or mint + persist one.
fn read_or_mint_device(data_dir: &Path) -> Result<String> {
    let path = data_dir.join(DEVICE_FILE);
    if path.exists() {
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("reading device file {}", path.display()))?;
        let device = raw.trim().to_string();
        if !crate::oplog::device_id_ok(&device) {
            anyhow::bail!(
                "device file {} holds an invalid id {device:?}",
                path.display()
            );
        }
        return Ok(device);
    }
    let device = format!("dev-{}", nanoid::nanoid!(12));
    debug_assert!(crate::oplog::device_id_ok(&device));
    std::fs::write(&path, format!("{device}\n"))
        .with_context(|| format!("writing device file {}", path.display()))?;
    Ok(device)
}

/// serde_json → ciborium, structurally. Payloads are JSON in the command
/// layer (ergonomics) and CBOR on the wire (the frozen envelope); the fold
/// reads them back through serde_json::to_value, so this round-trips.
pub(crate) fn json_to_cbor(v: &Json) -> ciborium::Value {
    use ciborium::Value as Cb;
    match v {
        Json::Null => Cb::Null,
        Json::Bool(b) => Cb::Bool(*b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Cb::Integer(i.into())
            } else if let Some(u) = n.as_u64() {
                Cb::Integer(u.into())
            } else {
                Cb::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Json::String(s) => Cb::Text(s.clone()),
        Json::Array(items) => Cb::Array(items.iter().map(json_to_cbor).collect()),
        Json::Object(map) => Cb::Map(
            map.iter()
                .map(|(k, val)| (Cb::Text(k.clone()), json_to_cbor(val)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_to_cbor_roundtrips_through_the_fold_view() {
        let json = serde_json::json!({
            "id": "x", "n": 7, "f": 1.5, "b": true, "nil": null,
            "arr": ["a", 2], "map": {"k": "v"}
        });
        let cbor = json_to_cbor(&json);
        let back: Json = serde_json::to_value(&cbor).unwrap();
        assert_eq!(json, back);
    }
}
