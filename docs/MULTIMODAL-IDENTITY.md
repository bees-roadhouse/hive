# Multimodal search and local identity: design note

> **Still PROPOSED, and now argued against an architecture that is gone.**
> [WEB-APP.md](./WEB-APP.md) (2026-08-10) replaced the local-first design this
> note is built on. D37-D40 were never ratified, so nothing here was ever
> adopted; what changed is that half the reasoning no longer describes the
> repo. Read it for the argument, and check every mechanism claim against the
> tree before acting on it.
>
> **What survives.** Part 1's recommendation holds: text stays on BGE, images
> get their own space under their own `ref_kind`, and cross-space results fuse
> by reciprocal rank rather than score. `embeddings` came through the rewrite
> with the same `PRIMARY KEY (ref_kind, ref_id, chunk_idx)` and the same
> per-row `model` and `dim`, so "give each space its own `ref_kind`" is still
> the cheap move and still needs no schema change. Part 3 (enrollment, gating
> on margin as well as similarity, `unknown` as a first-class outcome, and the
> correction that the model must not adapt) is model-level and untouched. Part
> 5's consent argument gets sharper under multi-org, not weaker: a faceprint of
> someone who never agreed to any of this now sits in a shared database.
>
> **What does not.** Two things, and the first inverts a load-bearing claim:
>
> * **"The index is already plural" is no longer true.** `SqliteIndex`,
>   `AnnIndex`, and the `anns: HashMap<String, Box<dyn AnnIndex>>` rebuilt at
>   every open are gone. ANN is pgvector HNSW over one fixed-width column,
>   `embeddings.vec_v vector(384)`
>   (`core/migrations/0002_pgvector_embeddings.sql`); the 256-dim hash tier
>   rides the separate `vec BYTEA` column rather than a second ANN structure.
>   A 512- or 768-dim CLIP space does not fit either. Adding one now means a
>   second vector column or a second table plus its own HNSW index ... more
>   work than this note prices, not less.
> * **Part 2 is void.** No blockstore, no `BlobRef`, no wrapped content key, no
>   `blob_refs`, no fold, no `FOLD_VERSION`. Crypto-shred is gone by decision
>   (WEB-APP.md, *Encryption*), and it is the entire reason Part 2 concludes a
>   print must be a blob. That argument has to be made again from scratch
>   against Postgres, RLS, and `DELETE`. The `blob_refs` / `DROP_DERIVED`
>   hazard the note calls a hard prerequisite ... and item 10 of *What I could
>   not verify* ... is moot: the machinery it breaks does not exist.
>
> One correction to *What the repo actually does today*: contact cards no
> longer exist. `core/src/store/contacts.rs` is still on disk but is not
> declared in `core/src/store/mod.rs`, so it is never compiled, and the
> `contact` entity type is seeded nowhere. The prerequisite this note records
> as already shipped has to ship again.
>
> Kept intact below.

Status: PROPOSED 2026-08-02, not adopted. Written against `main` at 8a1d2bc
from a read of the embedding seam, the index, the blockstore, and the journal
write path. Companion to [DIRECTION.md](./DIRECTION.md) (proposes D37-D40) and
[PLAN-v2.1.md](./PLAN-v2.1.md) (proposes a Phase 8 section, explicitly
wishlist). Nothing here is phase-committed and nothing here should be built
before the decisions it proposes are ratified or rejected.

Two features are designed together because they are the same mechanism seen
twice: **search images by text or by image**, and **identify people by voice
and face against contact cards**. Both are "a vector in a space that is not
BGE's". The second carries a privacy bill the first does not, and that bill —
not the modelling — is what makes it hard.

## Summary

The shared-space problem is real but smaller than it looks: hive **already**
runs one ANN index per model, keyed by the `model` string, with per-index
dimension. A CLIP-space image index is not new architecture, it is a second
entry in a `HashMap` that has held two entries (384-dim BGE and 256-dim hash)
since PR 1.5. What is genuinely missing is plural *embedders* (there is one
global `Arc<dyn Embedder>`), a byte path in the `Embedder` trait, and a fusion
rule that works across spaces whose scores are not commensurable. Moving the
text corpus onto a CLIP text tower is rejected, on evidence, in the next
section.

