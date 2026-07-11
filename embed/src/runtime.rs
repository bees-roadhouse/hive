// The config-driven embedder the app actually injects at `Store::new`. It
// resolves ONE inner `Embedder` from an `EmbedConfig` snapshot at construction
// and delegates every trait call to it — so the store, the backfill, and the
// Settings state readout all see the SAME truthful engine.
//
// Why a snapshot rather than a live-swappable engine: the backend choice
// (native ONNX vs. Ollama vs. the CI hash path) is a coarse, rare switch, and
// the config lives in the SQLCipher index which isn't readable until the store
// is open — a chicken/egg the app breaks with a small plaintext sidecar file
// read at boot (see the app's `boot()`), NOT by reopening the store mid-frame.
// Switching backend therefore takes effect on the next launch, and the readout
// tells the user so instead of pretending the new choice is already live. The
// DEFAULT backend is native ONNX, so a fresh user gets real embeddings with no
// restart; only opting into Ollama needs a relaunch. The device, by contrast,
// IS dynamic: the native path auto-detects it at model load every process.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{DefaultEmbedder, Embedder, HashEmbedder};

/// The backend family. `Native` is the on-device ONNX BGE engine (the default);
/// `Ollama` is the optional manual server backend; `Hash` is the offline
/// deterministic path CI forces via `HIVE_EMBED=hash`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    #[default]
    Native,
    Ollama,
    Hash,
}

impl Backend {
    pub fn as_str(self) -> &'static str {
        match self {
            Backend::Native => "native",
            Backend::Ollama => "ollama",
            Backend::Hash => "hash",
        }
    }
    /// Parse the persisted string. Unknown/empty → Native (the safe default).
    /// The legacy scaffold wrote "onnx-local" for the native path; accept it.
    pub fn parse(s: &str) -> Backend {
        match s.trim().to_lowercase().as_str() {
            "ollama" => Backend::Ollama,
            "hash" => Backend::Hash,
            _ => Backend::Native, // "native", "onnx-local", "", anything
        }
    }
}

/// The persisted embedder configuration — small enough to live in a plaintext
/// sidecar file the app reads at boot (it holds no secrets: a server URL and
/// model tags, nothing the encrypted index needs to guard).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmbedConfig {
    pub backend: Backend,
    /// Ollama model tag (only meaningful for `Backend::Ollama`). The native
    /// model is chosen by `$HIVE_EMBED_MODEL` / the sensible BGE default.
    #[serde(default)]
    pub ollama_model: String,
    /// Ollama server base URL (only meaningful for `Backend::Ollama`).
    #[serde(default)]
    pub ollama_url: String,
}

impl EmbedConfig {
    pub fn native() -> Self {
        Self {
            backend: Backend::Native,
            ..Default::default()
        }
    }

    /// The sidecar file path beside a data dir. Plaintext on purpose (see the
    /// module header): boot must read the backend choice BEFORE the encrypted
    /// store opens, and it holds no secrets.
    pub fn sidecar_path(data_dir: &std::path::Path) -> std::path::PathBuf {
        data_dir.join("embedder.json")
    }

    /// Read the sidecar beside `data_dir`. A missing/unreadable/garbled file
    /// yields the native default — the app must always boot into a working
    /// engine, never fail because config drifted.
    pub fn load(data_dir: &std::path::Path) -> Self {
        let path = Self::sidecar_path(data_dir);
        match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|e| {
                tracing::warn!(path = %path.display(), error = %e, "embedder config unreadable; using native default");
                Self::native()
            }),
            Err(_) => Self::native(),
        }
    }

    /// Persist the sidecar beside `data_dir` (atomic-ish: write + rename).
    pub fn save(&self, data_dir: &std::path::Path) -> std::io::Result<()> {
        let path = Self::sidecar_path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path)
    }
}

/// The injected engine. Holds exactly one resolved inner `Embedder`.
#[derive(Clone)]
pub struct RuntimeEmbedder {
    inner: Arc<dyn Embedder>,
}

impl RuntimeEmbedder {
    /// Resolve the inner engine from config. Never fails and never blocks on IO
    /// — the native engine defers its model download/load to first embed, and
    /// Ollama defers to its first request; both degrade to the hash fallback on
    /// failure (and say so via `latched()`), so construction is always cheap
    /// and infallible.
    pub fn from_config(cfg: &EmbedConfig) -> Self {
        let inner: Arc<dyn Embedder> = match cfg.backend {
            Backend::Hash => Arc::new(HashEmbedder),
            Backend::Native => Arc::new(DefaultEmbedder),
            Backend::Ollama => Arc::new(crate::ollama::OllamaEmbedder::new(
                cfg.ollama_url.clone(),
                cfg.ollama_model.clone(),
            )),
        };
        Self { inner }
    }

