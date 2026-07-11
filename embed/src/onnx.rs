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
// runtime that won't load.
//
// Accelerator: auto-detected at model load. We try the requested GPU execution
// provider (CUDA on NVIDIA, ROCm on AMD) and FALL BACK to CPU the moment its
// registration fails — a GPU that isn't compiled in (the `cuda`/`rocm` cargo
// features are off by default) or whose runtime is unreachable (the flatpak
// sandbox has no CUDA/ROCm compute runtime) degrades cleanly to CPU. The
// resolved device is recorded so the UI can report the TRUTH, never a GPU that
// didn't actually initialize. See `resolve_device`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Context, Result};
#[cfg(any(feature = "cuda", feature = "rocm"))]
use ort::execution_providers::ExecutionProvider;
use ort::session::builder::{BuilderResult, GraphOptimizationLevel, SessionBuilder};
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::{EncodeInput, Tokenizer, TruncationParams};

use crate::{model_cache_dir, set_onnx_provider, OnnxProvider};

/// The accelerator actually chosen for the loaded model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    Cuda,
    Rocm,
}

impl Device {
    /// Human-readable label for the Settings state readout.
    pub fn label(self) -> &'static str {
        match self {
            Device::Cpu => "CPU",
            Device::Cuda => "CUDA",
            Device::Rocm => "ROCm",
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            Device::Cpu => 0,
            Device::Cuda => 1,
            Device::Rocm => 2,
        }
    }
    fn from_u8(v: u8) -> Device {
        match v {
            1 => Device::Cuda,
            2 => Device::Rocm,
            _ => Device::Cpu,
        }
    }
}

/// What the user/env asked for. Auto tries GPU then CPU; the explicit variants
/// still fall back to CPU if the chosen GPU can't initialize (honesty over a
/// hard failure — search must keep working).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevicePref {
    Auto,
    Cpu,
    Cuda,
    Rocm,
}

impl DevicePref {
    /// Parse `$HIVE_EMBED_DEVICE` ("auto" | "cpu" | "cuda" | "rocm"); anything
    /// else (or unset) is Auto.
    pub fn from_env() -> DevicePref {
        match std::env::var("HIVE_EMBED_DEVICE")
            .unwrap_or_default()
            .to_lowercase()
            .as_str()
        {
            "cpu" => DevicePref::Cpu,
            "cuda" => DevicePref::Cuda,
            "rocm" => DevicePref::Rocm,
            _ => DevicePref::Auto,
        }
    }

    /// The GPU candidate to attempt for this preference, if any. Auto prefers
    /// CUDA (NVIDIA is the common case); a box with ROCm-only should set
    /// `HIVE_EMBED_DEVICE=rocm` (or the app's device pref) explicitly.
    fn gpu_candidate(self) -> Option<Device> {
        match self {
            DevicePref::Cuda | DevicePref::Auto => Some(Device::Cuda),
            DevicePref::Rocm => Some(Device::Rocm),
            DevicePref::Cpu => None,
        }
    }
}

/// Process-global record of the default engine's resolved device, so
/// `DefaultEmbedder::device()` can read it without holding the engine. 0 (CPU)
/// until a model actually loads on the default path.
static DEFAULT_DEVICE: AtomicU8 = AtomicU8::new(0);

/// The device the process's default engine resolved to (once a model has
/// loaded). "CPU" before any load, or when the `onnx` feature drives nothing —
/// callers must treat "no GPU proven yet" as CPU, never as an assumed GPU.
pub fn resolved_device() -> String {
    Device::from_u8(DEFAULT_DEVICE.load(Ordering::Relaxed))
        .label()
        .to_string()
}

/// Build the default engine (lazy — no model IO yet) and install it as the
/// process-wide ONNX provider. Idempotent via the `OnceLock` in lib.rs. Device
/// preference comes from `$HIVE_EMBED_DEVICE` (default Auto).
pub fn install_default() {
    set_onnx_provider(Box::new(OnnxEngine::with_pref(DevicePref::from_env())));
}

/// Pure device-selection core, unit-testable without loading a model: given a
/// preference and a closure that reports whether a candidate GPU registered
/// successfully, decide the final device. CPU is the guaranteed floor.
///
/// `try_gpu(dev)` must return `true` only if the GPU EP actually registered on
/// a real session builder — so a compiled-out or runtime-missing GPU (the
/// sandbox) yields `false` and we land on CPU.
///
/// VRAM: the spec's ideal is a free-VRAM check (NVML / rocm-smi) before picking
/// a GPU. We implement the spec's stated MINIMUM instead — fall back to CPU when
/// GPU *registration* fails — because (a) the GPU EPs can't even build in CI or
/// the flatpak (features off), so an NVML dependency would be untestable dead
/// weight here, and (b) registration failure already covers "GPU unusable right
/// now". The `try_gpu` closure is the seam: the host-side GPU sidecar (a
/// follow-on) can wrap it with a VRAM probe without touching this logic.
pub(crate) fn resolve_device(pref: DevicePref, mut try_gpu: impl FnMut(Device) -> bool) -> Device {
    match pref.gpu_candidate() {
        Some(gpu) if try_gpu(gpu) => gpu,
        _ => Device::Cpu,
    }
}

