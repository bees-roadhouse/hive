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

/// Whether a cross-encoder reranker is available right now.
pub fn rerank_available() -> bool {
    provider_is_transformers()
        && !TRANSFORMERS_FAILED.load(Ordering::Relaxed)
        && ONNX.get().map(|p| p.supports_rerank()).unwrap_or(false)
}

/// Embed a passage/document into a unit-length vector for the active provider.
pub fn embed(text: &str) -> Vec<f32> {
    if provider_is_transformers() && !TRANSFORMERS_FAILED.load(Ordering::Relaxed) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
