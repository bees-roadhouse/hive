use hive_shared::*;
use ndarray::Array1;
use ort::{GraphOptimizationLevel, Session};
use std::path::Path;
use tokenizers::Tokenizer;

/// ONNX Runtime embedding engine.
pub struct EmbeddingEngine {
    session: Session,
    tokenizer: Tokenizer,
    model_name: String,
    dim: usize,
}

impl EmbeddingEngine {
    pub fn new<P: AsRef<Path>>(model_path: P, tokenizer_path: P) -> anyhow::Result<Self> {
        let session = Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_intra_threads(4)?
            .commit_from_file(model_path)?;

        let tokenizer = Tokenizer::from_file(tokenizer_path).map_err(|e| anyhow::anyhow!("tokenizer load failed: {:?}", e))?;

        // Infer dimension from output shape
        let dim = session
            .outputs
            .first()
            .and_then(|o| o.dimensions.last().copied())
            .flatten()
            .unwrap_or(384);

        let model_name = "all-MiniLM-L6-v2".to_string();
        Ok(Self { session, tokenizer, model_name, dim })
    }

    pub async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut all = Vec::with_capacity(texts.len());
        for text in texts {
            let encoding = self.tokenizer.encode(text.to_string(), true).map_err(|e| anyhow::anyhow!("tokenize failed: {:?}", e))?;
            let input_ids: Vec<i64> = encoding.get_ids().iter().map(|&id| id as i64).collect();
            let attention_mask: Vec<i64> = encoding.get_attention_mask().iter().map(|&m| m as i64).collect();
            let token_type_ids: Vec<i64> = encoding.get_type_ids().iter().map(|&t| t as i64).collect();

            let input_ids_array = ndarray::Array2::from_shape_vec((1, input_ids.len()), input_ids)?;
            let attention_mask_array = ndarray::Array2::from_shape_vec((1, attention_mask.len()), attention_mask)?;
            let token_type_ids_array = ndarray::Array2::from_shape_vec((1, token_type_ids.len()), token_type_ids)?;

            let outputs = self.session.run(ort::inputs! {
                "input_ids" => input_ids_array,
                "attention_mask" => attention_mask_array,
                "token_type_ids" => token_type_ids_array,
            }?)?;

            let embeddings = outputs["last_hidden_state"]
                .try_extract_tensor::<f32>()?
                .to_owned();

            // Mean pooling with attention mask
            let mask_expanded = attention_mask_array.mapv(|m| m as f32);
            let sum_mask = mask_expanded.sum();
            let pooled = (&embeddings * &mask_expanded.insert_axis(ndarray::Axis(2))).sum_axis(ndarray::Axis(1)) / sum_mask;
            let vec: Vec<f32> = pooled.iter().copied().collect();
            all.push(vec);
        }
        Ok(all)
    }

    pub fn model_name(&self) -> &str { &self.model_name }
    pub fn dim(&self) -> usize { self.dim }
}

/// Simple cosine similarity.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 { 0.0 } else { dot / (norm_a * norm_b) }
}
