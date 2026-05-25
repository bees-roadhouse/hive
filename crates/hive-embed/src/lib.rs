//! Embedder + reranker for hybrid search (option-A: native fastembed-rs).
//!
//! Models are downloaded from HuggingFace Hub on first use and cached under
//! `dirs::cache_dir()/fastembed-cache` (override via `FASTEMBED_CACHE_DIR`).
//!
//! Public surface:
//! - `Embedder::new_standard()` ... bge-small-en-v1.5, 384d
//! - `Embedder::new_precision()` ... bge-base-en-v1.5, 768d
//! - `Embedder::embed_one` / `embed_batch`
//! - `Reranker::new()` ... bge-reranker-v2-m3
//! - `Reranker::rerank(query, docs, top_k)`
//! - `store_embedding` / `load_embeddings` (postgres bridge via hive-db)
//! - `backfill::backfill_embeddings` (journal/notes/tasks)

pub mod backfill;

use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result};
use fastembed::{
    EmbeddingModel, RerankInitOptions, RerankerModel, TextEmbedding, TextInitOptions, TextRerank,
};
use hive_db::PgPool;
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub const EMBED_MODEL_TAG: &str = "bge-small-en-v1.5";
pub const EMBED_DIM: i64 = 384;

const RERANKER_TAG: &str = "bge-reranker-v2-m3";

/// Resolve the model cache dir. Honours `FASTEMBED_CACHE_DIR`, else falls
/// back to `dirs::cache_dir()/fastembed-cache`, else `./fastembed-cache`.
fn cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("FASTEMBED_CACHE_DIR") {
        return PathBuf::from(p);
    }
    directories::BaseDirs::new()
        .map(|b| b.cache_dir().join("fastembed-cache"))
        .unwrap_or_else(|| PathBuf::from("./fastembed-cache"))
}

/// Text embedder. Holds the underlying fastembed model behind a Mutex so
/// callers can share &Embedder across threads despite fastembed needing
/// &mut self for `embed`.
pub struct Embedder {
    inner: Mutex<TextEmbedding>,
    pub dim: usize,
    pub model_id: &'static str,
}

impl Embedder {
    /// bge-small-en-v1.5 (384d). Fast default.
    pub fn new_standard() -> Result<Self> {
        Self::with_model(EmbeddingModel::BGESmallENV15, "bge-small-en-v1.5", 384)
    }

    /// bge-base-en-v1.5 (768d). Higher accuracy, slower.
    pub fn new_precision() -> Result<Self> {
        Self::with_model(EmbeddingModel::BGEBaseENV15, "bge-base-en-v1.5", 768)
    }

    fn with_model(model: EmbeddingModel, tag: &'static str, dim: usize) -> Result<Self> {
        let cache = cache_dir();
        if let Err(e) = std::fs::create_dir_all(&cache) {
            tracing::warn!(
                "could not create fastembed cache dir {}: {}",
                cache.display(),
                e
            );
        }
        let opts = TextInitOptions::new(model)
            .with_cache_dir(cache)
            .with_show_download_progress(true)
            .with_max_length(512);
        let inner = TextEmbedding::try_new(opts)
            .with_context(|| format!("failed to init TextEmbedding({tag})"))?;
        Ok(Self {
            inner: Mutex::new(inner),
            dim,
            model_id: tag,
        })
    }

    pub fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed_batch(&[text])?;
        v.pop().context("empty embedding result")
    }

    pub fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let owned: Vec<String> = texts.iter().map(|s| (*s).to_string()).collect();
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("embedder mutex poisoned"))?;
        let out = guard
            .embed(owned, None)
            .context("fastembed embed() failed")?;
        Ok(out)
    }
}

/// Cross-encoder reranker. Wraps fastembed's bge-reranker-v2-m3.
pub struct Reranker {
    inner: Mutex<TextRerank>,
    pub model_id: &'static str,
}

impl Reranker {
    pub fn new() -> Result<Self> {
        let cache = cache_dir();
        let _ = std::fs::create_dir_all(&cache);
        let opts = RerankInitOptions::new(RerankerModel::BGERerankerV2M3)
            .with_cache_dir(cache)
            .with_show_download_progress(true)
            .with_max_length(512);
        let inner = TextRerank::try_new(opts).context("failed to init bge-reranker-v2-m3")?;
        Ok(Self {
            inner: Mutex::new(inner),
            model_id: RERANKER_TAG,
        })
    }

    /// Returns (index_into_documents, score) sorted by score desc, truncated
    /// to top_k. top_k=0 means return all.
    pub fn rerank(
        &self,
        query: &str,
        documents: &[&str],
        top_k: usize,
    ) -> Result<Vec<(usize, f32)>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        let docs: Vec<String> = documents.iter().map(|s| (*s).to_string()).collect();
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("reranker mutex poisoned"))?;
        let results = guard
            .rerank(query.to_string(), docs, false, None)
            .context("fastembed rerank() failed")?;
        let mut scored: Vec<(usize, f32)> =
            results.into_iter().map(|r| (r.index, r.score)).collect();
        // fastembed already returns sorted desc, but be defensive.
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        if top_k > 0 && scored.len() > top_k {
            scored.truncate(top_k);
        }
        Ok(scored)
    }
}

// ---------- storage bridge ---------------------------------------------------

/// sha256(text) as lowercase hex. Used for idempotency on (source, model).
pub fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())
}

/// Upsert an embedding. Delegates to `hive_db::queries::embeddings::upsert`,
/// which handles the (source_table, source_id, model) conflict server-side.
pub async fn store_embedding(
    pool: &PgPool,
    source_table: &str,
    source_id: Uuid,
    model: &str,
    embedding: &[f32],
    content_hash: &str,
) -> hive_db::Result<()> {
    let dim = embedding.len() as i32;
    hive_db::queries::embeddings::upsert(
        pool,
        source_table,
        source_id,
        model,
        dim,
        embedding,
        content_hash,
    )
    .await
}

/// Load every (source_id, vector) pair for a (table, model).
pub async fn load_embeddings(
    pool: &PgPool,
    source_table: &str,
    model: &str,
) -> hive_db::Result<Vec<(Uuid, Vec<f32>)>> {
    let (ids, vecs) = hive_db::queries::embeddings::load_all(pool, source_table, model).await?;
    Ok(ids.into_iter().zip(vecs.into_iter()).collect())
}
