// Local embedder + cross-encoder seam for semantic search — parity port of
// packages/api/src/embed.ts, shared by the api (query-time) and the worker
// (backfill).
//
// Two providers behind one seam, chosen by $HIVE_EMBED:
//   transformers (default) — BGE ONNX models (Xenova/bge-small-en-v1.5, 384d)
//                 plus Xenova/bge-reranker-base as the cross-encoder. The ONNX
//                 engine (ort) plugs in via `set_onnx_provider`; until/unless it
//                 loads, the resilience latch degrades to the hash embedder —
//                 the same #47 behavior the Node API has.
//   hash — deterministic hashed bag-of-ngrams, 256d. No model download, instant
//                 offline, no reranker. CI selects this (HIVE_EMBED=hash).
//
// Vectors are stored as packed little-endian f32 BLOBs; `content_hash` is the
// FNV-1a hex stamp used to decide re-embeds. All of these must match the Node
// implementation bit-for-bit because both sides read the same database.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

#[cfg(feature = "onnx")]
pub mod onnx;

pub const HASH_DIM: usize = 256;
pub const HASH_MODEL: &str = "hash-ngram-v1";
pub const BGE_QUERY_INSTRUCTION: &str = "Represent this sentence for searching relevant passages: ";

fn provider_is_transformers() -> bool {
    static USE_TRANSFORMERS: OnceLock<bool> = OnceLock::new();
    *USE_TRANSFORMERS.get_or_init(|| {
        std::env::var("HIVE_EMBED")
            .map(|v| v.to_lowercase())
            .as_deref()
            .unwrap_or("transformers")
            == "transformers"
    })
}

pub fn embed_repo() -> &'static str {
    static REPO: OnceLock<String> = OnceLock::new();
    REPO.get_or_init(|| {
        std::env::var("HIVE_EMBED_MODEL").unwrap_or_else(|_| "Xenova/bge-small-en-v1.5".to_string())
    })
}

pub fn rerank_repo() -> &'static str {
    static REPO: OnceLock<String> = OnceLock::new();
    REPO.get_or_init(|| {
        std::env::var("HIVE_RERANK_MODEL")
            .unwrap_or_else(|_| "Xenova/bge-reranker-base".to_string())
    })
}

/// The model name stamped on stored vectors.
pub fn embed_model() -> &'static str {
    if provider_is_transformers() {
        embed_repo()
    } else {
        HASH_MODEL
    }
}

/// Nominal dimension for status display.
pub fn embed_dim() -> usize {
    if provider_is_transformers() {
        if embed_repo().to_lowercase().contains("bge-large") {
            1024
        } else {
            384
        }
    } else {
        HASH_DIM
    }
}

fn is_bge() -> bool {
    embed_repo().to_lowercase().contains("bge")
}

/// Where ONNX models cache on disk (the writable data volume; #49).
pub fn model_cache_dir() -> String {
    std::env::var("HIVE_MODEL_CACHE").unwrap_or_else(|_| "/data/models".to_string())
}

// ---- ONNX provider plug-in seam ---------------------------------------------

/// The ONNX engine interface the worker/api wire in (ort-backed). Object-safe so
/// the heavy ort/tokenizers deps stay out of this crate's dependents that don't
/// need them.
pub trait OnnxProvider: Send + Sync {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;
    fn rerank(&self, query: &str, docs: &[String]) -> anyhow::Result<Vec<f64>>;
    fn supports_rerank(&self) -> bool;
}

static ONNX: OnceLock<Box<dyn OnnxProvider>> = OnceLock::new();
static TRANSFORMERS_FAILED: AtomicBool = AtomicBool::new(false);
static WARNED: AtomicBool = AtomicBool::new(false);

/// Install the ort-backed engine (call once at startup when available).
pub fn set_onnx_provider(p: Box<dyn OnnxProvider>) {
    let _ = ONNX.set(p);
}

/// Lazy auto-install (Node parity: transformers.js pipelines are lazy
/// promises — nothing loads until the first embed/rerank). When the `onnx`
/// feature is on, the provider is transformers, and nobody wired an engine
/// explicitly, the first embed()/rerank_available() call installs the default
/// ort engine. The engine itself defers model download/load to first use, so
/// this is cheap; any later load failure surfaces as Err and latches to hash.
fn ensure_default_engine() {
    #[cfg(feature = "onnx")]
    {
        static INSTALL_ONCE: OnceLock<()> = OnceLock::new();
        INSTALL_ONCE.get_or_init(|| {
            if provider_is_transformers()
                && !TRANSFORMERS_FAILED.load(Ordering::Relaxed)
                && ONNX.get().is_none()
            {
                onnx::install_default();
            }
        });
    }
}

