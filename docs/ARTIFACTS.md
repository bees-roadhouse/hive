# Artifacts: naming, types, ingest, and derived text

Design note, 2026-08-10. Three decisions taken (see *Decided*, at the end);
the rest is for reaction.

## What this does NOT cover

The multimodal retrieval design is already written and this note does not
restate or contest it:

- **D37** — embedding spaces are plural and named, prose stays on BGE, images
  get their own CLIP-family space and their own `ref_kind`, a text query
  reaches them through that model's text tower, and cross-space results fuse by
  **reciprocal rank, not score**.
- **D38** — derived biometric templates are blobs, forced there by crypto-shred.
- **D39** — an identification is a proposal, closed-set, never written unconfirmed.
- **D40** — an AI correction is an appended layer; the capture is never modified.
- `docs/MULTIMODAL-IDENTITY.md` — the full argument behind all four.

One thing worth recording: the note's stated prerequisite is now **cleared**.
The `blob_refs` hazard ("it blocks the design") was fixed by PR #135, which is
the tip of `main`. `blob_refs` no longer rides `DROP_DERIVED`, so a
`FOLD_VERSION` bump no longer crypto-shreds every stored attachment.

What follows is only what those decisions do not reach: what the blob layer is
CALLED, what happens to bytes on the way in, and where derived text lives.

## Part 1 — the name

Today the blob layer speaks `BlobRef`, `blockstore`, `blocks/`, `blob_refs`,
and `mail_attachments`. That vocabulary describes storage mechanics, not the
thing a person has. A scanned invoice is not a blob to its owner.

**Blobs become artifacts.**

The collision is real but narrow. `artifacts` today means Claude Code
skills, agents, and slash-commands stored per AI identity: `store/artifacts.rs`,
the `identity_artifacts` table, `IdentityArtifact` in shared, an
`identity_artifact` fold record kind, and five MCP tools.

The storage is *already* namespaced — table `identity_artifacts`, type
`IdentityArtifact`. Only four of the five tools dropped the prefix:

| today | becomes |
|---|---|
| `artifacts_list` | `identity_artifacts_list` |
| `artifacts_get` | `identity_artifacts_get` |
| `artifacts_upsert` | `identity_artifacts_upsert` |
| `artifacts_remove` | `identity_artifacts_remove` |
| `identity_artifacts_sync` | unchanged, already correct |

That frees the bare word and makes the existing surface more consistent rather
than less. It is a rename of four schema literals and four match arms.

`BlobRef` and the blockstore keep their names. They are the mechanism under an
artifact, not the artifact, and the crypto-shred property lives there: the
wrapped content key in `BlobRef` IS the artifact's life, so destroying it makes
the bytes unreachable. That is already the delete story.

## Part 2 — types, and what each one triggers

The blockstore keys on `(master key, bytes, mime)`, so a type discriminator
already exists. What is new is dispatch on it.

| mime | on ingest |
|---|---|
| `image/*` | EXIF extract, CLIP-space embed (D37), caption (see Part 4) |
| `application/pdf`, scans | OCR to derived text |
| `audio/*` | speech-to-text to derived text |
| everything else | stored, indexed by filename and context only |

Unknown types must be storable. An artifact whose type has no pipeline is a
normal artifact that simply has nothing derived from it, never a rejected one.

## Part 2a — bytes stream, references sync (D43)

An artifact's BYTES are not replicated eagerly. The log record carries a
`BlobRef` — the reference plus the wrapped content key — never the payload, so
a client can know an artifact exists, its mime, its size, and its manifest hash
while holding none of it. That separation is already in the frozen format; what
this note adds is the policy on top.

| tier | contents |
|---|---|
| eager | op-log, index, embeddings, derived text |
| eager | thumbnails and small renditions |
| on demand | original bytes, streamed and LRU-cached to a user-set ceiling |

**A thumbnail is a derived artifact like any other**, produced by the same
pipeline as a caption or an OCR pass, indexed nowhere, and crypto-shredded with
its parent. It is in the eager tier for one reason: without it a photo grid is
empty on a plane, and a photo product whose grid is empty offline is not a
photo product.