Biometric templates are forced into the blockstore, as blobs, by the
crypto-shred requirement — and not only the raw audio and images, as the
brief assumed, but **the print vectors themselves**. An op-log record cannot
hold a print (the log is append-only and its segment keys are shared), and an
index row is not individually shreddable. Only a blob has a per-item wrapped
key that can be destroyed. That single constraint determines most of the
storage design.

An identification is a proposal, never a fact. It is closed-set (is this one
of my N enrolled contacts?), it is never written to the log unconfirmed, and
what gets recorded is the human's confirmation, not the model's guess.

One prerequisite is a bug, not a feature — see *The `blob_refs` hazard*. It
must be fixed before any of this is built, and arguably before PR 4.11 lands
FOLD_VERSION 4 regardless of whether this design ever ships.

## What the repo actually does today

Verified against code, since the brief's orientation was right in substance
and wrong in several load-bearing details.

- `hive_embed::Embedder` (`embed/src/lib.rs:211`) is a nine-method trait, not
  a one-method one: `model()`, `dim()`, `embed()`, `embed_query()`,
  `rerank_available()`, `rerank()`, `latched()`, `backend()`, `device()`. It
  is object-safe and held as `Arc<dyn Embedder>`, injected at `Store::new`.
  Every text-bearing method takes `&str`; there is no byte path anywhere, as
  the brief said. The separate `embed_query()` is worth noticing — asymmetric
  query-side and document-side encoding is already a first-class idea here,
  which is exactly the shape a CLIP text tower needs.
- The default model is `Xenova/bge-small-en-v1.5` at **384** dims
  (`embed/src/lib.rs:47`, `embed_dim()` at 69), with
  `Xenova/bge-reranker-base` wired as a cross-encoder used in precision mode
  (`rerank()` is a trait method, not a bolt-on). The hash fallback is **256**
  dims (`HASH_DIM`, `embed/src/lib.rs:29`), not 384 — so a second
  dimensionality is not hypothetical, it ships and CI runs on it.
- `SqliteIndex` holds `anns: HashMap<String, Box<dyn AnnIndex>>`
  (`core/src/index/mod.rs:128`) — **one ANN structure per model, constructed
  with that model's dim** (`new_ann_index(vec.len())`, line 385).
  `rebuild_ann` (line 468) groups by `model` on open. `upsert_embedding`
  already evicts a key from the old model's structure on a model swap
  (line 377). The multi-space index is built.
- `embeddings` is `PRIMARY KEY (ref_kind, ref_id, chunk_idx)` with `model`,
  `dim`, `owner`, `vec`, `hash`, `created_at` (`core/src/index/mod.rs:784`) —
  as the brief said, but note what it implies: **`model` is not in the key**,
  and `ann_keys` carries `UNIQUE (ref_kind, ref_id, chunk_idx)` (line 806).
  One item holds vectors from exactly one model at a time.
- Vectors are **not records**. `embed_backfill.rs` writes
  `SqliteIndex::{remove_embeddings, upsert_embedding}` directly on the writer
  thread; `canonical_dump` excludes them (`core/src/index/mod.rs:501`). They
  are derived-of-derived, which is why PLAN-v2.1 can reject vector sync
  outright.
- `FOLD_VERSION` is 3 (`core/src/fold/mod.rs:209`), and `DROP_DERIVED`
  (`core/src/index/mod.rs:601`) does drop `embeddings` and `ann_keys` on a
  bump, forcing a full re-embed through the backfill. Confirmed. It also
  drops `blob_refs`, which is a separate and much worse problem — below.
- Retrieval fuses lexical and vector by **weighted score sum**, not RRF:
  `vector * 0.7 + keyword * 0.2 + blanket`, or `0.55/0.25` in precision mode,
  times a per-kind weight (`core/src/store/semantic.rs:543`). The ANN path is
  gated by a literal `self.embedder().dim() == 384 && q.len() == 384`
  (line 371).
- Blobs: `BlobRef { manifest_hash, wrapped_key, size, mime, plaintext_hash }`,
  CBOR in `blob_refs(hash PK, ref BLOB, size, mime, created_at)`. Content keys
  are convergent-with-secret —
  `blake3::keyed_hash(derive_key("hive-blob-v1", master), plaintext_hash)`
  (`core/src/blockstore/mod.rs:479`). Crypto-shred is the destruction of the
  `blob_refs` row plus the blocks, proven through replay in
  `core/tests/crypto_shred.rs`. Mail attachments are the **only** producer of
  blobs today, and blob GC is refcounted off `mail_attachments.blob_hash`.
