# Threat model

Status: ships with Phase 1 (DIRECTION.md D27). Single user, single machine —
sync (Phase 4) will extend this with the relay-observability section D21
promises. Engineering statement, not marketing; when the code and this
document disagree, fix whichever is wrong in the same change.

## Assets

Everything lives under one data dir (`$XDG_DATA_HOME/hive`):

- **Op log** — `log/<device>/*` segment files: every record ever written
  (journal prose, entities, mail metadata, config). The source of truth.
- **Derived index** — `index.db`: SQLCipher SQLite projection of the log
  (current entities, FTS5, vectors). Rebuildable by replay; contains the
  same content as the log, differently shaped.
- **Blobs** — `blocks/`: payload bytes (mail attachments today; file/page
  captures later) as encrypted content-addressed blocks.
- **Master key** — 32 bytes in the OS keychain (service `hive`, entry
  `master-key`). The root of every other key.
- **Module credentials** — mail passwords in the `cc_credentials` vault,
  AES-256-GCM under `HIVE_CRED_KEY` (legacy; moves to keychain wrapping in
  Phase 3 per D27).

## Adversaries considered

- **Device thief / disk-at-rest reader** — steals the machine or a backup
  of the data dir, without your OS login.
- **Cloud or relay observer** — whoever can read bytes you place off-box.
  Today hive places nothing off-box; from Phase 4 a relay sees ciphertext,
  node ids, and IPs (to be restated concretely in that phase).
- **Malicious ingestion module** (future, Phase 3) — WASM modules run
  sandboxed with capability grants; not load-bearing yet because modules
  don't exist yet.
- **Curious houseguest** — someone at your unlocked machine, or another
  (non-root) user account on it.
- **Whoever your AI talks to** — Anthropic sees whatever your Claude
  sessions read through MCP when you use them. The bridge doesn't change
  what a tool returns; it changes who can ask (local processes only).

Out of scope: malware running as you, root on your live machine, hardware
attacks on RAM. Nothing here defends a compromised running session.

## What encryption covers at rest

- Op-log segments: per-frame XChaCha20-Poly1305 under per-segment keys
  wrapped by the master key. Record contents are unreadable without it.
- Blobs: per-blob content keys (a keyed PRF of master + plaintext hash —
  convergent within your key domain, so dedup works but outsiders can't
  confirm known files). Blocks are ciphertext addressed by their own hash.
- Index: SQLCipher over the whole database file; its key is derived from
  the master key (compromise of master = compromise of everything, by
  design — one root, one recovery story).
- NOT hidden: file sizes and counts, block/segment layout, the device id
  file, timestamps on files. Traffic analysis of the data dir reveals that
  you use hive and roughly how much, not what's in it.

## What the keychain protects

The master key never touches the data dir. A stolen disk or copied data dir
is noise without the keychain entry, which the OS releases only to your
logged-in session. It does NOT protect against anyone already inside that
session (they can read the key the same way the app does). Losing the
keychain entry loses everything — the passphrase export wrap and printed
recovery code exist in core (`keys.rs`) for exactly this, but there is no UI
for them yet (gap, below).

## What crypto-shred guarantees — and does not

Hard delete (D19) destroys every stored copy of a blob's wrapped content key
and deletes its blocks, then appends a tombstone; replaying the log
reproduces the deletion, never the content. After a shred:

- the blob's bytes are unrecoverable from the data dir, even to someone
  holding the master key (no wrapped key survives — verified end to end in
  `core/tests/crypto_shred.rs`);
- FTS and vector rows for the item are dropped, so retrieval can't
  resurface fragments;
- the tombstone propagates through future sync (Phase 4), so peers converge
  on the deletion.

It does NOT reach:

- **RAM** — plaintext read before the shred may sit in process memory or OS
  caches until reboot;
- **swap** — if the OS paged that memory out, remnants can persist on disk
  outside hive's control;
- **pre-shred backups** — a copy of the data dir taken before the shred
  still contains the wrapped key and blocks; shredding now does not reach
  backups you made then;
- **filesystem forensics** — deleted block files and SQLite free pages are
  overwritten lazily; remnants matter only to an attacker who also holds
  the master key, but state it: physical erasure is not guaranteed.

## Telemetry

Zero. No analytics, no crash reporting, no update pings, no network calls at
all from the engine, the app, or the bridge (the grep gate keeps HTTP
clients out of the bridge). Embeddings are computed locally. When update
checks arrive (Phase 2.5) they are identifier-free against a static
manifest.

## Current gaps, stated plainly

- **No passphrase re-wrap or recovery-code UI yet.** The primitives exist;
  until the UI lands, the OS keychain is the only custody and key loss is
  unrecoverable. Print your recovery code by hand if you care today.
- **Bridge/app mutual exclusion is an advisory flock** on
  `<data_dir>/lock`. It reliably fences cooperating hive processes; it is
  not a security boundary — any process that can read your data dir can
  already read your keychain-released key material at runtime.
- **SQLCipher key derivation from master**: index encryption is only as
  strong as master-key custody; there is no separate index passphrase.
- **The bridge trusts the local user.** Anything running as you can invoke
  it and read your store — the OS user boundary is the auth (D16: single
  user). Stored content returned by MCP tools is untrusted input to the
  calling agent (prompt injection through remembered data is real; the
  mail-handling rules in the plugin skill exist for this).
- **`HIVE_CRED_KEY` env-var custody** for mail credentials until Phase 3
  moves them under the keychain.
