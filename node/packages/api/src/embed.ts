// Local, offline embedder for semantic search.
//
// The rust hive used fastembed (bge-small, 384d). To stay runnable in an
// ephemeral sandbox with no model download, the default provider here is a
// deterministic hashed bag-of-ngrams embedder — meaningful enough to cluster
// related prose, and a drop-in seam for a real transformer (set HIVE_EMBED=
// transformers to load @huggingface/transformers later).

export const EMBED_DIM = 256;
export const EMBED_MODEL = process.env.HIVE_EMBED_MODEL ?? "hash-ngram-v1";

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

/** Embed text into a unit-length EMBED_DIM vector. */
export function embed(text: string): number[] {
  const v = new Array(EMBED_DIM).fill(0);
  const toks = tokens(text);
  const grams: string[] = [...toks];
  for (let i = 0; i < toks.length - 1; i++) grams.push(`${toks[i]}_${toks[i + 1]}`);
  for (const g of grams) {
    const h = hash(g);
    const idx = h % EMBED_DIM;
    const sign = (h >>> 31) & 1 ? -1 : 1;
    v[idx] += sign; // raw count; magnitude grows with term frequency
  }
  let norm = Math.sqrt(v.reduce((s, x) => s + x * x, 0));
  if (norm === 0) norm = 1;
  return v.map((x) => x / norm);
}

export function cosine(a: number[], b: number[]): number {
  let dot = 0;
  for (let i = 0; i < a.length && i < b.length; i++) dot += a[i] * b[i];
  return dot; // both are unit vectors
}

/** Stable content hash so we only re-embed when text changes. */
export function contentHash(text: string): string {
  return hash(text).toString(16);
}