- Nothing anywhere decodes blob content. No image, audio, OCR, or
  transcription code exists in the workspace. Confirmed by grep.
- Contact cards **exist** (`core/src/store/contacts.rs`) as a seeded custom
  entity type, slug `contact`, twelve fields, with `[contact: Name]` bracket
  emergence and a Contacts pane in the app. The brief listed contact cards as
  a prerequisite; slice 1 of that prerequisite already shipped. What has not
  shipped is the identity↔contact link (slice 2, deliberately left as a seam).
- There is no revision or correction model for journal entries. Decisions
  carry `supersedes`; nothing else does. Anchors bind `(entry_id, start, end)`
  as UTF-16 offsets into a specific body (`core/src/index/mod.rs:653`).

## Part 1 — the shared-space problem

### The option to reject, and why

Moving the text corpus onto a CLIP-family text encoder is wrong, and the
brief's prior is correct. Three independent reasons, any one of which is
sufficient:

**Context length.** CLIP's text tower has a hard 77-token limit; SigLIP's is
64. hive chunks prose at a 450-token target with 60-token overlap
(`embed/src/lib.rs:433,448`), tuned for paragraphs. Re-chunking a journal to
fifty-word fragments does not just cost retrieval quality, it dissolves the
semantic unit the whole emergence model is built on — a `[task:]` token and
the sentence explaining it would routinely land in different chunks.

**Training objective.** CLIP and SigLIP text towers are trained for
image-text contrastive alignment over alt-text captions. They are not
retrieval encoders and are consistently poor at text-to-text semantic
similarity relative to purpose-built retrievers. BGE-small-en-v1.5 is a
retrieval model. Swapping it for a caption encoder trades hive's primary use
case for its secondary one.

**Cost of being wrong.** The corpus is prose and it is the substrate. If the
image half disappoints, a second index is deleted; if the text half regresses,
the product regresses, and `golden_retrieval.json` — the cross-backend parity
oracle — churns for reasons that have nothing to do with correctness.

There is no evidence that argues the other way, so this is not close.

### Why the second index is smaller than the brief assumes

The brief frames option A as "a SECOND ANN index". The index is already
plural. `SqliteIndex.anns` is keyed by model string and each entry is
constructed at that model's dimension; the 384-dim BGE structure and the
256-dim hash structure coexist today, and `upsert_embedding` already handles
a key moving between them. A CLIP image index is a third key in that map.

The three things that are actually missing:

**1. One embedder, globally.** `Store` holds a single `Arc<dyn Embedder>`, and
the backfill's skip-map query is `WHERE chunk_idx = 0 AND model = ?1` with
`model = self.embedder().model()` (`core/src/store/embed_backfill.rs:45,54`).
The pipeline assumes one active model. This becomes a small registry: a set of
named **spaces**, each with an embedder, a dim, and the set of `ref_kind`s it
covers. `Store::new` takes the registry instead of the single embedder; the
existing single-embedder call sites keep working by naming the `text` space.

**2. No byte path in the trait.** `Embedder` needs a sibling, not a widened
signature — `embed()` taking `&str` is correct for a text model and forcing
`Option<&[u8]>` into it would make every implementor lie. Propose a separate
`ImageEmbedder` (and later `AudioEmbedder`) trait, and let the registry hold
whichever kind a space needs. The hash fallback gets a byte-hash counterpart
so `HIVE_EMBED=hash` keeps CI offline and deterministic — this is not
optional, it is what makes the whole feature testable.

