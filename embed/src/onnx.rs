// The ort-backed ONNX engine behind the `OnnxProvider` seam — the Rust
// equivalent of embed.ts's @huggingface/transformers provider.
//
// - Embedder: Xenova/bge-small-en-v1.5 (or $HIVE_EMBED_MODEL) — BERT-style
//   feature extraction, mean-pooled over the attention mask + L2-normalized,
//   exactly transformers.js `pipeline("feature-extraction", …, { pooling:
//   "mean", normalize: true })`.
// - Reranker: Xenova/bge-reranker-base (or $HIVE_RERANK_MODEL) — sequence
//   classification over [query, doc] pairs; sigmoid of the single logit.
//
// Models download from the HF hub into `model_cache_dir()` on first use and
// load lazily (Node parity: the pipelines are lazy promises). Every failure
// surfaces as Err so lib.rs's resilience latch degrades to the hash embedder
// (#47) — nothing in here panics on a missing model, bad download, or a
// runtime that won't load. CPU execution provider only.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::{EncodeInput, Tokenizer, TruncationParams};

use crate::{model_cache_dir, set_onnx_provider, OnnxProvider};

/// Build the default engine (lazy — no model IO yet) and install it as the
/// process-wide ONNX provider. Idempotent via the `OnceLock` in lib.rs.
pub fn install_default() {
    set_onnx_provider(Box::new(OnnxEngine::new()));
}

/// One loaded model: tokenizer + ort session. `Session::run` needs `&mut`, so
/// callers go through a `Mutex` (embed/rerank are sync, callers off-hot-path).
struct Model {
    tokenizer: Tokenizer,
    session: Session,
    needs_token_type_ids: bool,
}

pub struct OnnxEngine {
    // Result<…, String> so a load failure latches (Node clears the rejected
    // promise but the outer transformersFailed latch stops retries — same
    // effect: one attempt, then permanent degradation for this process).
    embedder: OnceLock<std::result::Result<Mutex<Model>, String>>,
    reranker: OnceLock<std::result::Result<Mutex<Model>, String>>,
}

impl OnnxEngine {
    pub fn new() -> Self {
        Self {
            embedder: OnceLock::new(),
            reranker: OnceLock::new(),
        }
    }

