// LogWriter — the single writer for one device's op log. Owns the tail
// segment, the frame chain state, and rotation; `open` doubles as the
// torn-tail repair path (see mod.rs "Recovery semantics").
//
// Concurrency model: exactly one LogWriter per device log, by construction
// (from PR 1.6 it lives on the store's single writer thread). Nothing here
// locks; two writers on one directory would be a caller bug.

use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::segment::{
    derive_segment_key, device_dir, encode_header, list_segments, read_frame, read_header,
    seal_frame, segment_path, sync_dir, FrameRead, Header, HeaderRead,
};
use super::{device_id_ok, kind, ts_shape_ok, Record, ENVELOPE_VERSION, SEGMENT_ROTATE_BYTES};
use crate::keys::KeySource;

struct Tail {
    file: File,
    segment_key: [u8; 32],
    /// Current on-disk length of the tail segment.
    bytes: u64,
}

/// Result of walking the tail segment at open: the byte offset where valid
/// data ends, and the last valid record's (seq, frame_hash) when at least
/// one frame survived.
struct TailWalk {
    valid_end: u64,
    last: Option<(u64, [u8; 32])>,
}

pub struct LogWriter {
    data_dir: PathBuf,
    device: String,
    /// Held for the writer's lifetime: rotation wraps a fresh segment key on
    /// every new segment. Single-process desktop app; acceptable residency.
    master: [u8; 32],
    rotate_at: u64,
    tail: Option<Tail>,
    last_seq: u64,
    last_hash: [u8; 32],
    /// Set when an append failed midway: in-memory chain state may be ahead
    /// of disk, so further appends are refused. Reopen to repair.
    poisoned: bool,
}

impl LogWriter {
    /// Open (creating directories as needed) the writer for `device` under
    /// `<data_dir>/log/<device>/`, repairing a torn tail if the last run
    /// crashed mid-append. Uses the production rotation threshold.
    pub fn open(data_dir: &Path, device: &str, keys: &dyn KeySource) -> Result<LogWriter> {
        Self::open_with_segment_limit(data_dir, device, keys, SEGMENT_ROTATE_BYTES)
    }

    /// `open` with an explicit rotation threshold. Exists for tests (small
    /// segments make rotation cheap to exercise); production code uses
    /// `open`. The threshold is a writer policy, not a format property —
    /// readers never care where a segment ends.
    pub fn open_with_segment_limit(
        data_dir: &Path,
        device: &str,
        keys: &dyn KeySource,
        rotate_at: u64,
    ) -> Result<LogWriter> {
        if !device_id_ok(device) {
            bail!("invalid device id {device:?}");
        }
        let master = keys.master_key()?;
        std::fs::create_dir_all(device_dir(data_dir, device))
            .with_context(|| format!("creating log dir for {device}"))?;
        let mut w = LogWriter {
            data_dir: data_dir.to_path_buf(),
            device: device.to_string(),
            master,
            rotate_at,
            tail: None,
            last_seq: 0,
            last_hash: [0u8; 32],
            poisoned: false,
        };
        w.recover()?;
        Ok(w)
    }

    /// Seq of the last durable record (0 when the log is empty). Callers mint
    /// the next record as `last_seq() + 1`.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Frame hash of the last durable record ([0;32] when the log is empty).
    /// The next appended record's `prev` will be exactly this.
    pub fn last_frame_hash(&self) -> [u8; 32] {
        self.last_hash
    }

    /// Append records atomically-enough: frames are written in order and the
    /// tail file is fsynced once before returning — a record is durable iff
    /// this call returned Ok. On any mid-batch failure the writer poisons
    /// itself (reopen to repair); unfsynced partial frames are exactly what
    /// `open`'s torn-tail truncation removes.
    ///
    /// The chain is the writer's: each input record's `prev` is IGNORED and
    /// overwritten with the running frame chain (callers cannot know frame
    /// hashes before sealing). The returned vector holds the frame hash of
    /// each appended record, in order — `records[i]`'s sealed `prev` is the
    /// hash returned for `records[i-1]` (or `last_frame_hash()` before the
    /// call, for i = 0).
    ///
    /// Validation (before anything is written): envelope version, device
    /// match, `ts` shape, kind in the closed set, and gapless seq continuing
    /// from `last_seq()`.
    pub fn append_batch(&mut self, records: &[Record]) -> Result<Vec<[u8; 32]>> {
        if self.poisoned {
            bail!("writer is poisoned by an earlier failed append; reopen the log");
        }
        if records.is_empty() {
            return Ok(Vec::new());
        }
        let mut expect = self.last_seq + 1;
        for r in records {
            if r.v != ENVELOPE_VERSION {
                bail!(
                    "record seq {} has envelope version {}, expected {ENVELOPE_VERSION}",
                    r.seq,
                    r.v
                );
            }
            if r.device != self.device {
                bail!(
                    "record seq {} names device {:?}, writer is {:?}",
                    r.seq,
                    r.device,
                    self.device
                );
            }
            if !ts_shape_ok(&r.ts) {
                bail!(
                    "record seq {} ts {:?} is not the frozen 24-char ISO shape",
                    r.seq,
                    r.ts
                );
            }
            if !kind::ALL.contains(&r.kind.as_str()) {
                bail!("record seq {} has unknown kind {:?}", r.seq, r.kind);
            }
            if r.seq != expect {
                bail!(
                    "record seq {} breaks the gapless chain (expected {expect})",
                    r.seq
                );
            }
            expect += 1;
        }
        let mut hashes = Vec::with_capacity(records.len());
        match self.write_batch(records, &mut hashes) {
            Ok(()) => Ok(hashes),
            Err(e) => {
                self.poisoned = true;
                Err(e.context("append failed midway; writer poisoned (reopen to repair)"))
            }
        }
    }