**3. The `(ref_kind, ref_id, chunk_idx)` key.** Because `model` is not in the
primary key, one item cannot carry vectors in two spaces. There are two ways
out and the cheap one is better: **give each space its own `ref_kind`.** An
image attachment embeds as `ref_kind = 'image'`, a faceprint as `'face'`, a
voiceprint as `'voice'` — different rows, no collision, **no schema change and
no FOLD_VERSION bump.** Adding `model` to the primary key is the honest shape
and should happen eventually, but it costs a bump and a full re-embed, so it
should ride an already-scheduled bump (PR 4.11's FOLD_VERSION 4) rather than
mint its own.

The `dim() == 384` literal at `semantic.rs:371` becomes a per-space property.
That line is currently doing double duty as a latch guard — it also stops a
mid-flight ONNX failure from probing a 384-dim structure with a 256-dim hash
vector. Whatever replaces it must keep that job.

### The modality gap, and the rule it forces

CLIP-family image and text embeddings do **not** interleave. They occupy
separate cones of the shared space — the well-documented modality gap — so a
text vector's cosine to any image is systematically lower than any image's
cosine to any other image. Text-to-image retrieval works because ranking
*within the image set* is preserved, not because the distances are calibrated.

The rule that follows is concrete and load-bearing: **the CLIP-space index
holds images only.** A text query is encoded by the CLIP *text* tower at query
time and searched against that image-only index. Image vectors and text
vectors never share an index, and a result list is never sorted by raw
cross-space score.

### Fusion: rank, not score

This is the sharpest engineering problem and the brief does not name it. The
existing blend adds a cosine from BGE space to a normalized BM25 score with
fixed weights. CLIP cosines are on a different scale entirely — a strong
text-to-image match sits far lower in absolute terms than a mediocre
text-to-text one. Adding them with any fixed weight produces a ranking whose
behaviour changes with corpus composition.

Recommendation: keep the existing weighted blend **inside** the text space,
untouched, and fuse the image-space result list into the final list by
**reciprocal rank**. Two properties make this the right call. It needs no
score calibration, which is the thing that would otherwise need per-corpus
tuning nobody will do. And when a query returns no image results, the output
is bit-identical to today's — which means `golden_retrieval.json` does not
churn, and the parity oracle keeps meaning what it means.

Per-space presentation matters too: an image hit and a journal hit are not
interchangeable answers to the same question, and the UI should say which
space each result came from rather than silently interleaving them.

### The third option, named and deferred

There is a path the brief does not list: caption images with a local
vision-language model and embed the *caption* into the existing BGE space.
Text-to-image search then works with no new index, no new space, and no
fusion problem, and the caption is human-readable and auditable in a way a
vector is not.

It is not the recommendation, because it cannot do image-to-image search at
all, and because a caption is a hallucination surface sitting between you and
your own photograph. But it composes well later, it costs nothing
architecturally — a caption is just text, hive's substrate — and it lands
naturally under D40: a caption is an AI-authored derived layer with
provenance, shown as such, never as your words. Ship at most one of these at
a time.

### Recommendation

**Option A, reframed: plural named spaces, text stays on BGE.** Images embed
into a CLIP-family space under their own `ref_kind`; text queries reach them
through the image model's text tower; results fuse by reciprocal rank. Model
candidate is SigLIP-base (sigmoid loss, stronger zero-shot retrieval than
CLIP ViT-B/32 at comparable cost) with CLIP ViT-B/32 as the fallback if the
ONNX export or the Rust-side preprocessing proves worse. Both are unverified —
see *What I could not verify*.

## Part 2 — the storage shape for prints

### Why a print cannot be a record, and cannot be an index row

The requirement is that destroying a contact destroys their prints, provably,
the way blob shredding works today. Test each option against it.

**An op-log record?** No. The log is append-only and immutable by construction
(D18), and its segments are encrypted per-frame under per-segment keys. To
destroy one print you would have to destroy the segment key, which destroys
every other record in that segment. There is no per-record shred and there is
not meant to be — that is exactly why D19 puts payload bodies in blobs and
leaves the log holding references.

**An index row?** No. `index.db` is SQLCipher-encrypted as one file under one
master-derived key. Deleting the row removes it from the live database, but
that is a delete, not a shred: the page may persist in free space, and — more
decisively — the row is *derived*, so if the durable source still exists the
next rebuild puts it straight back. An index row can only ever be a cache of a
print, never its home.

**A blob.** Yes, and only this. A blob carries its own content key, wrapped
under master and stored in exactly one place (`blob_refs.ref`); destroying
that row destroys the key; the ciphertext blocks become noise. This is proven
end to end, including through full replay, in `core/tests/crypto_shred.rs`.

So: **the print vector itself is a blob.** This is the part the brief
under-specifies — it argues the blockstore is right for the raw audio and
images, which is true, but the derived template is biometric data in its own
right and needs the same treatment. A 192-float voiceprint is a small blob;
that is fine, blobs under 256 KiB are single-chunk.

### The shape

Per contact, per modality:

- **Source captures** — raw audio clips and face crops — are blobs, one per
  capture, exactly like a mail attachment. Op-log records carry the reference,
  the contact id, the modality, and capture provenance (when, from what).
- **Per-capture templates** — one embedding per capture — are blobs. Derived,
  re-derivable, and individually shreddable.
- **The reference set / centroid** is computed at load, not stored as a
  separate durable artifact. Store the per-capture templates; derive the
  centroid. Keeping the individual vectors is what lets you drop one bad
  enrollment, score by max-over-references when there are few, and re-derive
  cleanly.
- **The ANN entry** is an `embeddings` row under `ref_kind = 'voice'` /
  `'face'` — a derived cache of the template blob, dropped on shred like any
  other vector row, and re-derived from the blobs when it is missing (which is
  exactly what a FOLD_VERSION bump makes happen, since the bump drops
  `embeddings` while the templates survive as blobs).

Three consequences worth stating.

**Blob production stops being mail-only.** Today `mail_attachment_store_blob`
is the single producer and GC refcounts off `mail_attachments.blob_hash`.
A second producer needs its own table, its own record kind or `module.doc`
convention, its own refcount source, and its own redaction fold rule. That is
real, bounded work and it is a prerequisite, not a detail.

**Biometric blobs must use random-key put, not convergent.** Content keys are
convergent within the master-key domain
(`blake3::keyed_hash(subkey, plaintext_hash)`), so identical plaintext
re-derives an identical key. Re-ingesting the same enrollment clip after a
shred would resurrect a key a revoked party might hold — D31 already names
this trap and already decided random-key put ships as the default for
grant-referenced and post-shred blobs. Biometric blobs join that set by the
same argument. The price is dedup loss on exactly those blobs, which is
nothing at this scale.

**Print search costs a decrypt at open.** `rebuild_ann` reads vectors from the
`embeddings` table, so the steady-state path is unchanged; the blobs are read
only when the cache is cold or a re-derive runs. At household scale — tens of
contacts, a handful of captures each — this is microseconds and does not
deserve engineering.

### The `blob_refs` hazard — a prerequisite, and a stale mitigation

This one is verified in code, and it blocks the design.

`DROP_DERIVED` includes `DROP TABLE IF EXISTS blob_refs`
(`core/src/index/mod.rs:629`). `blob_refs` is never written by the fold — zero
occurrences in `core/src/fold/mod.rs`. It is written directly by
`Store::mail_attachment_store_blob` (`core/src/store/mail.rs:2277`), outside
the fold, the same way `embeddings` is. A `FOLD_VERSION` bump therefore drops
every wrapped blob key, and replay cannot rebuild them: the `module.doc`
record written at attachment-store time carries exactly `blob_hash` and
`skipped_reason` (`core/src/store/mail.rs:2284`) — enough to repopulate
`mail_attachments`, nothing like enough to repopulate a `BlobRef`.

The hazard is *known*. `core/src/index/mod.rs:72` lists `blob_refs` among
three runtime tables whose rebuild loss is "each one's documented trade", and
says so plainly: "Loses pointers on rebuild". What is not right is the
mitigation the same comment claims, in both halves:

> the Phase 3 mail module re-fetches, and 1.7-imported blobs carry their refs
> in log records instead

**The re-fetch would not fire.** `mail_attachments_pending()` selects
`WHERE blob_hash IS NULL AND skipped_reason IS NULL`
(`core/src/store/mail.rs:2217`), and replay repopulates `blob_hash` from the
`module.doc` records. After a bump every attachment looks fetched. Nothing
re-queues, and the Phase 3 module that was supposed to notice does not exist
yet anyway.

**Imported blobs do not carry their refs in log records.** `hive-import`
routes attachment bytes through `store_attachment_blobs`, which calls
`mail_attachment_store_blob` — the identical runtime path
(`importer/src/lib.rs:1371,1384`, and the comment at line 464 says so). The
`alias` records the importer does emit carry only blob-hash re-keying
(`namespace: "blob", from, to`), not `BlobRef`s. So imported attachments — the
bulk of a migrated corpus — are exactly as exposed as live-synced ones.

Net: FOLD_VERSION 4, scheduled at PR 4.11, permanently destroys every stored
mail attachment, silently, leaving the `mail_attachments` rows looking healthy
and the blocks on disk as undecryptable noise. No test covers it —
`fold_version_bump_resets_tables_and_watermark`
(`core/tests/fold_replay.rs:820`) exercises a bump over journal and config
rows only, never an attachment.

Recoverable in principle is not the same as recovered. Content keys are
convergent (`keyed_hash(derive_key("hive-blob-v1", master), plaintext_hash)`)
and `mail_attachments.blob_hash` is that plaintext blake3, so a tool holding
master could re-derive a content key and trial-decrypt blocks to rediscover
the manifest. No such tool exists in-tree, and software nobody has written is
not a backup story.

This design puts biometric templates in the blockstore. If a fold bump wipes
wrapped keys, a fold bump wipes every print in the house. **The fix is a hard
prerequisite for Phase 8, and it should land regardless of whether Phase 8
ever ships** — proposed as PR 8.0, and honestly it belongs in front of PR 4.11
on its own merits. The two candidate fixes are to stop dropping `blob_refs`
(it is not fold-owned, so its presence in `DROP_DERIVED` is arguably the
actual defect) or to make the wrapped key durable in the log. The second is a
frozen-format change and the first is one line; the first should win unless a
reason appears.

## Part 3 — enrollment, matching, thresholds

The brief's mechanism is right: a voiceprint is a speaker embedding, and
identity is nearest-neighbour above a threshold. Three sharpenings and one
correction.

### Enrollment

Capture N clips per contact (voice: a floor around three to five clips of ten
to twenty seconds, spanning more than one sitting; face: several crops across
lighting and angle). Embed each. Store each template as its own blob. The
reference set is the set; the centroid is the L2-normalized mean of the
L2-normalized templates, computed on load.

Enrollment is explicit and consented. There is no path where ambient audio
silently becomes an enrollment.

### Matching, and what "confidence" should mean

Score a probe against every enrolled contact. Report **two** numbers, and gate
on both:

- **Similarity** — cosine to the best-matching contact's reference set. Use
  max-over-references when the set is small, centroid when it is large; the
  centroid tightens as the set grows, which is the brief's point (1) and it is
  correct.
- **Margin** — the gap between the best and second-best contact.

Absolute similarity thresholds transfer badly across microphones, rooms, and
codecs; margin is far more stable, and a large margin with a mediocre absolute
score is usually a correct identification of someone speaking badly. Gate on
similarity ≥ threshold **and** margin ≥ floor, and emit `unknown` otherwise.

**`unknown` must be a first-class outcome.** This is open-set recognition, and
a system that always names its best guess is a system that names strangers
after your friends.

Thresholds are per-contact, live in `config` records (no schema change), and
are calibrated against an impostor set that comes free: everyone else you have
enrolled. Ship a defensible default, expose the number, and let a
false-match's correction move it.

### The correction: the model does not adapt

The brief's sharpening (3) — "the model itself can adapt over time" — should
be dropped, and this is the part of the framing I would push back on hardest.

Fine-tuning a speaker or face encoder on a handful of local examples is not a
small extension of this design; it is a different project with a much worse
failure mode. It also destroys the property that makes sharpening (2) work: a
template derived under a locally-adapted model is not comparable to one
derived under a stock model, so every adaptation forces a full re-derive of
every contact — from source audio you may deliberately have shredded. Two
sharpenings that undermine each other should not both be in the design.

What actually improves over time, and is enough: the **reference set** grows,
the **centroid** tightens, the **threshold** calibrates against observed
scores, and the **model** is replaced wholesale when a better stock one ships
(the re-derivation story below). Keep three of the four.

### Voices drift — keep that caveat, and put it in the UI

Illness, age, emotion, and a different microphone all move a voiceprint. The
false-match and false-reject rates are real and more enrollment data reduces
but never eliminates them. This belongs in the interface, not just the design
note: a match is shown as a proposal with its score, and rejecting it is one
click and is itself a signal.

## Part 4 — re-derivation, and its honest cost

Because templates are derived and source captures are append-only blobs, a
better model means: register the new space, re-derive every template from its
source blob, write new template blobs, rebuild the ANN entries. Old templates
stay until explicitly shredded — they are blobs, so they can be. Nothing
mutates.

The cost the brief does not price: **re-derivation requires keeping the raw
audio and images forever, and the raw capture is a larger biometric liability
than the template.** A template is a lossy vector; the source is the person's
actual voice and face, and voice cloning from twenty seconds of clean audio is
a solved problem. "Keep everything so we can re-derive" is not automatically
the privacy-maximizing choice, and presenting it as pure upside would be
dishonest.

Design answer: enrollment captures — short, deliberate, consented — are
retained by default. Ambient and passive capture is **not** retained. Each
contact gets a "forget the source captures" action that shreds the sources,
keeps the templates, and records plainly that re-derivation is no longer
possible for that contact. That is a real trade the person whose voice it is
should get to make.

## Part 5 — the privacy argument

Five statements. The first three are inherited; the last two are new and are
why this feature is different from everything else in hive.

**It never leaves the device, in the sense that everything else does.**
Biometric derivation is local ONNX, like embeddings (D27). No cloud model, no
telemetry, and D27's zero-collection is code-enforced by the existing grep
gates, not policy.

**Blast radius is by tier, not by sentiment.** Blocks replicate to a node as
ciphertext addressed by bare id, so a blind node holds biometric blobs it
cannot read — that is the existing E2E-against-the-node claim and it holds
unchanged. A **trusted** node holds master and can read them, exactly as
delta 1 already says for everything else. Biometric data does not get a
special exemption from a tier statement; it gets a sentence in the threat
model saying plainly that trusted-tier means the always-on box can read your
household's faces and voices.

**Shred works, and is the reason for the shape.** Destroying a contact
destroys their source captures and their templates by destroying wrapped keys,
verifiable by the same test that proves it for attachments — subject to the
`blob_refs` prerequisite above, and subject to D19's stated limits (RAM, swap,
pre-shred backups, filesystem forensics). Those limits are not new here but
they land differently on a faceprint than on a PDF, and the threat model
should say so rather than let the existing sentence carry it.