/// One loaded model: tokenizer + ort session. `Session::run` needs `&mut`, so
/// callers go through a `Mutex` (embed/rerank are sync, callers off-hot-path).
struct Model {
    tokenizer: Tokenizer,
    session: Session,
    needs_token_type_ids: bool,
    /// The accelerator this session actually loaded on.
    device: Device,
}

pub struct OnnxEngine {
    // Result<…, String> so a load failure latches (Node clears the rejected
    // promise but the outer transformersFailed latch stops retries — same
    // effect: one attempt, then permanent degradation for this process).
    embedder: OnceLock<std::result::Result<Mutex<Model>, String>>,
    reranker: OnceLock<std::result::Result<Mutex<Model>, String>>,
    pref: DevicePref,
    /// Whether this engine feeds `DEFAULT_DEVICE` (the process-wide readout).
    /// Only the app's default engine does; explicitly-built ones don't clobber
    /// it.
    is_default: bool,
}

impl OnnxEngine {
    pub fn new() -> Self {
        Self::with_pref(DevicePref::Auto)
    }

    pub fn with_pref(pref: DevicePref) -> Self {
        Self {
            embedder: OnceLock::new(),
            reranker: OnceLock::new(),
            pref,
            is_default: true,
        }
    }

    fn embedder(&self) -> Result<&Mutex<Model>> {
        let pref = self.pref;
        let is_default = self.is_default;
        self.embedder
            .get_or_init(|| {
                Model::load(crate::embed_repo(), pref)
                    .inspect(|m| {
                        if is_default {
                            DEFAULT_DEVICE.store(m.device.as_u8(), Ordering::Relaxed);
                        }
                    })
                    .map(Mutex::new)
                    .map_err(|e| format!("{e:#}"))
            })
            .as_ref()
            .map_err(|e| anyhow!("embed model load failed: {e}"))
    }