fn mark_transformers_unavailable(reason: &str) {
    TRANSFORMERS_FAILED.store(true, Ordering::Relaxed);
    if !WARNED.swap(true, Ordering::Relaxed) {
        // Expected, handled condition: one clean line, no stack — search keeps
        // working on the hash path (#47).
        tracing::warn!(
            reason,
            "embeddings model unavailable, using keyword fallback (rerank disabled)"
        );
    }
}

/// Whether the transformers provider is configured but has latched to the hash
/// fallback (a model-load failure; clears on restart). While latched, embed()
/// returns 256-dim hash vectors although embed_model() still names the ONNX
/// model — callers that persist vectors must pause instead of writing those
/// mislabeled fallback vectors (DIRECTION.md Phase 0 item 4).
pub fn transformers_latched() -> bool {
    provider_is_transformers() && TRANSFORMERS_FAILED.load(Ordering::Relaxed)
}

/// Whether a cross-encoder reranker is available right now.
pub fn rerank_available() -> bool {
    if !provider_is_transformers() || TRANSFORMERS_FAILED.load(Ordering::Relaxed) {
        return false;
    }
    ensure_default_engine();
    ONNX.get().map(|p| p.supports_rerank()).unwrap_or(false)
}

/// Embed a passage/document into a unit-length vector for the active provider.
pub fn embed(text: &str) -> Vec<f32> {
    if provider_is_transformers() && !TRANSFORMERS_FAILED.load(Ordering::Relaxed) {
        ensure_default_engine();
        if let Some(engine) = ONNX.get() {
            match engine.embed(text) {
                Ok(v) => return v,
                Err(e) => mark_transformers_unavailable(&e.to_string()),
            }
        } else {
            mark_transformers_unavailable("no ONNX engine installed");
        }
    }
    embed_hash(text)
}

/// Embed a search query (BGE models get the retrieval instruction prefix).
pub fn embed_query(text: &str) -> Vec<f32> {
    if !provider_is_transformers() {
        return embed_hash(text);
    }
    if is_bge() {
        embed(&format!("{BGE_QUERY_INSTRUCTION}{text}"))
    } else {
        embed(text)
    }
}

/// Cross-encoder relevance scores per doc, in input order. None when no
/// reranker is available (hash provider, or a model that failed to load).
pub fn rerank(query: &str, docs: &[String]) -> Option<Vec<f64>> {
    if !rerank_available() || docs.is_empty() {
        return None;
    }
    match ONNX.get()?.rerank(query, docs) {
        Ok(scores) => Some(scores),
        Err(e) => {
            mark_transformers_unavailable(&e.to_string());
            None
        }
    }
}

// ---- vector <-> blob (packed little-endian f32) ------------------------------

pub fn to_blob(embedding: &[f32]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(embedding.len() * 4);
    for v in embedding {
        buf.extend_from_slice(&v.to_le_bytes());
    }
    buf
}

pub fn from_blob(blob: &[u8]) -> Vec<f32> {
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Full cosine similarity — normalizes by both magnitudes (doesn't assume unit
/// vectors).
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let n = a.len().min(b.len());
    let (mut dot, mut na, mut nb) = (0f64, 0f64, 0f64);
    for i in 0..n {
        dot += a[i] as f64 * b[i] as f64;
        na += a[i] as f64 * a[i] as f64;
        nb += b[i] as f64 * b[i] as f64;
    }
    let denom = na.sqrt() * nb.sqrt();
    if denom == 0.0 {
        0.0
    } else {
        dot / denom
    }
}

// ---- hash provider ------------------------------------------------------------

const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "of", "to", "in", "on", "for", "with", "is", "are", "was",
    "were", "be", "been", "being", "this", "that", "it", "as", "at", "by", "from", "we", "our",
    "you", "your", "they", "their", "i",
];

fn tokens(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|t| t.chars().count() > 1 && !STOP.contains(t))
        .map(String::from)
        .collect()
}

