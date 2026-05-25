//! Embedder + reranker client.
//!
//! Strategy pending Cera review (see `DESIGN.md` "Embedder" section: option
//! A native fastembed-rs vs option B python sidecar vs option C drop hybrid).
//! Once locked, this crate exposes:
//!
//! - `Embedder::encode(texts: &[&str]) -> Vec<Vec<f32>>`
//! - `Reranker::score(query: &str, docs: &[(i64, &str)]) -> HashMap<i64, f32>`
//!
//! plus the `index_table`/`status` flows that mirror python `hive_embed.py`.

pub const EMBED_MODEL_TAG: &str = "bge-small-en-v1.5";
pub const EMBED_DIM: i64 = 384;