    /// The env/CI floor: `HIVE_EMBED=hash` forces the hash path regardless of
    /// the persisted backend, so a test/CI run never touches a model, the
    /// network, or a GPU. Callers building the app embedder route through here.
    pub fn from_config_or_env(cfg: &EmbedConfig) -> Self {
        if std::env::var("HIVE_EMBED").map(|v| v.to_lowercase()) == Ok("hash".to_string()) {
            return Self {
                inner: Arc::new(HashEmbedder),
            };
        }
        Self::from_config(cfg)
    }
}

impl Embedder for RuntimeEmbedder {
    fn model(&self) -> String {
        self.inner.model()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        self.inner.embed(text)
    }
    fn embed_query(&self, text: &str) -> Vec<f32> {
        self.inner.embed_query(text)
    }
    fn rerank_available(&self) -> bool {
        self.inner.rerank_available()
    }
    fn rerank(&self, query: &str, docs: &[String]) -> Option<Vec<f64>> {
        self.inner.rerank(query, docs)
    }
    fn latched(&self) -> bool {
        self.inner.latched()
    }
    fn backend(&self) -> String {
        self.inner.backend()
    }
    fn device(&self) -> String {
        self.inner.device()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_parse_accepts_legacy_and_defaults_native() {
        assert_eq!(Backend::parse("ollama"), Backend::Ollama);
        assert_eq!(Backend::parse("hash"), Backend::Hash);
        assert_eq!(Backend::parse("native"), Backend::Native);
        // The Phase-2.1 scaffold persisted "onnx-local" for the native path.
        assert_eq!(Backend::parse("onnx-local"), Backend::Native);
        assert_eq!(Backend::parse(""), Backend::Native);
        assert_eq!(Backend::parse("garbage"), Backend::Native);
    }

    #[test]
    fn config_routes_to_the_right_backend() {
        // Hash: deterministic, offline, never a model.
        let e = RuntimeEmbedder::from_config(&EmbedConfig {
            backend: Backend::Hash,
            ..Default::default()
        });
        assert_eq!(e.backend(), "hash");
        assert_eq!(e.device(), "hash");
        assert_eq!(e.model(), crate::HASH_MODEL);

        // Ollama: the model tag is echoed into the stored-vector stamp; no
        // request is made here (dim defers), so this stays offline.
        let e = RuntimeEmbedder::from_config(&EmbedConfig {
            backend: Backend::Ollama,
            ollama_model: "nomic-embed-text".to_string(),
            ollama_url: "http://localhost:11434".to_string(),
        });
        assert_eq!(e.backend(), "ollama");
        assert_eq!(e.device(), "Ollama");
        assert_eq!(e.model(), "ollama:nomic-embed-text");
    }

    #[test]
    fn env_hash_overrides_persisted_backend() {
        // Belt-and-braces: even if the user persisted Ollama, HIVE_EMBED=hash
        // (CI/tests) must force the hash path. Guarded so we don't stomp a
        // parallel test's env expectations.
        if std::env::var("HIVE_EMBED").map(|v| v.to_lowercase()) == Ok("hash".to_string()) {
            let e = RuntimeEmbedder::from_config_or_env(&EmbedConfig {
                backend: Backend::Ollama,
                ollama_model: "nomic-embed-text".to_string(),
                ollama_url: "http://localhost:11434".to_string(),
            });
            assert_eq!(e.backend(), "hash", "HIVE_EMBED=hash must force hash");
        }
    }

    #[test]
    fn sidecar_roundtrips_and_missing_file_is_native() {
        let dir = std::env::temp_dir().join(format!("hive-embed-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        // Missing file → native default.
        let _ = std::fs::remove_file(EmbedConfig::sidecar_path(&dir));
        assert_eq!(EmbedConfig::load(&dir).backend, Backend::Native);
        // Save then load preserves the Ollama choice.
        let cfg = EmbedConfig {
            backend: Backend::Ollama,
            ollama_model: "nomic-embed-text".to_string(),
            ollama_url: "http://localhost:11434".to_string(),
        };
        cfg.save(&dir).unwrap();
        let back = EmbedConfig::load(&dir);
        assert_eq!(back.backend, Backend::Ollama);
        assert_eq!(back.ollama_model, "nomic-embed-text");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn embed_config_json_roundtrips_and_tolerates_missing_fields() {
        let json = r#"{"backend":"ollama","ollama_model":"m","ollama_url":"http://x"}"#;
        let cfg: EmbedConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.backend, Backend::Ollama);
        assert_eq!(cfg.ollama_model, "m");
        // Missing optional fields default cleanly (a bare native config).
        let cfg: EmbedConfig = serde_json::from_str(r#"{"backend":"native"}"#).unwrap();
        assert_eq!(cfg.backend, Backend::Native);
        assert_eq!(cfg.ollama_url, "");
    }
}