Streaming works through a blind cache because content addressing is over
CIPHERTEXT — a cache serves blocks it cannot read, and the client verifies them
by hash. Two households holding the same photo produce different ciphertext,
because the content key is a PRF of the plaintext hash under a master-derived
subkey, so nothing about who holds what leaks to whoever runs the cache.

The fetch chain is main device, then blind cache if the user enabled one, then
unavailable and say so. Caching everything on every device is not a cache; for
a household's whole photographic history it is a copy.

## Part 3 — derived text is one kind with several producers

OCR, speech-to-text, and image captioning are the same shape: **text derived
from an artifact by a model.** They should be one record kind, linked to the
artifact, not three features.

Reasons, in order of weight:

1. **It is indexed and embedded for free.** Derived text is text of a known
   kind, so `search_fts`, the `embeddings` table, and `backfill_embeddings`
   pick it up with no new machinery. Scanning a receipt makes it findable three
   months later by half-remembering it. That is the entire payoff and it
   already exists.
2. **It is re-derivable.** A better model next year re-derives, and the
   `model`-per-row stamping that already lets an embedder swap re-backfill only
   mismatched rows applies unchanged.
3. **It carries its own metadata** — model, language, confidence, when — which
   a field on the artifact cannot.
4. **D40 already governs it.** A derived transcription is an AI-authored layer
   over a capture that is never modified. Corrections append.

The producer should be a backend seam shaped like `embed/`'s
`Backend { Native, Ollama, Hash }`, for the same reason: a swap re-derives only
what is stale, and CI stays offline and deterministic through the hash tier.

## Part 4 — captions AND CLIP, which D37 defers

D37 names local-caption-to-BGE as a real third option, deferred rather than
rejected, and says plainly: **"Ship at most one of the two at a time."**

The ask is for both, and the argument for both is that they fail differently.
A caption is prose in the text space, so it rides the existing retrieval and is
*readable* — you can see why a photo matched. CLIP is a separate space that
knows visual concepts a caption never bothers to state, and it is the only one
of the two that can do image-to-image at all.

D37's sequencing caution still holds and is honoured: build one, measure it on
a real corpus, then build the other.

**Decided 2026-08-10: captions first.** They are cheaper, they reuse the text
space and the existing retrieval end to end, and they produce something
readable rather than a number. CLIP follows once captions have been measured
on a real corpus, and it is the half that adds image-to-image.

## Part 4a — provenance, and why it is not a review queue

**Decided 2026-08-10: derived text is never presented as though a human wrote
it. It is authored by the model.**

The failure being avoided is specific: a wrong OCR or a confidently wrong
caption is a *searchable lie*. It does not announce itself, and the corpus
treats it exactly like prose its owner wrote.

The tempting fix is a confidence-gated review queue. It does not survive
contact with the producers:

| producer | honest confidence? |
|---|---|
| OCR (Tesseract) | yes, per word |
| speech-to-text (Whisper) | yes, log-probs |
| **VLM caption** | **no** ... equally fluent when wrong |

So the producer chosen to ship first is precisely the one with nothing to gate
on. A confidence threshold would silently pass every bad caption.

The mechanism that does work already exists and is load-bearing in the product:
**authorship**. Journal entries carry `author`; `people` carries
`kind: human | ai`; AI identities are first-class and appear in the roster. A
derived transcription authored by its model is AI-authored in exactly the way
one of Pia's journal entries is, through the machinery that already renders
that distinction everywhere.

That gives four properties for no new concepts:

1. **Nothing machine-written ever looks human-written.** True by construction,
   not by remembering to set a flag.
2. **Search results carry it.** A hit on derived text shows its author, so a
   bad OCR match is visibly a guess rather than something you wrote. This is
   the cheapest available fix for the searchable-lie problem.
3. **Correction is already specified.** D40 says an AI correction is an
   appended layer and the capture is never modified. A human edit appends,
   authored by the human, and the original derivation is retained.
4. **Filtering is free.** The journal's existing All/mine writer filter already
   separates human from AI authorship.

Confidence still earns its keep where it is real: OCR and speech-to-text below
threshold queue into the existing `inbox` table for a look. Captions do not
queue, because a caption's confidence is not information. They are simply,
visibly, machine-authored.