**Closed-set only.** hive never answers "who is this?" It answers "is this one
of the N contacts I enrolled?" There is no gallery beyond your own contacts,
no external database, no import of anyone else's prints, and no export of
yours. This bounds the harm enormously and it should be a stated invariant
rather than an emergent property of not having built the other thing yet.

**Consent belongs to someone who is not the user.** This is the genuinely new
problem. Every other byte in hive is Nate's own content or content addressed
to him. A faceprint of Maggie, or of a friend in a photograph, is biometric
data about a person who never agreed to hive's threat model and cannot revoke
it — and unlike a password, a face cannot be rotated after a breach. The
design's answers are the closed-set rule, per-contact shred as a first-class
and discoverable action, and enrollment being explicit rather than ambient.
The design does not solve it, and should not claim to.

## Part 6 — voice-to-journal, staged separately

Push-to-talk capture, Whisper-class local ASR, transcript into the normal
journal write path so `[task:]`, `[contact:]` and `@mentions` materialise
exactly as typed prose does.

This is its own feature and should be staged as one. It shares the audio
capture path and the blockstore with speaker identification and shares
nothing else: no vector space, no matching, no thresholds, no contact
binding. It is also the easier and more immediately useful of the two, and it
is the feature that forces D40, because raw ASR output of spoken prose is
not what anyone wants to read.