    fn write_batch(&mut self, records: &[Record], hashes: &mut Vec<[u8; 32]>) -> Result<()> {
        for r in records {
            self.ensure_tail_for(r.seq)?;
            let mut rec = r.clone();
            rec.prev = self.last_hash;
            let bytes = rec.to_cbor_bytes()?;
            let tail = self.tail.as_mut().expect("tail ensured above");
            let (frame, hash) = seal_frame(&tail.segment_key, &bytes)?;
            tail.file
                .write_all(&frame)
                .context("writing frame to tail segment")?;
            tail.bytes += frame.len() as u64;
            self.last_hash = hash;
            self.last_seq = rec.seq;
            hashes.push(hash);
        }
        self.tail
            .as_mut()
            .expect("batch was non-empty")
            .file
            .sync_all()
            .context("fsync tail segment (commit point)")?;
        Ok(())
    }

    /// Make sure there is a writable tail with room: seal the current tail
    /// and start `<seq:016x>.seg` when the threshold is reached (or nothing
    /// is open yet). Called per record, so a segment may finish above the
    /// threshold by at most one frame.
    fn ensure_tail_for(&mut self, seq: u64) -> Result<()> {
        let rotate = match &self.tail {
            None => true,
            Some(t) => t.bytes >= self.rotate_at,
        };
        if !rotate {
            return Ok(());
        }
        if let Some(t) = self.tail.take() {
            // Seal: final fsync; the file is never opened for write again.
            t.file.sync_all().context("fsync sealed segment")?;
        }
        let path = segment_path(&self.data_dir, &self.device, seq);
        let segment_key = derive_segment_key(&self.master, &self.device, seq);
        let header = encode_header(&self.master, &self.device, seq, &segment_key)?;
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("creating segment {}", path.display()))?;
        file.write_all(&header).context("writing segment header")?;
        // Make the new name durable now; the content fsync happens at the
        // batch commit point.
        sync_dir(&device_dir(&self.data_dir, &self.device))?;
        self.tail = Some(Tail {
            file,
            segment_key,
            bytes: header.len() as u64,
        });
        Ok(())
    }

    /// Open-time recovery. Policy (documented in mod.rs): only the tail
    /// segment is repairable — it is truncated at the first invalid frame,
    /// because everything past the last fsync belongs to an unacknowledged
    /// batch. A tail whose *header* is torn is deleted whole (it cannot hold
    /// any acknowledged record: header and first frame are written before
    /// the first commit fsync). Sealed segments are never repaired; damage
    /// there is a hard error surfaced by `LogReader::scan`, not silently
    /// truncated here.
    fn recover(&mut self) -> Result<()> {
        let mut segs = list_segments(&self.data_dir, &self.device)?;
        let Some((tail_start, tail_path)) = segs.pop() else {
            return Ok(()); // empty log; first append creates 0…01.seg
        };
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&tail_path)
            .with_context(|| format!("opening tail segment {}", tail_path.display()))?;
        let header = match read_header(&mut file, &self.master, &self.device)
            .with_context(|| format!("tail segment {}", tail_path.display()))?
        {
            HeaderRead::Ok(h) => h,
            HeaderRead::Torn => {
                // Crash during segment creation: nothing in this file was
                // ever acknowledged. Remove it and fall back to the previous
                // segment (which, having been sealed, must be intact).
                drop(file);
                std::fs::remove_file(&tail_path)
                    .with_context(|| format!("removing torn segment {}", tail_path.display()))?;
                sync_dir(&device_dir(&self.data_dir, &self.device))?;
                let Some((prev_start, prev_path)) = segs.pop() else {
                    return Ok(());
                };
                let (seq, hash) = self.read_sealed_state(prev_start, &prev_path)?;
                self.last_seq = seq;
                self.last_hash = hash;
                // The previous segment stays sealed: the next append opens a
                // fresh segment (rotate_at check sees no tail).
                return Ok(());
            }
        };
        if header.start_seq != tail_start {
            bail!(
                "segment {} header says start seq {}, filename says {tail_start}",
                tail_path.display(),
                header.start_seq
            );
        }
        let walk = self.walk_tail(&mut file, &header, &tail_path)?;
        let valid_end = walk.valid_end;
        if let Some((last_seq, last_hash)) = walk.last {
            self.last_seq = last_seq;
            self.last_hash = last_hash;
        } else {
            // Header-only tail (its first frame was torn away, or never
            // written). Chain state comes from the previous segment.
            match segs.pop() {
                Some((prev_start, prev_path)) => {
                    let (seq, hash) = self.read_sealed_state(prev_start, &prev_path)?;
                    if header.start_seq != seq + 1 {
                        bail!(
                            "segment {} starts at seq {} but the log ends at seq {seq}",
                            tail_path.display(),
                            header.start_seq
                        );
                    }
                    self.last_seq = seq;
                    self.last_hash = hash;
                }
                None => {
                    if header.start_seq != 1 {
                        bail!(
                            "first segment {} starts at seq {}, expected 1",
                            tail_path.display(),
                            header.start_seq
                        );
                    }
                }
            }
        }
        file.set_len(valid_end)
            .with_context(|| format!("truncating torn tail of {}", tail_path.display()))?;
        file.sync_all().context("fsync after tail truncation")?;
        file.seek(SeekFrom::Start(valid_end))
            .context("seeking to tail append position")?;
        self.tail = Some(Tail {
            file,
            segment_key: header.segment_key,
            bytes: valid_end,
        });
        Ok(())
    }

    /// Walk the tail segment's frames. Any invalid frame — torn,
    /// unauthenticated — ends the walk there (see `recover`); authenticated
    /// but undecodable/inconsistent frames are hard errors.
    fn walk_tail(&self, file: &mut File, header: &Header, path: &Path) -> Result<TailWalk> {
        let mut end = header.len;
        let mut state: Option<(u64, [u8; 32])> = None;
        let mut expect_seq = header.start_seq;
        let mut expect_prev: Option<[u8; 32]> = None; // first frame's prev is prior-segment state; not re-verified here
        loop {
            match read_frame(file, &header.segment_key) {
                Ok(FrameRead::Eof) => break,
                Ok(FrameRead::Torn) | Err(_) => break, // truncate here
                Ok(FrameRead::Ok {
                    record_bytes,
                    frame_hash,
                    frame_len,
                }) => {
                    // An authenticated frame is byte-for-byte something this
                    // writer once produced (a torn writeback cannot forge a
                    // valid tag), so it must decode and chain — anything else
                    // is damage repair must not paper over.
                    let rec = Record::from_cbor_bytes(&record_bytes).with_context(|| {
                        format!(
                            "segment {}: authenticated frame for seq {expect_seq} is undecodable",
                            path.display()
                        )
                    })?;
                    let chain_ok = rec.v == ENVELOPE_VERSION
                        && rec.device == self.device
                        && rec.seq == expect_seq
                        && expect_prev.is_none_or(|p| rec.prev == p);
                    if !chain_ok {
                        // An authenticated frame that contradicts the chain
                        // is real corruption, not a torn write; refuse to
                        // shorten the log over it.
                        bail!(
                            "segment {} frame for seq {expect_seq}: authenticated but inconsistent (chain damage?)",
                            path.display()
                        );
                    }
                    end += frame_len;
                    state = Some((rec.seq, frame_hash));
                    expect_seq += 1;
                    expect_prev = Some(frame_hash);
                }
            }
        }
        Ok(TailWalk {
            valid_end: end,
            last: state,
        })
    }

    /// Read a sealed segment strictly (no repair) and return its last
    /// record's (seq, frame_hash). Sealed segments were fsynced at rotation,
    /// so anything short of a fully valid segment is a hard error.
    fn read_sealed_state(&self, start_seq: u64, path: &Path) -> Result<(u64, [u8; 32])> {
        let mut file = File::open(path)
            .with_context(|| format!("opening sealed segment {}", path.display()))?;
        let header = match read_header(&mut file, &self.master, &self.device)
            .with_context(|| format!("sealed segment {}", path.display()))?
        {
            HeaderRead::Ok(h) => h,
            HeaderRead::Torn => bail!("sealed segment {} has a torn header", path.display()),
        };
        if header.start_seq != start_seq {
            bail!(
                "sealed segment {} header start seq {} disagrees with filename",
                path.display(),
                header.start_seq
            );
        }
        let mut expect_seq = header.start_seq;
        let mut last: Option<(u64, [u8; 32])> = None;
        loop {
            match read_frame(&mut file, &header.segment_key)
                .with_context(|| format!("sealed segment {}", path.display()))?
            {
                FrameRead::Eof => break,
                FrameRead::Torn => bail!("sealed segment {} is truncated", path.display()),
                FrameRead::Ok {
                    record_bytes,
                    frame_hash,
                    ..
                } => {
                    let rec = Record::from_cbor_bytes(&record_bytes)
                        .with_context(|| format!("sealed segment {}", path.display()))?;
                    if rec.seq != expect_seq {
                        bail!(
                            "sealed segment {}: seq {} where {expect_seq} expected",
                            path.display(),
                            rec.seq
                        );
                    }
                    last = Some((rec.seq, frame_hash));
                    expect_seq += 1;
                }
            }
        }
        last.with_context(|| format!("sealed segment {} contains no frames", path.display()))
    }
}