One consequence to accept deliberately: the producing models become AI
identities and appear in the identities roster beside Pia and Apis. That is
the honest data model ... the thing that wrote the text is not a human ... but
it does mean the roster grows a row per producer, and the UI should probably
group them apart from conversational identities.

## Part 5 — EXIF, which nothing currently reads

If photos are first-class rather than attachments, the organising spine is
date, place, and people. The blockstore knows `(bytes, mime)` and nothing else.

Ingest needs:

- **Capture timestamp.** Not file mtime. The date a photo belongs to is the
  date it was taken, and mtime is whenever it was copied.
- **GPS**, where present.
- **Camera and orientation**, cheap to keep and needed for correct display.

People-in-photos is D38/D39 territory and is deliberately not in scope here.

### Place names: both, decided 2026-08-10

Reverse geocoding is a network call, and `docs/THREAT-MODEL.md` enumerates this
product's outbound calls **exhaustively at one** ... the Postgres URL a user
types into the importer. It also states the principle that shapes this: zero
collection is "enforced in code and held by those gates, not by the sandbox,
which cannot tell a user-initiated import from a phone-home" (D27).

So online geocoding is a threat-model amendment carrying a code gate, not a
settings checkbox.

**Offline always runs, in `node-core`, no network.** GeoNames, bundled. The
tiers are `cities15000` at ~2 MB, `cities5000` at ~7 MB, `cities500` at ~30 MB,
`allCountries` at ~350 MB. Admin1/admin2 hierarchies are small and always
included, so country, state, and county are reliable. Settlements are not:
Loganton, PA has roughly 450 residents and falls below every tier short of the
full dump. Offline therefore answers "Clinton County, Pennsylvania" plus a
nearby town, and that is its honest ceiling.

**Online is an opt-in upgrade living ABOVE core**, in the Electron main
process, never inside the store layer. Three mechanics are part of the
decision, not implementation detail:

1. **Coordinates are rounded to a grid cell before they leave the machine.**
   Never the exact fix. The place name is identical and a third party does not
   receive where a family actually stood.
2. **Results cache by cell.** Every photo taken at home resolves one cell, so
   home is looked up once, ever ... not once per photo. Less leakage, fewer
   calls, and faster.
3. **A grep gate mirroring `importer/tests/no_postgres_gate.rs`**, so exactly
   one module may dial out and that is mechanically true rather than a promise.
   This is what honours D27: the guarantee cannot rest on the setting being
   off.

`THREAT-MODEL.md`'s exhaustive list grows by one entry, stated as plainly as
the Postgres one is.

Provider is unresolved. Nominatim's usage policy restricts volume and requires
a real User-Agent; Photon is OSM-based and more permissive; commercial
geocoders exist. Cell caching makes the volume tiny either way, but the policy
needs reading before a provider is picked rather than after.

## Part 6 — ScanSnap

The reliable integration is a **watched folder**. ScanSnap Home supports
scan-to-folder profiles, so a button press on the scanner drops a PDF where
hive is watching, and hive ingests it as an artifact.

That can feel first-class without an SDK: press the button, the scan appears,
the OCR arrives seconds later. A ScanSnap Cloud API exists; nobody here has
verified what it exposes, and it should not be built on until someone has.

The watched folder is also general. Any scanner, any phone-sync folder, any
`Save to` target feeds the same path.

## Open questions

1. **Direct or triaged?** Does a scan land in a journal entry immediately, or
   in an inbox to sort? Direct is fewer steps; an inbox handles scanning twelve
   things at once. The `inbox` table already exists.
2. **Provider for online reverse geocoding.** Nominatim, Photon, or commercial
   ... the usage policy needs reading before the pick, not after. See Part 5.


### Decided

- **Captions before CLIP** (2026-08-10). See Part 4.
- **Both offline and online place names** (2026-08-10). See Part 5.
- **Derived text is authored by its model, not reviewed into existence**
  (2026-08-10). See Part 4a. Confidence gates a queue only where confidence is
  real, which is OCR and speech-to-text and not captions.
