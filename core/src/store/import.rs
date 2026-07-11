// The PR 1.7 import seam: batch-append PRE-BUILT records for the one-shot
// hosted-Postgres importer (the `hive-import` binary).
//
// Why a seam exists at all: every normal store write MINTS ids and
// timestamps in the command layer, but an import must preserve the original
// nanoid ids (citations resolve natively) and the original wall times
// verbatim. `import_batch` therefore accepts finished payloads and record
// timestamps from the caller — and nothing else changes: drafts still flow
// through `Core::commit`, so device/seq/lc assignment, the fsync'd op-log
// append, and the one-transaction fold stay exactly the canonical write
// path. Fail-closed at the edge: unknown kinds and off-shape timestamps are
// rejected here, malformed payloads by the fold itself.
//
// Importer-only by contract (#[doc(hidden)]): production features must keep
// using the typed store surface, which owns emergence, fan-out, and policy.

use anyhow::{bail, Result};
use serde_json::Value as Json;

use super::{Draft, Store};

/// One record-to-be from the importer: a closed-set kind, the ORIGINAL
/// author and timestamp, and a payload already shaped per the fold contract
/// (core/src/fold header) — including the v3 `origin` provenance key.
pub struct ImportRecord {
    /// One of the closed record kinds (oplog::kind::ALL).
    pub kind: String,
    /// Record author — the original writer where the source row names one,
    /// else "importer".
    pub actor: String,
    /// Record timestamp in the frozen 24-char ISO shape (oplog::ts_shape_ok)
    /// — the original wall time, not "now".
    pub ts: String,
    /// Fold-contract payload for `kind`.
    pub payload: Json,
}

impl Store {
    /// Append one ordered batch of import records: validate kind + timestamp
    /// shape, then commit through the canonical path (op-log append + fold in
    /// one transaction). The batch is all-or-nothing.
    #[doc(hidden)]
    pub async fn import_batch(&self, records: Vec<ImportRecord>) -> Result<()> {
        self.run(move |core| {
            let mut drafts = Vec::with_capacity(records.len());
            for r in records {
                let Some(kind) = crate::oplog::kind::ALL
                    .iter()
                    .copied()
                    .find(|k| *k == r.kind)
                else {
                    bail!("import_batch rejects unknown record kind {:?}", r.kind);
                };
                if !crate::oplog::ts_shape_ok(&r.ts) {
                    bail!(
                        "import_batch rejects record ts {:?} (kind {kind}): not the \
                         frozen 24-char ISO shape — normalize before importing",
                        r.ts
                    );
                }
                drafts.push(Draft::new(kind, &r.actor, &r.ts, r.payload));
            }
            core.commit(drafts)
        })
        .await
    }
}
