// ANN candidate index (PR 1.5): the in-memory approximate-nearest-neighbor
// structure behind semantic retrieval. Shaped to serve the `ann_candidates`
// stage of store/semantic.rs — this layer returns raw (key, similarity)
// candidates; chunk collapse, kind weights, and keyed hydration all happen
// above it, exactly as they do over the Postgres HNSW probes today.
//
// Persistence contract: vectors live in the `embeddings` table (packed
// little-endian f32 BLOBs, columns aligned with the Postgres embeddings
// table); the ANN structure is rebuilt from that table at open and updated
// incrementally on upsert. It is derived-of-derived state — losing it costs a
// rebuild scan, never data.
//
// Implementation choice (PR 1.5 spike result): `usearch` (the plan's first
// choice) was spiked. Its C++/cxx build itself compiles and runs cleanly on
// Linux (add/search/remove all verified) — but the current release line
// (2.26) is an edition-2024 crate (rustc ≥ 1.85) and floors `cxx` at a
// rust-version-1.85 release, both above the workspace's pinned
// rust-version 1.84. Holding it would take a version-archaeology double pin
// (usearch AND transitive cxx), against the workspace's standing MSRV
// discipline (see the chacha20poly1305/keyring pins). Adopting usearch is
// therefore a deliberate MSRV-bump decision for a later PR, and PR 1.5 ships
// the fallback the plan blesses: an exact, deterministic BRUTE-FORCE scan.
// Perf envelope: personal corpora are 10^4–10^5 vectors; at 384 dims a full
// scan is ~15M–150M mul-adds ≈ single-digit milliseconds on desktop hardware,
// comfortably inside interactive search budget. The `AnnIndex` trait is the
// seam: an HNSW implementation drops in behind the same four methods.

/// Approximate-nearest-neighbor index over unit-agnostic f32 vectors.
/// Keys are opaque u64 handles; the `ann_keys` table maps them to
/// (ref_kind, ref_id, chunk_idx).
pub trait AnnIndex: Send {
    /// Insert or replace the vector stored under `key`.
    fn upsert(&mut self, key: u64, vec: &[f32]);
    /// Remove `key` if present (no-op otherwise).
    fn remove(&mut self, key: u64);
    /// The `k` nearest candidates to `query`, best first, as
    /// (key, cosine similarity in -1..=1 — higher is better). Mirrors the
    /// Postgres probe's `1 - (vec_v <=> $1)` orientation.
    fn candidates(&self, query: &[f32], k: usize) -> Vec<(u64, f32)>;
    /// Number of vectors currently indexed.
    fn len(&self) -> usize;
    /// True when nothing is indexed.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Exact scan behind the `AnnIndex` seam (see module header for why this is
/// the PR 1.5 implementation). Deterministic: ties broken by ascending key.
#[derive(Default)]
pub struct BruteForceAnn {
    /// (key, vector) pairs; position tracked by `slots` for O(1) upsert.
    rows: Vec<(u64, Vec<f32>)>,
    slots: std::collections::HashMap<u64, usize>,
}

impl BruteForceAnn {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AnnIndex for BruteForceAnn {
    fn upsert(&mut self, key: u64, vec: &[f32]) {
        match self.slots.get(&key) {
            Some(&i) => self.rows[i].1 = vec.to_vec(),
            None => {
                self.slots.insert(key, self.rows.len());
                self.rows.push((key, vec.to_vec()));
            }
        }
    }

    fn remove(&mut self, key: u64) {
        let Some(i) = self.slots.remove(&key) else {
            return;
        };
        self.rows.swap_remove(i);
        if i < self.rows.len() {
            // The former tail now lives at `i`; repoint its slot.
            self.slots.insert(self.rows[i].0, i);
        }
    }

    fn candidates(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        if k == 0 || self.rows.is_empty() {
            return Vec::new();
        }
        let mut scored: Vec<(u64, f32)> = self
            .rows
            .iter()
            .map(|(key, v)| (*key, cosine_f32(query, v)))
            .collect();
        // Best similarity first; equal scores order by key so replays and
        // rebuilds return identical candidate lists.
        scored.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        scored.truncate(k);
        scored
    }

    fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Cosine similarity in f32 (the trait's score type). Zero or mismatched
/// vectors score 0 rather than NaN — a degenerate row must never poison the
/// sort. Mirrors hive_embed::cosine's shape, kept local so the index layer
/// has no embed-crate dependency.
fn cosine_f32(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// The index implementation PR 1.5 ships (see module header). `dim` is
/// accepted for parity with HNSW constructors that need it up front; the
/// brute-force scan infers per-row.
pub fn new_ann_index(_dim: usize) -> Box<dyn AnnIndex> {
    Box::new(BruteForceAnn::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-vectors (tiny LCG; no RNG crates — this module
    /// sits inside the determinism fence).
    fn vec_for(seed: u64, dim: usize) -> Vec<f32> {
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        (0..dim)
            .map(|_| {
                x = x
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((x >> 33) as f32 / u32::MAX as f32) * 2.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn self_retrieval_and_len() {
        let mut ann = BruteForceAnn::new();
        for key in 1..=50u64 {
            ann.upsert(key, &vec_for(key, 32));
        }
        assert_eq!(ann.len(), 50);
        for key in [1u64, 25, 50] {
            let hits = ann.candidates(&vec_for(key, 32), 3);
            assert_eq!(hits[0].0, key, "own vector must be its own nearest");
            assert!(hits[0].1 > 0.999);
        }
    }

    #[test]
    fn upsert_replaces_and_remove_forgets() {
        let mut ann = BruteForceAnn::new();
        ann.upsert(7, &vec_for(7, 16));
        ann.upsert(8, &vec_for(8, 16));
        // Replace 7 with 9's vector: querying 9's vector now finds key 7.
        ann.upsert(7, &vec_for(9, 16));
        assert_eq!(ann.len(), 2);
        assert_eq!(ann.candidates(&vec_for(9, 16), 1)[0].0, 7);
        ann.remove(7);
        assert_eq!(ann.len(), 1);
        assert!(ann
            .candidates(&vec_for(9, 16), 5)
            .iter()
            .all(|(k, _)| *k != 7));
        ann.remove(7); // absent: no-op
        assert_eq!(ann.len(), 1);
    }

    #[test]
    fn zero_vectors_do_not_poison() {
        let mut ann = BruteForceAnn::new();
        ann.upsert(1, &[0.0; 8]);
        ann.upsert(2, &vec_for(2, 8));
        let hits = ann.candidates(&vec_for(2, 8), 2);
        assert_eq!(hits[0].0, 2);
        assert!(hits.iter().all(|(_, s)| s.is_finite()));
    }
}