The one real dependency it adds is a second model runtime. Whisper is
encoder-decoder with an autoregressive loop; driving that through `ort`
directly is possible but awkward, and `whisper.cpp` behind a Rust binding is
the pragmatic path at the cost of a C dependency and a second model-cache
story. That is a decision gate with a spike behind it, not a detail.

Scope discipline: transcription writes an ordinary journal entry through
`journal_append`. It does not get its own emergence rules, its own entity
kinds, or its own pane.

## Part 7 — the append-only AI-correction principle

The brief asks for this to be formalised as a DIRECTION decision, and it
should be, because it is the general model for any AI edit of the user's
content — transcription cleanup is only its first instance.

The shape: a capture and its raw transcript are records. An AI correction is a
**new record referencing the original**, carrying the corrected text, the model
id, the prompt version, and when it ran. The UI renders the corrected layer by
default with a "view original" toggle. The raw capture is never overwritten,
and "revert" means rendering a different layer, not deleting anything.

Two details that are not obvious and that determine whether this survives
contact with the existing code:

**Emergence runs over the corrected layer.** Raw ASR does not produce
`[task: ...]`; it produces "bracket task colon". Running the emergence parser
on raw output would materialise nothing, and running it on both would
double-materialise. The corrected layer is the one that means what the speaker
meant, so it is the one that emerges.