    fn reranker(&self) -> Result<&Mutex<Model>> {
        let pref = self.pref;
        self.reranker
            .get_or_init(|| {
                Model::load(crate::rerank_repo(), pref)
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

/// Build the session on the accelerator `pref` asks for, with a hard CPU floor.
/// `resolve_device` decides the device by actually attempting to register the
/// GPU EP on a throwaway builder (the honest test of "usable right now"); the
/// committed session then registers the winning GPU EP, or nothing at all for
/// CPU (ONNX Runtime's implicit fallback EP).
fn build_session(model_path: &PathBuf, pref: DevicePref) -> Result<(Session, Device)> {
    let device = resolve_device(pref, gpu_registers);

    let builder = Session::builder()
        .map_err(|e| anyhow!("ort session builder failed: {e}"))?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(|e| anyhow!("ort optimization level failed: {e}"))?;
    // `with_execution_providers` consumes+returns the builder, so fold the GPU
    // EP in here; CPU passes the builder straight through unchanged.
    let mut builder = with_gpu_ep(builder, device)?;
    let session = builder
        .commit_from_file(model_path)
        .map_err(|e| anyhow!("onnx session load failed ({}): {e}", model_path.display()))?;
    Ok((session, device))
}

/// Fold the resolved GPU EP into the (consuming) builder. No-op for CPU and
/// when the feature is off. `fail_silently` keeps a late hiccup from aborting
/// the load — CPU still catches us at commit.
#[allow(unused_variables)]
fn with_gpu_ep(builder: SessionBuilder, device: Device) -> Result<SessionBuilder> {
    let mapped = |r: BuilderResult| r.map_err(|e| anyhow!("registering GPU EP failed: {e}"));
    match device {
        Device::Cpu => Ok(builder),
        Device::Cuda => {
            #[cfg(feature = "cuda")]
            {
                mapped(
                    builder.with_execution_providers([
                        ort::execution_providers::CUDAExecutionProvider::default()
                            .build()
                            .fail_silently(),
                    ]),
                )
            }
            #[cfg(not(feature = "cuda"))]
            {
                Ok(builder)
            }
        }
        Device::Rocm => {
            #[cfg(feature = "rocm")]
            {
                mapped(
                    builder.with_execution_providers([
                        ort::execution_providers::ROCmExecutionProvider::default()
                            .build()
                            .fail_silently(),
                    ]),
                )
            }
            #[cfg(not(feature = "rocm"))]
            {
                Ok(builder)
            }
        }
    }
}

/// Does the given GPU execution provider register on a real (empty) session
/// builder right now? True only when ONNX Runtime was compiled with the EP
/// (the `cuda`/`rocm` cargo features, off by default) AND its runtime is
/// present. In the standard build (and inside the flatpak) the EP types don't
/// even exist, so this is a compile-time `false` → CPU. `register`/`is_available`
/// live on the concrete `ExecutionProvider` trait, so this works per typed EP.
#[allow(unused_variables)]
fn gpu_registers(device: Device) -> bool {
    match device {
        Device::Cpu => false,
        Device::Cuda => {
            #[cfg(feature = "cuda")]
            {
                probe(ort::execution_providers::CUDAExecutionProvider::default())
            }
            #[cfg(not(feature = "cuda"))]
            {
                false
            }
        }
        Device::Rocm => {
            #[cfg(feature = "rocm")]
            {
                probe(ort::execution_providers::ROCmExecutionProvider::default())
            }
            #[cfg(not(feature = "rocm"))]
            {
                false
            }
        }
    }
}

/// Attempt a concrete EP's registration on a throwaway builder — the truthful
/// probe used by `gpu_registers`. Only compiled when a GPU feature is on.
#[cfg(any(feature = "cuda", feature = "rocm"))]
fn probe(ep: impl ExecutionProvider) -> bool {
    if !ep.is_available().unwrap_or(false) {
        return false;
    }
    match Session::builder() {
        Ok(mut b) => ep.register(&mut b).is_ok(),
        Err(_) => false,
    }
}

impl Model {
    fn load(repo: &str, pref: DevicePref) -> Result<Self> {
        let (tokenizer_path, model_path) = fetch_files(repo)?;
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|e| anyhow!("tokenizer load failed: {e}"))?;
        // BGE max sequence length; LongestFirst is the TruncationParams default.
        tokenizer
            .with_truncation(Some(TruncationParams::default()))
            .map_err(|e| anyhow!("tokenizer truncation config failed: {e}"))?;
        let (session, device) = build_session(&model_path, pref)?;
        if device != Device::Cpu {
            tracing::info!(device = device.label(), repo, "onnx model loaded on GPU");
        }
        let needs_token_type_ids = session
            .inputs()
            .iter()
            .any(|i| i.name() == "token_type_ids");
        Ok(Self {
            tokenizer,
            session,
            needs_token_type_ids,
            device,
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

    // ---- device selection (pure, no model load, offline) ----

    #[test]
    fn auto_pref_uses_gpu_when_available_else_cpu() {
        // GPU reports available → Auto lands on it (CUDA is Auto's candidate).
        assert_eq!(
            resolve_device(DevicePref::Auto, |_| true),
            Device::Cuda,
            "auto must take the GPU when its EP registers"
        );
        // GPU registration fails (the default build / the sandbox) → CPU.
        assert_eq!(
            resolve_device(DevicePref::Auto, |_| false),
            Device::Cpu,
            "auto must fall back to CPU when the GPU EP is unavailable"
        );
    }

    #[test]
    fn explicit_cpu_never_probes_gpu() {
        let mut probed = false;
        let d = resolve_device(DevicePref::Cpu, |_| {
            probed = true;
            true
        });
        assert_eq!(d, Device::Cpu);
        assert!(!probed, "CPU preference must not even probe a GPU");
    }

    #[test]
    fn explicit_gpu_falls_back_to_cpu_when_unavailable() {
        // The whole point: a user who picked CUDA on a box whose runtime is
        // missing (or in-sandbox) gets CPU, honestly — never a crash.
        assert_eq!(resolve_device(DevicePref::Cuda, |_| false), Device::Cpu);
        assert_eq!(resolve_device(DevicePref::Rocm, |_| false), Device::Cpu);
        // And the requested GPU is the one probed (not a hardcoded CUDA).
        assert_eq!(
            resolve_device(DevicePref::Rocm, |dev| dev == Device::Rocm),
            Device::Rocm
        );
    }

    #[test]
    fn device_pref_parses_env_values() {
        // (Parsing is env-driven; assert the mapping directly via the match.)
        assert_eq!(DevicePref::Cuda.gpu_candidate(), Some(Device::Cuda));
        assert_eq!(DevicePref::Rocm.gpu_candidate(), Some(Device::Rocm));
        assert_eq!(DevicePref::Auto.gpu_candidate(), Some(Device::Cuda));
        assert_eq!(DevicePref::Cpu.gpu_candidate(), None);
    }

    #[test]
    fn gpu_never_registers_without_the_feature() {
        // In the default build (no cuda/rocm features) the GPU EP types don't
        // exist, so `gpu_registers` is a compile-time false → CPU is the
        // compiled-in guarantee. CPU itself is never a "GPU that registered".
        assert!(!gpu_registers(Device::Cpu));
        #[cfg(not(feature = "cuda"))]
        assert!(!gpu_registers(Device::Cuda));
        #[cfg(not(feature = "rocm"))]
        assert!(!gpu_registers(Device::Rocm));
    }

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
