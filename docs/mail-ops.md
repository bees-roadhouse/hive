# Mail operations

How to turn on, connect, protect, and validate hive-mail: the daemon that
syncs JMAP accounts (Stalwart at `https://mail.beesroadhouse.com`) into
Hive's searchable mail archive. Everything here assumes the Rust compose
stack (`docker/docker-compose.rust.yml`).

## Enable mail on the box

Mail ships dark. The `hive-mail` service always runs but idles, and every
`/api/mail/*` route 404s, until the flag flips.

1. Generate the credential-vault key once:

   ```sh
   openssl rand -hex 32
   ```

2. Put both values in the compose environment (`.env` next to the compose
   file, or exported):

   ```sh
   HIVE_CRED_KEY=<the 64-hex-char output>
   HIVE_MAIL_ENABLED=1
   ```

3. Restart the stack:

   ```sh
   docker compose -f docker/docker-compose.rust.yml up -d
   ```

`HIVE_CRED_KEY` is hard-required by `hive-api` and `hive-mail` (compose
refuses to start without it). It is the AES-256-GCM key for the credential
vault, which holds mail account passwords and runtime sign-ins.

**Back the key up separately from the database.** A database backup does not
contain the key, and the key does not live anywhere else. Losing it orphans
every stored credential: mail accounts stop syncing and every runtime
sign-in dies until each secret is re-entered by hand.

## Backups now contain your mailbox

Once an account backfills, `mail_messages` holds full message bodies. **An
unencrypted `pg_dump` is now a copy of the mailbox.** Encrypt database
backups at rest (for example `pg_dump ... | age -r <recipient>` or an
encrypted backup target), and apply the same standard to volume snapshots of
`hive-pgdata`.

The one thing a plain dump does not contain is usable credentials: vault
rows are AES-GCM ciphertext, useless without `HIVE_CRED_KEY`. That is also
why the key must never be stored beside the dumps.

## Connect an account

Connecting is admin-only in v1 because the JMAP URL is a server-side fetch
target (SSRF surface). The API validates the credential with a real JMAP
session-discovery call before storing anything, so a typo'd password or URL
fails at the form, not silently in the daemon.

1. Open **Settings → Mail accounts** (the section appears once
   `HIVE_MAIL_ENABLED=1` reaches `hive-api`).
2. Fill the connect form:
   - **address**: the mailbox, e.g. `nate@bierlysmith.com`
   - **JMAP URL**: `https://mail.beesroadhouse.com`
   - **login**: only if the Stalwart login principal differs from the address
   - **app password**: the account password (a dedicated app password if the
     account has one)
3. Connect. The account row appears with backfill status `pending`.
4. Tick the mailboxes to ingest (the per-mailbox checkboxes under the
   account row; they arrive after hive-mail's first mailbox sync, within
   `HIVE_MAIL_TICK` seconds). **Nothing is ingested until a mailbox is
   opted in.** This is the spam gate: leave Junk unticked.
5. Backfill starts on the next tick and pages newest-first. Progress events
   land on the wire every 50 pages; the row flips to `complete` when done.

Sync failures back off exponentially and surface on the account row
(`last_error`). After 8 consecutive failures the account disables itself
and notifies its owner; fix the cause, then hit resume (the ⏸/▶ toggle).

## Stalwart version pinning

Stalwart invalidates JMAP state strings after upgrades and store
compactions. That is expected: the daemon answers `cannotCalculateChanges`
with a full reconciliation (a paged, ids-only diff that never re-fetches
messages it already holds), so mail keeps flowing. You may notice one
slightly slower cycle after a Stalwart upgrade, nothing more. If an account
ever looks wedged or inconsistent, **Settings → Mail accounts → ⟳** forces
that same reconciliation on the next cycle.

Two pins to maintain when upgrading the mail server:

- CI runs the sync e2e against `stalwartlabs/stalwart:v0.15.5`
  (`.github/workflows/ci.yml`, config in `ci/stalwart/`). Keep that tag
  tracking the version deployed at `mail.beesroadhouse.com`, so CI exercises
  the server behavior production actually has.
- v0.16.0 moved Stalwart's configuration from a config file into a
  database-backed registry provisioned via the admin API. When the box
  moves past v0.15, the CI harness (`ci/stalwart/config.toml` +
  `provision.sh`) needs a rework, not just a tag bump.

## Environment reference

| Env | Default | Used by | What it does |
| --- | --- | --- | --- |
| `HIVE_MAIL_ENABLED` | `0` | api, hive-mail | Master switch: 404s the mail routes and idles the daemon when off |
| `HIVE_CRED_KEY` | required | api, hive-mail | AES-256-GCM credential-vault key. Generate once, back up separately |
| `HIVE_MAIL_TICK` | `15` | hive-mail | Seconds between scans for due accounts |
| `HIVE_MAIL_POLL_SECS` | `300` | hive-mail | Doorbell wait per cycle; the poll cadence when the EventSource stream is down |
| `HIVE_MAIL_MAX_BODY_BYTES` | `262144` | hive-mail | Per-message bodyValues cap requested from the server |
| `HIVE_MAIL_PAGE_SIZE` | `200` | hive-mail | Backfill/delta page size. Mostly a test knob (the CI e2e sets 10) |
| `HIVE_MAIL_MAX_ATTACHMENT_BYTES` | `15728640` | hive-mail | Reserved: attachment byte cap, lands with the attachment pipeline |
| `HIVE_MAIL_EMBED_LIMIT` | `5000` | worker | Reserved: mail embedding gate, lifted by the retrieval workstream |
| `HIVE_TEST_STALWART_URL` / `_USER` / `_PASS` | unset | CI | Points `mail/tests/stalwart_e2e.rs` at a live Stalwart; the test self-skips without the URL |

## Real-mailbox validation checklist

Run once against `mail.beesroadhouse.com` after enabling mail on the box
(and record the numbers in the journal):

- [ ] Connect the real account; session discovery accepts the credential.
- [ ] Opt the Inbox (and chosen archives) into ingest; leave Junk out.
- [ ] **Backfill wall-clock**: note start (first `mail.backfill.progress`
      wire event) to `mail.backfill.completed`, and the message count.
- [ ] **Wire stays quiet during backfill**: the wire shows progress events
      (one per 50 pages), not per-message `mail.received` spam, and no
      inbox notification flood.
- [ ] Search a known phrase from old mail; the message surfaces next to
      journal results.
- [ ] **Delete-in-Stalwart propagates**: delete a test message in Stalwart
      (webmail/IMAP); within one poll interval (`HIVE_MAIL_POLL_SECS`,
      default 5 min) it is tombstoned here and gone from search.
- [ ] **Ingest-toggle drops FTS rows**: untick a mailbox; its messages
      leave search immediately (rows stay, D6 semantics). Re-tick; backfill
      re-arms and re-indexes with zero duplicates.
- [ ] Force resync (⟳) on the account; counts stay identical afterwards.
- [ ] Record dashboard search/blob totals and a few search latencies.

Next steps after validation: record the measurements in the journal, then
lift the mail embedding gate per the retrieval workstream plan
(`HIVE_MAIL_EMBED_LIMIT`).
