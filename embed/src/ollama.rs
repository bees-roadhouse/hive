// Ollama embedding backend — OPTIONAL and MANUAL. Only used when the user has
// explicitly enabled it in Settings with a server URL + a model tag; it is
// never auto-selected. Ollama runs its own process and manages its own GPU, so
// this is the clean immediate GPU route while the native host-side GPU sidecar
// is still a follow-on.
//
// Wire shape (Ollama's embeddings endpoint):
//   POST {url}/api/embeddings
//   { "model": "<tag>", "prompt": "<text>" }  ->  { "embedding": [f32, …] }
//
// The call is synchronous (ureq, the same blocking client hf-hub already links)
// and runs off the hot path via the callers' spawn_blocking, exactly like the
// ONNX engine. Any failure — server down, model absent, malformed reply — is
// caught, logged once, and LATCHES the embedder to the hash fallback so a
// persisting backfill pauses instead of writing mislabeled vectors (parity with
// the ONNX resilience latch). `latched()` then reports the degradation honestly.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::{embed_hash, Embedder, HASH_DIM};

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// A configured Ollama embedder. `dim` is learned from the first successful
/// response (Ollama doesn't advertise it up front); until then we report a
/// nominal 0 so the store's dim checks treat it as "not yet known".
pub struct OllamaEmbedder {
    url: String,
    model: String,
    dim: AtomicUsize,
    failed: AtomicBool,
}

#[derive(Deserialize)]
struct EmbeddingsResponse {
    embedding: Vec<f32>,
}

impl OllamaEmbedder {
    /// Build from a base URL (trailing slash tolerated; empty → localhost
    /// default) and a model tag. An empty model is accepted here but will fail
    /// at request time — the app only constructs this when the user supplied
    /// both, and reports the failure honestly if not.
    pub fn new(url: impl Into<String>, model: impl Into<String>) -> Self {
        let mut url = url.into().trim().trim_end_matches('/').to_string();
        if url.is_empty() {
            url = DEFAULT_OLLAMA_URL.to_string();
        }
        Self {
            url,
            model: model.into().trim().to_string(),
            dim: AtomicUsize::new(0),
            failed: AtomicBool::new(false),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/api/embeddings", self.url)
    }

    /// One embeddings request. Isolated (and pub(crate)) so a test can drive it
    /// against a localhost mock without a real Ollama.
    fn request(&self, text: &str) -> Result<Vec<f32>> {
        if self.model.is_empty() {
            return Err(anyhow!("no ollama model configured"));
        }
        let resp = ureq::post(&self.endpoint())
            .send_json(serde_json::json!({ "model": self.model, "prompt": text }))
            .with_context(|| format!("ollama request to {} failed", self.endpoint()))?;
        let parsed: EmbeddingsResponse = resp
            .into_json()
            .context("ollama response was not the expected {\"embedding\": [...]} shape")?;
        if parsed.embedding.is_empty() {
            return Err(anyhow!("ollama returned an empty embedding"));
        }
        Ok(parsed.embedding)
    }

    fn mark_failed(&self, reason: &str) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                backend = "ollama",
                model = self.model,
                url = self.url,
                reason,
                "ollama embeddings unavailable, using keyword fallback"
            );
        }
    }
}

impl Embedder for OllamaEmbedder {
    fn model(&self) -> String {
        // Stamp stored vectors with a namespaced tag so they never collide with
        // native/hash rows of the same model name and the stats readout is
        // unambiguous.
        format!("ollama:{}", self.model)
    }
    fn dim(&self) -> usize {
        let d = self.dim.load(Ordering::Relaxed);
        if d == 0 {
            HASH_DIM
        } else {
            d
        }
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        if self.failed.load(Ordering::Relaxed) {
            return embed_hash(text);
        }
        match self.request(text) {
            Ok(v) => {
                self.dim.store(v.len(), Ordering::Relaxed);
                v
            }
            Err(e) => {
                self.mark_failed(&format!("{e:#}"));
                embed_hash(text)
            }
        }
    }
    fn embed_query(&self, text: &str) -> Vec<f32> {
        // Ollama embedding models (nomic-embed-text, mxbai, …) don't take the
        // BGE retrieval-instruction prefix, so query == passage here.
        self.embed(text)
    }
    fn rerank_available(&self) -> bool {
        false
    }
    fn rerank(&self, _query: &str, _docs: &[String]) -> Option<Vec<f64>> {
        None
    }
    fn latched(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }
    fn backend(&self) -> String {
        "ollama".to_string()
    }
    fn device(&self) -> String {
        // Ollama owns its own device; from hive's side the honest label is just
        // "Ollama" (we can't and shouldn't claim to know its GPU state).
        "Ollama".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-shot localhost HTTP stub that returns a canned Ollama embeddings
    /// body. No network, no model — exercises the request/response wire shape.
    fn spawn_stub(body: &'static str, status_line: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // Drain the request headers (enough to not RST the client).
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    #[test]
    fn parses_embedding_response() {
        let url = spawn_stub(r#"{"embedding":[0.1,0.2,0.3,0.4]}"#, "HTTP/1.1 200 OK");
        let emb = OllamaEmbedder::new(url, "nomic-embed-text");
        let v = emb.embed("hello world");
        assert_eq!(v, vec![0.1, 0.2, 0.3, 0.4]);
        assert_eq!(emb.dim(), 4, "dim is learned from the response");
        assert!(!emb.latched(), "a good response must not latch");
        assert_eq!(emb.model(), "ollama:nomic-embed-text");
        assert_eq!(emb.backend(), "ollama");
        assert_eq!(emb.device(), "Ollama");
    }

    #[test]
    fn server_error_latches_and_degrades_to_hash() {
        let url = spawn_stub("upstream boom", "HTTP/1.1 500 Internal Server Error");
        let emb = OllamaEmbedder::new(url, "nomic-embed-text");
        let v = emb.embed("hello world");
        // Degraded to a hash vector…
        assert_eq!(v.len(), HASH_DIM);
        // …and latched so a persisting backfill pauses instead of storing it.
        assert!(emb.latched());
    }

    #[test]
    fn empty_model_is_a_clean_failure_not_a_panic() {
        let emb = OllamaEmbedder::new("http://127.0.0.1:1", "");
        let v = emb.embed("hello");
        assert_eq!(v.len(), HASH_DIM);
        assert!(emb.latched());
    }

    #[test]
    fn url_normalizes_trailing_slash_and_empty() {
        let emb = OllamaEmbedder::new("http://host:11434/", "m");
        assert_eq!(emb.endpoint(), "http://host:11434/api/embeddings");
        let emb = OllamaEmbedder::new("   ", "m");
        assert_eq!(emb.endpoint(), "http://localhost:11434/api/embeddings");
    }
}