/// FNV-1a over UTF-16 code units (JS charCodeAt parity) → 32-bit hash.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for unit in s.encode_utf16() {
        h ^= unit as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Deterministic hashed bag-of-ngrams embedding (unigrams + bigrams, signed
/// buckets, L2-normalized). 256d.
pub fn embed_hash(text: &str) -> Vec<f32> {
    let mut v = vec![0f32; HASH_DIM];
    let toks = tokens(text);
    let mut grams: Vec<String> = toks.clone();
    for pair in toks.windows(2) {
        grams.push(format!("{}_{}", pair[0], pair[1]));
    }
    for g in &grams {
        let h = fnv1a(g);
        let idx = (h % HASH_DIM as u32) as usize;
        let sign = if (h >> 31) & 1 == 1 { -1f32 } else { 1f32 };
        v[idx] += sign;
    }
    let mut norm = v
        .iter()
        .map(|x| (*x as f64) * (*x as f64))
        .sum::<f64>()
        .sqrt();
    if norm == 0.0 {
        norm = 1.0;
    }
    v.iter().map(|x| (*x as f64 / norm) as f32).collect()
}

/// Stable content hash (FNV-1a hex) so we only re-embed when the text changes.
/// Matches Node's `hash(text).toString(16)` — lowercase hex, no leading zeros.
pub fn content_hash(text: &str) -> String {
    format!("{:x}", fnv1a(text))
}

// ---- chunking -----------------------------------------------------------------

/// Defaults for `chunk_text`. 450 + 60 tokens keeps a worst-case chunk just
/// under the ONNX encoder's 512-token truncation window.
pub const CHUNK_TARGET_TOKENS: usize = 450;
pub const CHUNK_OVERLAP_TOKENS: usize = 60;
pub const CHUNK_MAX_CHUNKS: usize = 64;

/// Split `text` into overlapping chunks for embedding.
///
/// Paragraph-first packing: blank-line paragraphs pack greedily into chunks of
/// roughly `target_tokens`, estimated as chars/4 — provider-independent and
/// deterministic (the ONNX 512-token truncation stays the safety net for
/// underestimates). A single paragraph longer than the target hard-splits into
/// target-sized windows. Each chunk after the first opens with ~`overlap_tokens`
/// of the previous chunk's tail (word-aligned) so meaning that straddles a
/// boundary lands whole in at least one chunk. At most `max_chunks` chunks —
/// the tail beyond the cap is dropped (still reachable via keyword FTS).
/// Empty/whitespace-only input yields no chunks.
pub fn chunk_text(
    text: &str,
    target_tokens: usize,
    overlap_tokens: usize,
    max_chunks: usize,
) -> Vec<String> {
    let text = text.trim();
    if text.is_empty() || max_chunks == 0 {
        return Vec::new();
    }
    let target_chars = target_tokens.saturating_mul(4).max(1);
    // Overlap is context, not content: cap it below half a chunk so packing
    // always advances.
    let overlap_chars = overlap_tokens.saturating_mul(4).min(target_chars / 2);
    if text.chars().count() <= target_chars {
        return vec![text.to_string()];
    }

    // Units: paragraphs, with oversized paragraphs hard-split into
    // target-sized windows (inter-chunk overlap is added at pack time).
    let mut units: Vec<String> = Vec::new();
    for para in text.split("\n\n") {
        let para = para.trim();
        if para.is_empty() {
            continue;
        }
        let chars: Vec<char> = para.chars().collect();
        if chars.len() <= target_chars {
            units.push(para.to_string());
        } else {
            let mut start = 0usize;
            while start < chars.len() {
                let end = (start + target_chars).min(chars.len());
                units.push(chars[start..end].iter().collect());
                start = end;
            }
        }
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_chars = 0usize;
    for unit in units {
        let unit_chars = unit.chars().count();
        if !cur.is_empty() && cur_chars + 2 + unit_chars > target_chars {
            chunks.push(cur);
            if chunks.len() == max_chunks {
                return chunks;
            }
            cur = overlap_tail(chunks.last().unwrap(), overlap_chars);
            cur_chars = cur.chars().count();
        }
        if !cur.is_empty() {
            cur.push_str("\n\n");
            cur_chars += 2;
        }
        cur.push_str(&unit);
        cur_chars += unit_chars;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Last ~`overlap_chars` characters of `s`, advanced to the next word start so
/// a chunk never opens mid-word. May return empty (e.g. one unbroken word).
fn overlap_tail(s: &str, overlap_chars: usize) -> String {
    if overlap_chars == 0 {
        return String::new();
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= overlap_chars {
        return s.to_string();
    }
    let mut start = chars.len() - overlap_chars;
    while start < chars.len() && !chars[start - 1].is_whitespace() {
        start += 1;
    }
    chars[start..]
        .iter()
        .collect::<String>()
        .trim_start()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingEngine;
    impl OnnxProvider for FailingEngine {
        fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            anyhow::bail!("model load failed (test)")
        }
        fn rerank(&self, _query: &str, _docs: &[String]) -> anyhow::Result<Vec<f64>> {
            anyhow::bail!("no reranker (test)")
        }
        fn supports_rerank(&self) -> bool {
            false
        }
    }

    #[test]
    fn onnx_failure_latches_and_is_observable() {
        if !provider_is_transformers() {
            return; // HIVE_EMBED=hash in this environment — nothing to latch.
        }
        // Wire a broken engine before the lazy default can install itself.
        set_onnx_provider(Box::new(FailingEngine));
        let v = embed("latch trip");
        // Degraded to the hash fallback…
        assert_eq!(v.len(), HASH_DIM);
        // …and observable, so persisting callers (worker backfill) can pause
        // instead of storing fallback vectors under the ONNX model tag.
        assert!(transformers_latched());
    }

    #[test]
    fn fnv1a_matches_js() {
        // JS: hash("") = 0x811c9dc5
        assert_eq!(fnv1a(""), 0x811c9dc5);
        // JS: hash("a") = (0x811c9dc5 ^ 97) * 0x01000193 >>> 0 = 0xe40c292c
        assert_eq!(fnv1a("a"), 0xe40c292c);
        assert_eq!(content_hash("a"), "e40c292c");
    }

    #[test]
    fn blob_roundtrip() {
        let v = vec![0.25f32, -1.5, 3.0];
        assert_eq!(from_blob(&to_blob(&v)), v);
    }

    #[test]
    fn hash_embedding_is_unit_length_and_deterministic() {
        let a = embed_hash("the quick brown fox jumps over the lazy dog");
        let b = embed_hash("the quick brown fox jumps over the lazy dog");
        assert_eq!(a, b);
        let norm: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5);
        assert!(cosine(&a, &b) > 0.999);
    }

    #[test]
    fn stop_words_and_short_tokens_drop() {
        assert_eq!(tokens("I am the a x ok fine"), vec!["am", "ok", "fine"]);
    }

    // ---- chunk_text ----

    #[test]
    fn chunk_empty_input_yields_no_chunks() {
        assert!(chunk_text("", 450, 60, 64).is_empty());
        assert!(chunk_text("  \n\n \t ", 450, 60, 64).is_empty());
        assert!(chunk_text("some text", 450, 60, 0).is_empty());
    }

    #[test]
    fn chunk_short_text_is_a_single_untouched_chunk() {
        let text = "[journal] pia: hive check\n\nQueen spotted on frame 4, brood solid.";
        assert_eq!(chunk_text(text, 450, 60, 64), vec![text.to_string()]);
    }

    #[test]
    fn chunk_long_multi_paragraph_packs_paragraph_first() {
        // 12 paragraphs of ~170 chars against a 400-char target: paragraphs
        // must pack ~2 per chunk and never split mid-paragraph.
        let para = |i: usize| format!("Paragraph {i:02} {}", "inspection notes ".repeat(9));
        let text = (0..12).map(para).collect::<Vec<_>>().join("\n\n");
        let chunks = chunk_text(&text, 100, 0, 64);
        assert!(chunks.len() > 1, "long text must split: {}", chunks.len());
        assert!(
            chunks.len() < 12,
            "paragraphs should pack together, not one chunk each: {}",
            chunks.len()
        );
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.chars().count() <= 100 * 4 + 60 * 4 + 2,
                "chunk {i} exceeds target+overlap: {} chars",
                c.chars().count()
            );
        }
        // Every paragraph survives whole in some chunk (packing, not slicing).
        // Compare trimmed: the chunker trims each paragraph unit.
        for i in 0..12 {
            let p = para(i);
            let p = p.trim();
            assert!(
                chunks.iter().any(|c| c.contains(p)),
                "paragraph {i} missing or split"
            );
        }
    }

    #[test]
    fn chunk_overlap_carries_previous_tail() {
        let para = |i: usize| format!("Paragraph {i:02} {}", "overlap continuity ".repeat(10));
        let text = (0..10).map(para).collect::<Vec<_>>().join("\n\n");
        let chunks = chunk_text(&text, 100, 15, 64);
        assert!(chunks.len() > 1);
        for w in chunks.windows(2) {
            let tail = overlap_tail(&w[0], 15 * 4);
            assert!(!tail.is_empty(), "expected a non-empty overlap tail");
            assert!(
                w[1].starts_with(&tail),
                "next chunk must open with the previous tail\ntail: {tail:?}\nnext: {:?}",
                &w[1][..tail.len().min(w[1].len())]
            );
        }
    }

    #[test]
    fn chunk_max_chunks_caps_output() {
        // A paragraph far larger than the target hard-splits into windows;
        // the cap drops the tail.
        let text = "word ".repeat(4000);
        let uncapped = chunk_text(&text, 25, 5, usize::MAX);
        assert!(uncapped.len() > 8);
        let capped = chunk_text(&text, 25, 5, 8);
        assert_eq!(capped.len(), 8);
        assert_eq!(capped[..], uncapped[..8]);
    }
}
