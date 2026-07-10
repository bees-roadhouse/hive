// LogReader — the strict scan over one device's op log. Verifies everything
// the format promises (magic, header sanity, AEAD tags, CBOR envelope, the
// gapless seq, the blake3 prev chain across segment boundaries) and repairs
// nothing: the first invalid frame is reported as an error item and the scan
// fuses. Repair is `LogWriter::open`'s job.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use super::segment::{list_segments, read_frame, read_header, FrameRead, HeaderRead};
use super::{device_id_ok, kind, ts_shape_ok, Record, ENVELOPE_VERSION};
use crate::keys::KeySource;

pub struct LogReader;

impl LogReader {
    /// Scan `device`'s log under `<data_dir>/log/<device>/` in order,
    /// yielding each record with its frame hash. An empty or absent log
    /// yields nothing. The first violation of the format yields one `Err`
    /// describing the frame, then the iterator ends.
    pub fn scan(
        data_dir: &Path,
        device: &str,
        keys: &dyn KeySource,
    ) -> Result<impl Iterator<Item = Result<(Record, [u8; 32])>>> {
        if !device_id_ok(device) {
            bail!("invalid device id {device:?}");
        }
        let master = keys.master_key()?;
        let mut segments = list_segments(data_dir, device)?;
        segments.sort_by_key(|(s, _)| *s);
        Ok(ScanIter {
            device: device.to_string(),
            master,
            segments: segments.into_iter(),
            current: None,
            next_seq: 1,
            prev_hash: [0u8; 32],
            done: false,
        })
    }
}

struct CurrentSegment {
    file: File,
    segment_key: [u8; 32],
    path: PathBuf,
    /// True for the log's final segment: only there may the file end in a
    /// torn frame (an unrepaired crash tail) — which scan still reports,
    /// with a message that says what it is.
    last: bool,
}

struct ScanIter {
    device: String,
    master: [u8; 32],
    segments: std::vec::IntoIter<(u64, PathBuf)>,
    current: Option<CurrentSegment>,
    next_seq: u64,
    prev_hash: [u8; 32],
    done: bool,
}

impl ScanIter {
    /// Open the next segment file and verify its header + contiguity.
    fn open_next_segment(&mut self) -> Result<Option<()>> {
        let Some((start_seq, path)) = self.segments.next() else {
            return Ok(None);
        };
        let last = self.segments.as_slice().is_empty();
        let mut file =
            File::open(&path).with_context(|| format!("opening segment {}", path.display()))?;
        let header = match read_header(&mut file, &self.master, &self.device)
            .with_context(|| format!("segment {}", path.display()))?
        {
            HeaderRead::Ok(h) => h,
            HeaderRead::Torn => {
                if last {
                    bail!(
                        "segment {}: torn header (crash during segment creation; \
                         open a LogWriter to repair)",
                        path.display()
                    );
                }
                bail!("sealed segment {}: torn header", path.display());
            }
        };
        if header.start_seq != start_seq {
            bail!(
                "segment {} header start seq {} disagrees with its filename",
                path.display(),
                header.start_seq
            );
        }
        if header.start_seq != self.next_seq {
            bail!(
                "segment {} starts at seq {} but the log's next seq is {} (gap or missing segment)",
                path.display(),
                header.start_seq,
                self.next_seq
            );
        }
        self.current = Some(CurrentSegment {
            file,
            segment_key: header.segment_key,
            path,
            last,
        });
        Ok(Some(()))
    }

    fn next_item(&mut self) -> Result<Option<(Record, [u8; 32])>> {
        loop {
            if self.current.is_none() && self.open_next_segment()?.is_none() {
                return Ok(None);
            }
            let cur = self.current.as_mut().expect("segment opened above");
            match read_frame(&mut cur.file, &cur.segment_key) {
                Ok(FrameRead::Eof) => {
                    // Clean end of this segment; move on. (A frameless
                    // segment is tolerated only as the tail of a crash that
                    // tore its first frame away — mid-log it would break the
                    // seq contiguity check on the next header anyway.)
                    self.current = None;
                    continue;
                }
                Ok(FrameRead::Torn) => {
                    let what = if cur.last {
                        "torn tail frame (crash artifact; open a LogWriter to repair)"
                    } else {
                        "torn frame inside a sealed segment"
                    };
                    bail!(
                        "segment {}: frame for seq {}: {what}",
                        cur.path.display(),
                        self.next_seq
                    );
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!(
                            "segment {}: frame for seq {}",
                            cur.path.display(),
                            self.next_seq
                        )
                    });
                }
                Ok(FrameRead::Ok {
                    record_bytes,
                    frame_hash,
                    ..
                }) => {
                    let path = cur.path.clone();
                    let rec = Record::from_cbor_bytes(&record_bytes).with_context(|| {
                        format!(
                            "segment {}: frame for seq {}: authenticated but not a valid record",
                            path.display(),
                            self.next_seq
                        )
                    })?;
                    self.verify_envelope(&rec, &path)?;
                    self.prev_hash = frame_hash;
                    self.next_seq += 1;
                    return Ok(Some((rec, frame_hash)));
                }
            }
        }
    }

    fn verify_envelope(&self, rec: &Record, path: &Path) -> Result<()> {
        let at = |what: &str| {
            anyhow!(
                "segment {}: frame for seq {}: {what}",
                path.display(),
                self.next_seq
            )
        };
        if rec.v != ENVELOPE_VERSION {
            return Err(at(&format!(
                "envelope version {} (expected {ENVELOPE_VERSION})",
                rec.v
            )));
        }
        if rec.device != self.device {
            return Err(at(&format!("record names device {:?}", rec.device)));
        }
        if rec.seq != self.next_seq {
            return Err(at(&format!(
                "record seq is {} (gapless chain broken)",
                rec.seq
            )));
        }
        if rec.prev != self.prev_hash {
            return Err(at(
                "prev hash does not match the preceding frame (chain broken)",
            ));
        }
        if !ts_shape_ok(&rec.ts) {
            return Err(at(&format!(
                "ts {:?} is not the frozen 24-char ISO shape",
                rec.ts
            )));
        }
        if !kind::ALL.contains(&rec.kind.as_str()) {
            return Err(at(&format!("unknown kind {:?}", rec.kind)));
        }
        Ok(())
    }
}

impl Iterator for ScanIter {
    type Item = Result<(Record, [u8; 32])>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.next_item() {
            Ok(Some(item)) => Some(Ok(item)),
            Ok(None) => {
                self.done = true;
                None
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}