**Anchors bind to the layer they were computed against, never to "the current
text".** `anchors` is `(entry_id, start, end)` over UTF-16 offsets into a
specific body (`core/src/index/mod.rs:653`). A later, better correction is a
new layer with different offsets; if anchors floated to "latest", every
re-correction would silently corrupt every span binding in the entry.
Append-only makes this easy rather than hard — the old layer still exists, so
old anchors keep resolving against it — but only if the binding names its
layer explicitly. This is the detail that would be discovered painfully in
implementation if it were not decided here.

Scope boundary: this principle governs AI *corrections to the user's own
content*. AI-*authored* content (dreams, D26) is a different thing with an
existing answer — the actor model — and does not need a correction layer to
say who wrote it.

## What I could not verify

Stated explicitly, because several of these could change the design.

**Needs a spike, not an opinion:**

1. **Whether a usable SigLIP or CLIP ONNX export runs under `ort` 2.0.0-rc.12
   with acceptable CPU latency.** The Xenova org publishes ONNX exports and
   hive already depends on it for BGE, which is suggestive, not evidence. Both
   towers must export, and image preprocessing (resize, center-crop,
   normalize) has to be reimplemented Rust-side — hive has no image decoding
   today at all, so this drags in an image crate and a preprocessing parity
   problem against the Python reference.
