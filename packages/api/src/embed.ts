// Local embedder + cross-encoder for semantic search, mirroring how
// bees-roadhouse/bookstack-mcp does it (fastembed + BGE there; the Node-native
// equivalent here is @huggingface/transformers running BGE ONNX models from the
// HF hub).
//
// Two providers behind one seam, chosen by $HIVE_EMBED:
//   transformers (default) — the real stack: a small ARM-friendly BGE model,
//                 Xenova/bge-small-en-v1.5 (384d, mean-pooled + L2-normalized),
//                 ONNX via @huggingface/transformers, plus Xenova/bge-reranker-base
//                 as the cross-encoder. Runs on CPU/ARM. One-time model download,
//                 so deployments mount a models cache; embeddings carry their
//                 model, so flipping the model re-backfills only mismatched rows.
//   hash                   — deterministic hashed bag-of-ngrams, 256d. No model
//                 download, instant offline. No reranker. CI selects this
//                 explicitly (HIVE_EMBED=hash) so the seed smoke stays fast and
//                 network-free; it is NOT the default deployments get.
//
// embed()/embedQuery()/rerank() are async (the ONNX pipelines are). cosine()/
// contentHash()/blob helpers stay sync. Embedding only happens on the worker's
// backfill path and the read-side semanticSearch — never inside a better-sqlite3
// write transaction — so the async boundary is clean.
import { logger } from "./log.ts";

const log = logger("embed");

const PROVIDER = (process.env.HIVE_EMBED ?? "transformers").toLowerCase();
const USE_TRANSFORMERS = PROVIDER === "transformers";

// Default to a small (384d) BGE model that runs on ARM/CPU. Override with
// HIVE_EMBED_MODEL (e.g. Xenova/bge-large-en-v1.5 for 1024d on a beefier host,
// or Xenova/all-MiniLM-L6-v2 for a symmetric 384d model — see BGE detection below).
const EMBED_REPO = process.env.HIVE_EMBED_MODEL ?? "Xenova/bge-small-en-v1.5";
const RERANK_REPO = process.env.HIVE_RERANK_MODEL ?? "Xenova/bge-reranker-base";

export const EMBED_MODEL = USE_TRANSFORMERS ? EMBED_REPO : "hash-ngram-v1";
// Nominal dimension for status display. The authoritative dim of a stored
// vector is its own length (written to the `dim` column at upsert time). bge-small
// and all-MiniLM are both 384d; bge-large is 1024d.
export const EMBED_DIM = USE_TRANSFORMERS ? (/bge-large/i.test(EMBED_REPO) ? 1024 : 384) : 256;

// BGE models are asymmetric: queries get an instruction prefix, passages don't.
// This is the exact instruction bookstack-mcp's fastembed BGE applies internally.
// Non-BGE models (e.g. all-MiniLM) are symmetric — no prefix.
const BGE_QUERY_INSTRUCTION = "Represent this sentence for searching relevant passages: ";
const IS_BGE = /bge/i.test(EMBED_REPO);

// Resilience latch: if the transformers model can't load on this host (missing
// model cache, no network, incompatible ONNX runtime), we degrade to the hash
// embedder instead of hard-failing every embed()/rerank()/semanticSearch() call.
// Latched after the first failure so we don't retry-spam the load or the warning.
let transformersFailed = false;
function markTransformersUnavailable(err: unknown): void {
  if (!transformersFailed) {
    transformersFailed = true;
    // Expected, handled condition (no model cache / offline / bad runtime):
    // one clean line, no stack — search just keeps working on the hash path.
    log.warn("embeddings model unavailable, using keyword fallback (rerank disabled)", {
      reason: (err as Error)?.message ?? String(err),
    });
  }
}

/** Whether a cross-encoder reranker is available right now — true only when the
 * transformers provider is selected AND its model actually loaded. Goes false
 * the moment a model load fails, so callers (semanticSearch) stop forcing rerank. */
export const rerankAvailable = (): boolean => USE_TRANSFORMERS && !transformersFailed;

// ---- vector <-> blob (packed little-endian f32, matching bookstack-mcp) -----

export function toBlob(embedding: number[]): Buffer {
  const buf = Buffer.allocUnsafe(embedding.length * 4);
  for (let i = 0; i < embedding.length; i++) buf.writeFloatLE(embedding[i], i * 4);
  return buf;
}

export function fromBlob(blob: Buffer): number[] {
  const out = new Array(blob.length >> 2);
  for (let i = 0; i < out.length; i++) out[i] = blob.readFloatLE(i * 4);
  return out;
}

/** Full cosine similarity — normalizes by both magnitudes (doesn't assume unit
 * vectors), matching bookstack-mcp's `cosine_similarity`. */
export function cosine(a: number[], b: number[]): number {
  let dot = 0;
  let na = 0;
  let nb = 0;
  const n = Math.min(a.length, b.length);
  for (let i = 0; i < n; i++) {
    dot += a[i] * b[i];
    na += a[i] * a[i];
    nb += b[i] * b[i];
  }
  const denom = Math.sqrt(na) * Math.sqrt(nb);
  return denom === 0 ? 0 : dot / denom;
}

// ---- hash provider ---------------------------------------------------------

const HASH_DIM = 256;

const STOP = new Set(
  "the a an and or of to in on for with is are was were be been being this that it as at by from we our you your they their i".split(
    " ",
  ),
);