    fn embedder(&self) -> Result<&Mutex<Model>> {
        self.embedder
            .get_or_init(|| {
                Model::load(crate::embed_repo())
                    .map(Mutex::new)
                    .map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow!("embed model load failed: {e}"))
    }

    fn reranker(&self) -> Result<&Mutex<Model>> {
        self.reranker
            .get_or_init(|| {
                Model::load(crate::rerank_repo())
                    .map(Mutex::new)
                    .map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow!("rerank model load failed: {e}"))
    }
}

impl Default for OnnxEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    fn load(repo: &str) -> Result<Self> {
        let (tokenizer_path, model_path) = fetch_files(repo)?;
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow!("tokenizer load failed: {e}"))?;
        // BGE max sequence length; LongestFirst is the TruncationParams default.
        tokenizer
            .with_truncation(Some(TruncationParams::default()))
            .map_err(|e| anyhow!("tokenizer truncation config failed: {e}"))?;
        // ort's builder errors carry the (non-Send) builder back — stringify
        // instead of `?` so they convert into anyhow cleanly.
        let session = Session::builder()
            .map_err(|e| anyhow!("ort session builder failed: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow!("ort optimization level failed: {e}"))?
            .commit_from_file(&model_path)
            .map_err(|e| anyhow!("onnx session load failed ({}): {e}", model_path.display()))?;
        let needs_token_type_ids = session
            .inputs()
            .iter()
            .any(|i| i.name() == "token_type_ids");
        Ok(Self {
            tokenizer,
            session,
            needs_token_type_ids,
        })
    }

    /// Tokenize + run, returning the first output tensor as (shape, data).
    fn run(&mut self, input: EncodeInput) -> Result<(Vec<i64>, Vec<f32>, Vec<i64>)> {
        let enc = self
            .tokenizer
            .encode(input, true)
            .map_err(|e| anyhow!("tokenize failed: {e}"))?;
        let ids: Vec<i64> = enc.get_ids().iter().map(|&x| x as i64).collect();
        let mask: Vec<i64> = enc.get_attention_mask().iter().map(|&x| x as i64).collect();
        let seq = ids.len();
        if seq == 0 {
            return Err(anyhow!("empty encoding"));
        }
        let shape = [1usize, seq];
        let mut inputs = ort::inputs![
            "input_ids" => Tensor::from_array((shape, ids))?,
            "attention_mask" => Tensor::from_array((shape, mask.clone()))?,
        ];
        if self.needs_token_type_ids {
            let type_ids: Vec<i64> = enc.get_type_ids().iter().map(|&x| x as i64).collect();
            inputs.push((
                "token_type_ids".into(),
                Tensor::from_array((shape, type_ids))?.into(),
            ));
        }
        let outputs = self.session.run(inputs)?;
        let (out_shape, data) = outputs[0].try_extract_tensor::<f32>()?;
        let dims: Vec<i64> = out_shape.iter().copied().collect();
        Ok((dims, data.to_vec(), mask))
    }
}

impl OnnxProvider for OnnxEngine {
    fn embed(&self, text: &str) -> Result<Vec<f32>> {
        let model = self.embedder()?;
        let mut m = model.lock().map_err(|_| anyhow!("embed model poisoned"))?;
        let (dims, data, mask) = m.run(EncodeInput::from(text))?;
        // last_hidden_state: [1, seq, hidden] → masked mean-pool + L2 normalize.
        if dims.len() != 3 {
            return Err(anyhow!("unexpected embed output shape {dims:?}"));
        }
        let (seq, hidden) = (dims[1] as usize, dims[2] as usize);
        let mut pooled = vec![0f32; hidden];
        let mut count = 0f64;
        for t in 0..seq {
            if mask.get(t).copied().unwrap_or(0) == 0 {
                continue;
            }
            count += 1.0;
            let row = &data[t * hidden..(t + 1) * hidden];
            for (p, v) in pooled.iter_mut().zip(row) {
                *p += v;
            }
        }
        if count == 0.0 {
            return Err(anyhow!("attention mask is all zeros"));
        }
        let mut norm = 0f64;
        for p in pooled.iter_mut() {
            *p = (*p as f64 / count) as f32;
            norm += (*p as f64) * (*p as f64);
        }
        let norm = norm.sqrt().max(f64::MIN_POSITIVE);
        Ok(pooled
            .into_iter()
            .map(|x| (x as f64 / norm) as f32)
            .collect())
    }

    fn rerank(&self, query: &str, docs: &[String]) -> Result<Vec<f64>> {
        let model = self.reranker()?;
        let mut m = model.lock().map_err(|_| anyhow!("rerank model poisoned"))?;
        let mut scores = Vec::with_capacity(docs.len());
        for doc in docs {
            // One [query, doc] pair per run — no padding needed, identical
            // scores to Node's padded batch (the mask zeroes padding there).
            let pair = EncodeInput::from((query.to_string(), doc.clone()));
            let (dims, data, _) = m.run(pair)?;
            // bge-reranker emits one logit per pair; sigmoid → 0..1 relevance.
            let logit = *data
                .first()
                .ok_or_else(|| anyhow!("empty rerank logits (shape {dims:?})"))?;
            scores.push(1.0 / (1.0 + (-(logit as f64)).exp()));
        }
        Ok(scores)
    }

    fn supports_rerank(&self) -> bool {
        // Optimistic, like Node's rerankAvailable(): true until a load/run
        // failure latches the provider off (lib.rs handles that on Err).
        true
    }
}

/// Resolve tokenizer.json + the ONNX graph for a hub repo, downloading into
/// `model_cache_dir()` when missing (cache-first, so a warm cache is offline-
/// tolerant). Xenova repos ship `onnx/model_quantized.onnx` and/or
/// `onnx/model.onnx` — try quantized first (what transformers.js defaults to).
fn fetch_files(repo: &str) -> Result<(PathBuf, PathBuf)> {
    let cache = PathBuf::from(model_cache_dir());
    let api = hf_hub::api::sync::ApiBuilder::new()
        .with_cache_dir(cache)
        .with_progress(false)
        .build()
        .context("hf-hub api init failed")?;
    let repo_api = api.model(repo.to_string());
    let tokenizer = repo_api
        .get("tokenizer.json")
        .with_context(|| format!("{repo}: tokenizer.json fetch failed"))?;
    let model = repo_api
        .get("onnx/model_quantized.onnx")
        .or_else(|_| repo_api.get("onnx/model.onnx"))
        .with_context(|| format!("{repo}: onnx model fetch failed"))?;
    Ok((tokenizer, model))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real model download + inference — opt-in (slow, needs network on a cold
    /// cache): `HIVE_ONNX_TEST=1 cargo test -p hive-embed -- --nocapture`.
    #[test]
    fn onnx_engine_embeds_and_reranks() {
        if std::env::var("HIVE_ONNX_TEST").is_err() {
            return;
        }
        let engine = OnnxEngine::new();
        let v = engine.embed("the bees are doing well this spring").unwrap();
        assert_eq!(v.len(), 384);
        let norm: f64 = v.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "not unit length: {norm}");

        let scores = engine
            .rerank(
                "beekeeping in spring",
                &[
                    "Spring hive inspections and swarm prevention.".to_string(),
                    "Quarterly tax filing deadlines for LLCs.".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(scores.len(), 2);
        assert!(scores.iter().all(|s| (0.0..=1.0).contains(s)));
        assert!(scores[0] > scores[1], "rerank order wrong: {scores:?}");
    }
}