2. **Retrieval quality of a small CLIP-family model on personal photographs.**
   Benchmarks are on curated datasets, not on a household's camera roll.
   Nobody should commit to a model without a measured pass over Nate's actual
   images.
3. **Speaker-embedding model availability as ONNX.** ECAPA-TDNN at 192 dims
   (SpeechBrain lineage) and the WeSpeaker ResNet family are the candidates;
   WeSpeaker publishes ONNX, SpeechBrain generally needs conversion. Unverified.
4. **Face pipeline shape.** Detection-plus-alignment then embedding is two
   models and more moving parts than voice (SCRFD or YuNet, then an ArcFace
   variant at 512 dims). Whether that is worth it before voice proves the
   pattern is a scoping question, not a technical one.
5. **Real threshold and margin values for any of these on Nate's own data.**
   Every number in Part 3 is a shape, not a value. EER figures from VoxCeleb do
   not transfer to a kitchen and a laptop microphone.
6. **Whisper runtime choice** — `ort` versus `whisper.cpp` bindings — including
   model size, latency on this hardware, and whether a second model-cache and a
   C dependency are acceptable.
7. **Whether `rebuild_ann` at open stays acceptable** once image vectors join
   it. It is already the startup cost that scales with corpus size, and PR 4.19
   is scheduled to swap in usearch/HNSW behind the `AnnIndex` trait. The
   multi-space work should land *after* 4.19 or the seam gets rebuilt twice.

**Needs a benchmark:**

8. **Embedding throughput for a photo library.** `Embedder` has no batch API —
   embedding is one-at-a-time under `spawn_blocking`
   (`embed/src/onnx.rs:436`). That is fine for journal entries arriving one at a
   time and possibly not fine for ten thousand photographs. Batching may become
   a prerequisite rather than an optimisation.
9. **The reciprocal-rank fusion's actual behaviour** against the golden fixture
   and against real mixed queries. The claim that text-only queries stay
   bit-identical needs to be a test, not a paragraph.

**Confirmed in code — needs a decision, not a spike:**

10. **The `blob_refs` / `DROP_DERIVED` interaction** — see Part 2. Verified
    end to end: the drop, the absent fold ownership, the insufficient
    `module.doc` payload, the pending query that will not re-fire, the
    importer using the same runtime path, and the absent test. The two
    sentences of claimed mitigation in `core/src/index/mod.rs:74` are stale
    and should be fixed in whichever change addresses this. It blocks this
    design and, independently, PR 4.11.

**Deliberately not designed here:**

11. Passive or ambient identification of anyone, at any time. Every capture in
    this design is initiated by a person.
12. Any use of these templates for authentication or access control. They are
    memory aids. Nothing in hive should ever unlock on a face.