function tokens(text: string): string[] {
  return text
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s]/gu, " ")
    .split(/\s+/)
    .filter((t) => t.length > 1 && !STOP.has(t));
}

// FNV-1a → bucket index, with a sign hash so collisions can cancel.
function hash(str: string): number {
  let h = 0x811c9dc5;
  for (let i = 0; i < str.length; i++) {
    h ^= str.charCodeAt(i);
    h = Math.imul(h, 0x01000193);
  }
  return h >>> 0;
}

function embedHash(text: string): number[] {
  const v = new Array(HASH_DIM).fill(0);
  const toks = tokens(text);
  const grams: string[] = [...toks];
  for (let i = 0; i < toks.length - 1; i++) grams.push(`${toks[i]}_${toks[i + 1]}`);
  for (const g of grams) {
    const h = hash(g);
    const idx = h % HASH_DIM;
    const sign = (h >>> 31) & 1 ? -1 : 1;
    v[idx] += sign;
  }
  let norm = Math.sqrt(v.reduce((s, x) => s + x * x, 0));
  if (norm === 0) norm = 1;
  return v.map((x) => x / norm);
}

// ---- transformers provider -------------------------------------------------

type Extractor = (
  text: string,
  opts: { pooling: "mean"; normalize: boolean },
) => Promise<{ data: Float32Array }>;
let extractorPromise: Promise<Extractor> | null = null;

function getExtractor(): Promise<Extractor> {
  if (!extractorPromise) {
    // Don't cache a rejected promise — on load failure, clear the cache so the
    // latch (not a poisoned promise) governs the fallback, and rethrow so the
    // caller degrades to hash.
    extractorPromise = (
      import("@huggingface/transformers").then(({ pipeline }) =>
        pipeline("feature-extraction", EMBED_REPO),
      ) as Promise<Extractor>
    ).catch((err) => {
      extractorPromise = null;
      throw err;
    });
  }
  return extractorPromise;
}

/** Embed via transformers; on any model-load/run failure, latch the provider as
 *  unavailable and fall back to the hash embedder so callers never throw. */
async function embedTransformers(text: string): Promise<number[]> {
  if (transformersFailed) return embedHash(text);
  try {
    const extractor = await getExtractor();
    const out = await extractor(text, { pooling: "mean", normalize: true });
    return Array.from(out.data);
  } catch (err) {
    markTransformersUnavailable(err);
    return embedHash(text);
  }
}

// Cross-encoder: scores [query, doc] pairs jointly. Lazily loaded once.
interface RerankBundle {
  tokenizer: (
    queries: string[],
    opts: { text_pair: string[]; padding: boolean; truncation: boolean },
  ) => Record<string, unknown>;
  model: (inputs: Record<string, unknown>) => Promise<{ logits: { sigmoid(): { tolist(): number[][] } } }>;
}
let rerankPromise: Promise<RerankBundle> | null = null;

function getReranker(): Promise<RerankBundle> {
  if (!rerankPromise) {
    rerankPromise = import("@huggingface/transformers")
      .then(async ({ AutoTokenizer, AutoModelForSequenceClassification }) => {
        const [tokenizer, model] = await Promise.all([
          AutoTokenizer.from_pretrained(RERANK_REPO),
          AutoModelForSequenceClassification.from_pretrained(RERANK_REPO),
        ]);
        return {
          tokenizer: (queries, opts) => tokenizer(queries, opts) as Record<string, unknown>,
          model: (inputs) => model(inputs),
        } as RerankBundle;
      })
      .catch((err) => {
        // Same as the extractor: don't cache the rejection, latch + rethrow so
        // rerank() returns null (search keeps working without the cross-encoder).
        rerankPromise = null;
        throw err;
      });
  }
  return rerankPromise;
}

// ---- public seam -----------------------------------------------------------

/** Embed a passage/document into a unit-length vector for the active provider. */
export async function embed(text: string): Promise<number[]> {
  return USE_TRANSFORMERS ? embedTransformers(text) : embedHash(text);
}

/** Embed a search query. For BGE this adds the retrieval instruction prefix; a
 * symmetric model (e.g. all-MiniLM) embeds the query as-is; the hash provider is
 * identical to embed(). */
export async function embedQuery(text: string): Promise<number[]> {
  if (!USE_TRANSFORMERS) return embedHash(text);
  return embedTransformers(IS_BGE ? `${BGE_QUERY_INSTRUCTION}${text}` : text);
}

/** Cross-encoder relevance scores for each doc against the query, in input
 * order. Returns null when no reranker is available — the hash provider, or a
 * transformers model that failed to load (search then keeps its blended order
 * instead of crashing). */
export async function rerank(query: string, docs: string[]): Promise<number[] | null> {
  if (!USE_TRANSFORMERS || transformersFailed || docs.length === 0) return null;
  try {
    const { tokenizer, model } = await getReranker();
    const inputs = tokenizer(new Array(docs.length).fill(query), {
      text_pair: docs,
      padding: true,
      truncation: true,
    });
    const { logits } = await model(inputs);
    // bge-reranker emits one logit per pair; sigmoid → a 0..1 relevance score.
    return logits.sigmoid().tolist().map((row) => row[0]);
  } catch (err) {
    markTransformersUnavailable(err);
    return null;
  }
}

/** Stable content hash so we only re-embed when the text changes. */
export function contentHash(text: string): string {
  return hash(text).toString(16);
}
