# Prompt: multimodal embeddings + local voice/face identity

Paste into a fresh Claude Code session at the hive repo root. Self-contained on
purpose: assume the session knows nothing about this conversation.

---

I want to design and stage two related features for hive. Read the repo before
proposing anything — do not take my summary below as verified fact, it is
orientation only.

## Orientation

hive is a personal, local-first memory engine in Rust (`~/Desktop/Github/bees-roadhouse/hive`).
Journal-first: I write prose, and tasks/events/entities emerge from it by
anchoring spans of the text. Everything is append-only, encrypted, on my machine.
Read `README.md`, `docs/DIRECTION.md`, `docs/PLAN-v2.1.md`, `docs/THREAT-MODEL.md`
and `AGENTS.md` before you form an opinion. `AGENTS.md` is the conventions doc
and it is binding.

Relevant current state, as of `main` — VERIFY each of these, some may have moved:

- Embeddings are TEXT ONLY, at TWO seams, and a new modality has to cross both:
  `hive_embed::Embedder` (`fn embed(&self, text: &str) -> Vec<f32>`, what
  hive-core takes as `Arc<dyn Embedder>` at `Store::new`) and
  `hive_embed::OnnxProvider` (`fn embed(&self, text: &str) ->
  anyhow::Result<Vec<f32>>`, the ort-backed engine seam kept object-safe so the
  heavy deps stay out of dependents). Neither has a byte or image path. Model is
  `Xenova/bge-small-en-v1.5`, **384 dimensions**, with `bge-reranker-base` as an
  optional cross-encoder, and there is a deterministic hash fallback for tests
  (`HIVE_EMBED=hash`).
- The `embeddings` table is keyed `PRIMARY KEY (ref_kind, ref_id, chunk_idx)`
  with `model`, `dim`, `owner`, `vec`, `hash` columns — so a new modality has a
  natural home in `ref_kind` and `dim` is already per-row.
- The ANN index is **rebuilt into memory at every `Index::open`**
  (`rebuild_ann()`), so vector search never touches disk. This is also the
  startup cost that scales with corpus size.
- Blobs (mail attachments today) already live in a content-addressed, encrypted,
  crypto-shreddable blockstore. `mail_attachments.blob_hash` names them. Nothing
  currently walks those bytes — an attachment is searchable only by its metadata.
- `FOLD_VERSION` is currently 3. **Bumping it WIPES the embeddings table and
  forces a full re-embed.** Batch schema changes accordingly.

## What I want

### 1. Multimodal search over images

Embed image attachments (and any image in the blockstore) so I can search by
uploading an image OR by describing one in text, with results fused into the
existing search ranking.

The hard part is dimensional, not plumbing. CLIP/SigLIP land in their own space
at a different dimension (512/768) from BGE's 384, and text-to-image only works
if both sides share a space. So you cannot simply add image vectors beside the
existing text ones. I see two shapes and want your recommendation with reasoning:

- a SECOND ANN index in CLIP space (images + CLIP-encoded query text), results
  fused into the existing ranking; or
- moving the whole text corpus onto a CLIP-family text encoder, which costs
  retrieval quality on prose.

My prior is that the second one is wrong for a journal app where prose is the
substrate — but argue me out of it if the evidence says otherwise.

### 2. Local voice and face identification for contacts

Identify people in photos and by voice, tied to contact cards, using LOCAL
models only. The mechanism I already believe in, which you should sanity-check
rather than reinvent:

A voiceprint IS a speaker embedding — a vector the model measures, not a
stamped-once id. Identity is a nearest-neighbour match of a new clip's embedding
against a contact's stored reference print(s), above a threshold. It sharpens
three ways: (1) the enrollment centroid tightens as more audio is added; (2)
because raw audio is kept append-only in the blockstore, better prints can be
RE-DERIVED when the model improves; (3) the model itself can adapt over time.
Same pattern for faces. Honest caveat I want preserved in the design: this is a
probabilistic match with a confidence, never a guaranteed unique key. Voices
drift with illness, age and emotion, so there is a false-match rate that more
enrollment data reduces.

Adjacent and worth designing alongside, but do not let it expand scope
uncontrolled — flag it and stage it separately if it is its own feature:
voice-to-journal (push-to-talk, Whisper-class local ASR, spoken text going
through the normal emergence pipeline so `[task:]` and @mentions still
materialise).

## Constraints that are not negotiable

- **Everything local.** Biometrics never leave the device. No cloud model, no
  telemetry — D27 zero-collection is enforced in code, not by policy.
- **Crypto-shreddable.** Face and voice prints are biometric data. Destroying a
  contact must destroy their prints, provably, the same way blob shredding works
  today. This is the strongest reason the design must sit on the blockstore.
- **Append-only.** Raw audio and images are immutable op-log/blockstore
  content. Derived prints are exactly that — derived, and re-derivable.
- **The append-only AI-correction principle.** An AI cleanup (transcription
  correction, grammar) appends a corrected layer LINKED to the original; the UI
  shows the cleaned version with a "view original" toggle; the raw capture is
  never overwritten. I want this formalised as a DIRECTION decision — it is the
  general model for any AI edit of my content, not just transcription.

## What I want back, in this order

1. **A design note** (`docs/` — match the house style, read a neighbouring doc
   first) covering: the shared-space problem and your recommendation, the storage
   shape for prints, the enrollment and matching flow, threshold/confidence
   handling, the re-derivation story, and the privacy argument.
2. **Proposed DIRECTION.md decisions** in the existing D-numbered style, for the
   choices that are architectural rather than tactical — including the
   append-only AI-correction principle.
3. **A staged PR plan** in `docs/PLAN-v2.1.md`'s idiom (numbered PRs, each with
   an explicit gate). Be honest about ordering against the existing roadmap:
   this is currently WISHLIST, not phase-committed, and the local-model runtime
   plus contact cards are prerequisites.
4. **An explicit list of what you could not verify** and what would need a
   spike or a benchmark.

Do NOT start writing implementation code until I have read and approved the
design. If something in my framing is wrong, say so directly — I would rather
be corrected now than after a phase is built on it.

## Working notes for this repo

- Gates are `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, and `HIVE_EMBED=hash cargo test --workspace`.
- ALWAYS gate `cargo build --release -p hive-app` too — borrow-check differs
  between debug and release and it has bitten before.
- One build at a time. Parallel cargo builds have OOM'd this machine.
- `AGENTS.md` mandates test seams: no test constructs a store, domain, device
  identity, node, or device pair any way other than the named helper.
- Claude Code runs inside the `ubuntu-dev` distrobox; host tools need
  `distrobox-host-exec`. `hive-app` needs the webkit/gtk stack to build.
