-- The hosted instance's Postgres schema, reborn as a test fixture (PR 1.7).
--
-- Recovered from git history: the SCHEMA / SCHEMA_SEARCH consts of
-- core/src/db.rs as of the PR 1.6 cutover's parent (git show 7eb158c^),
-- with the migrate()-era `ADD COLUMN IF NOT EXISTS` additions folded into
-- their CREATE TABLE statements (tasks.phase, tasks.due) — i.e. the FINAL
-- shape a real old instance has on disk. Hosted-era tables that PR 1.3
-- stopped creating (users, sessions, api_tokens, oauth_*, shares,
-- cc_sessions/cc_messages, runtime_oauth_states) are not part of the
-- reference schema; live old instances may still carry them as orphans and
-- the importer never reads them.
--
-- Runs against pgvector (the embeddings.vec_v column and its HNSW index);
-- the extension objects land in public so per-test schemas resolve them.

CREATE EXTENSION IF NOT EXISTS vector WITH SCHEMA public;

-- The journal is the source of truth: append-only, write-once prose.
CREATE TABLE IF NOT EXISTS journal (
  id         TEXT PRIMARY KEY,
  author     TEXT NOT NULL,
  body       TEXT NOT NULL,
  tags       TEXT NOT NULL DEFAULT '[]',
  mentions   TEXT NOT NULL DEFAULT '[]',
  -- Namespace owner the entry was written into (NULL = global/continuous
  -- history). Retained for storage-shape stability across the 1.6 cutover
  -- and the 1.7 importer; single-user reads no longer filter on it.
  user_scope TEXT,
  created_at TEXT NOT NULL
);

-- A span of a journal entry that produced a structured entity.
CREATE TABLE IF NOT EXISTS anchors (
  id         TEXT PRIMARY KEY,
  entry_id   TEXT NOT NULL,
  start      BIGINT NOT NULL,
  "end"      BIGINT NOT NULL,
  text       TEXT NOT NULL,
  kind       TEXT NOT NULL,
  ref_id     TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS anchors_entry ON anchors (entry_id);
CREATE INDEX IF NOT EXISTS anchors_ref ON anchors (ref_id);

CREATE TABLE IF NOT EXISTS projects (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL UNIQUE,
  slug       TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS topics (
  id         TEXT PRIMARY KEY,
  name       TEXT NOT NULL,
  slug       TEXT NOT NULL UNIQUE,
  created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS phases (
  id         TEXT PRIMARY KEY,
  project    TEXT NOT NULL,
  name       TEXT NOT NULL,
  position   BIGINT NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS phases_project ON phases (project);

-- phase/due were migrate()-era ADD COLUMNs; folded into the base here.
CREATE TABLE IF NOT EXISTS tasks (
  id              TEXT PRIMARY KEY,
  project         TEXT,
  phase           TEXT,
  due             TEXT,
  title           TEXT NOT NULL,
  body            TEXT NOT NULL DEFAULT '',
  status          TEXT NOT NULL DEFAULT 'todo',
  priority        TEXT NOT NULL DEFAULT 'normal',
  tags            TEXT NOT NULL DEFAULT '[]',
  assignees       TEXT NOT NULL DEFAULT '[]',
  origin_entry_id TEXT,
  anchor_text     TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS decisions (
  id              TEXT PRIMARY KEY,
  title           TEXT NOT NULL,
  context         TEXT NOT NULL DEFAULT '',
  decision        TEXT NOT NULL,
  consequences    TEXT NOT NULL DEFAULT '',
  status          TEXT NOT NULL DEFAULT 'proposed',
  tags            TEXT NOT NULL DEFAULT '[]',
  assignees       TEXT NOT NULL DEFAULT '[]',
  project         TEXT,
  supersedes      TEXT,
  origin_entry_id TEXT,
  anchor_text     TEXT,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
  id              TEXT PRIMARY KEY,
  title           TEXT NOT NULL,
  body            TEXT NOT NULL DEFAULT '',
  at              TEXT,
  tags            TEXT NOT NULL DEFAULT '[]',
  assignees       TEXT NOT NULL DEFAULT '[]',
  origin_entry_id TEXT,
  anchor_text     TEXT,
  created_at      TEXT NOT NULL
);

-- Per-actor inbox (humans + AIs). One row = one unread-able notification.
CREATE TABLE IF NOT EXISTS inbox (
  id         TEXT PRIMARY KEY,
  recipient  TEXT NOT NULL,
  "from"     TEXT NOT NULL,
  reason     TEXT NOT NULL,
  ref_kind   TEXT NOT NULL,
  ref_id     TEXT NOT NULL,
  entry_id   TEXT,
  snippet    TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL,
  read_at    TEXT
);
CREATE INDEX IF NOT EXISTS inbox_recipient ON inbox (recipient, read_at);

CREATE TABLE IF NOT EXISTS links (
  id          TEXT PRIMARY KEY,
  source_kind TEXT NOT NULL,
  source_id   TEXT NOT NULL,
  target_kind TEXT NOT NULL,
  target_id   TEXT NOT NULL,
  rel         TEXT NOT NULL DEFAULT 'relates',
  created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS wire (
  id         TEXT PRIMARY KEY,
  kind       TEXT NOT NULL,
  actor      TEXT NOT NULL DEFAULT 'system',
  payload    TEXT NOT NULL DEFAULT 'null',
  created_at TEXT NOT NULL
);

-- Worker config: external feeds the worker polls into wire events.
CREATE TABLE IF NOT EXISTS sources (
  id            TEXT PRIMARY KEY,
  name          TEXT NOT NULL,
  url           TEXT NOT NULL,
  kind          TEXT NOT NULL DEFAULT 'rss',
  category      TEXT,
  severity      TEXT NOT NULL DEFAULT 'info',
  interval_secs BIGINT NOT NULL DEFAULT 900,
  notify        TEXT,
  enabled       BOOLEAN NOT NULL DEFAULT TRUE,
  owner         TEXT,
  last_polled_at TEXT,
  last_status   TEXT,
  created_at    TEXT NOT NULL
);

-- Outbound work queue the worker drains (webhooks, digests, …).
CREATE TABLE IF NOT EXISTS outbox (
  id           TEXT PRIMARY KEY,
  kind         TEXT NOT NULL,
  payload      TEXT NOT NULL DEFAULT '{}',
  status       TEXT NOT NULL DEFAULT 'pending',
  attempts     BIGINT NOT NULL DEFAULT 0,
  last_error   TEXT,
  run_after    TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  completed_at TEXT
);
CREATE INDEX IF NOT EXISTS outbox_pending ON outbox (status, run_after);

-- Local embeddings for semantic search, one row per chunk. `hash` is the
-- ITEM-level content hash, identical on every chunk row. vec = packed
-- little-endian f32 bytes; vec_v = native pgvector column (384-dim BGE only)
-- behind the HNSW index. owner = namespace user the row is visible to.
CREATE TABLE IF NOT EXISTS embeddings (
  ref_kind   TEXT NOT NULL,
  ref_id     TEXT NOT NULL,
  chunk_idx  INT  NOT NULL DEFAULT 0,
  model      TEXT NOT NULL,
  dim        BIGINT NOT NULL,
  owner      TEXT,
  vec        BYTEA,
  vec_v      public.vector(384),
  hash       TEXT NOT NULL,
  created_at TEXT NOT NULL,
  PRIMARY KEY (ref_kind, ref_id, chunk_idx),
  CONSTRAINT embeddings_vec_present CHECK (vec IS NOT NULL OR vec_v IS NOT NULL)
);
CREATE INDEX IF NOT EXISTS embeddings_vec_hnsw ON embeddings
  USING hnsw (vec_v public.vector_cosine_ops) WITH (m = 16, ef_construction = 64);
CREATE INDEX IF NOT EXISTS embeddings_owner ON embeddings (owner);
CREATE INDEX IF NOT EXISTS embeddings_kind ON embeddings (ref_kind);

-- Single-row worker heartbeat / last-run stats, surfaced in the GUI.
CREATE TABLE IF NOT EXISTS worker_status (
  id         BIGINT PRIMARY KEY CHECK (id = 1),
  heartbeat  TEXT,
  last_run   TEXT
);

-- Writers: every human and AI that can author journal entries.
CREATE TABLE IF NOT EXISTS people (
  id         TEXT PRIMARY KEY,
  slug       TEXT NOT NULL UNIQUE,
  name       TEXT NOT NULL,
  kind       TEXT NOT NULL DEFAULT 'human',
  owner      TEXT,
  bio        TEXT,
  role       TEXT,
  created_at TEXT NOT NULL
);

-- Key/value instance config.
CREATE TABLE IF NOT EXISTS config (
  key        TEXT PRIMARY KEY,
  value      TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- Mutable per-actor card (humans + AIs).
CREATE TABLE IF NOT EXISTS profile (
  actor        TEXT PRIMARY KEY,
  kind         TEXT NOT NULL DEFAULT 'human',
  display_name TEXT NOT NULL DEFAULT '',
  body         TEXT NOT NULL DEFAULT '{}',
  source       TEXT NOT NULL DEFAULT 'manual',
  derived_at   TEXT,
  updated_at   TEXT NOT NULL
);

-- Cross-platform identity mapping: Discord/Telegram/Slack user ids →
-- a people.slug.
CREATE TABLE IF NOT EXISTS identities (
  id          TEXT PRIMARY KEY,
  platform    TEXT NOT NULL,
  platform_id TEXT NOT NULL,
  actor       TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  UNIQUE (platform, platform_id)
);

-- Claude Code artifacts (skills / agents / slash-commands) per AI identity.
CREATE TABLE IF NOT EXISTS identity_artifacts (
  id          TEXT PRIMARY KEY,
  actor       TEXT NOT NULL,
  kind        TEXT NOT NULL,
  name        TEXT NOT NULL,
  content     TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  enabled     BOOLEAN NOT NULL DEFAULT TRUE,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL,
  UNIQUE (actor, kind, name)
);
CREATE INDEX IF NOT EXISTS identity_artifacts_actor ON identity_artifacts (actor);

-- The credential vault, encrypted at rest (AES-256-GCM via HIVE_CRED_KEY).
-- Named cc_credentials for hosted-era reasons; its ONLY consumer was mail
-- account credentials (mail_accounts.cred_id). NOT migrated by the 1.7
-- importer — Phase 3 re-enters credentials against the OS keychain.
CREATE TABLE IF NOT EXISTS cc_credentials (
  id           TEXT PRIMARY KEY,
  owner        TEXT NOT NULL,
  kind         TEXT NOT NULL,
  runtime      TEXT NOT NULL DEFAULT 'claude_code',
  provider     TEXT,
  label        TEXT NOT NULL DEFAULT '',
  ciphertext   TEXT NOT NULL,
  nonce        TEXT NOT NULL,
  tail         TEXT NOT NULL DEFAULT '',
  created_at   TEXT NOT NULL,
  last_used_at TEXT
);
CREATE INDEX IF NOT EXISTS cc_credentials_owner ON cc_credentials (owner);

-- Unified full-text index: a regular table with a generated tsvector column
-- (title + body, english config) and a GIN index.
CREATE TABLE IF NOT EXISTS search (
  kind   TEXT NOT NULL,
  ref_id TEXT NOT NULL,
  title  TEXT NOT NULL DEFAULT '',
  body   TEXT NOT NULL DEFAULT '',
  tsv    tsvector GENERATED ALWAYS AS (
           to_tsvector('english', coalesce(title, '') || ' ' || coalesce(body, ''))
         ) STORED,
  PRIMARY KEY (kind, ref_id)
);
CREATE INDEX IF NOT EXISTS search_tsv ON search USING GIN (tsv);

-- ===== User-defined custom entity types =====
-- Registry (entity_types + entity_fields) + validated JSONB instances
-- (entities). Kind at the seams (search.kind, links.*_kind) is
-- entity_types.slug; instance ids are uniformly 'ent_'.
CREATE TABLE IF NOT EXISTS entity_types (
  id          TEXT PRIMARY KEY,
  slug        TEXT NOT NULL UNIQUE,
  name        TEXT NOT NULL,
  name_plural TEXT NOT NULL DEFAULT '',
  description TEXT NOT NULL DEFAULT '',
  icon        TEXT NOT NULL DEFAULT '',
  color       TEXT NOT NULL DEFAULT '',
  -- slug of a choice field the generic board groups by; NULL = flat list.
  board_field TEXT,
  archived    BOOLEAN NOT NULL DEFAULT FALSE,
  created_by  TEXT NOT NULL,
  created_at  TEXT NOT NULL,
  updated_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS entity_fields (
  id         TEXT PRIMARY KEY,
  type_id    TEXT NOT NULL,
  slug       TEXT NOT NULL,
  label      TEXT NOT NULL,
  -- text | number | bool | date | choice | ref (validated in Rust only).
  field_type TEXT NOT NULL,
  required   BOOLEAN NOT NULL DEFAULT FALSE,
  position   BIGINT NOT NULL DEFAULT 0,
  options    TEXT NOT NULL DEFAULT '[]',
  -- ref fields: target kind — person|topic|project|task or a custom slug.
  ref_kind   TEXT,
  archived   BOOLEAN NOT NULL DEFAULT FALSE,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (type_id, slug)
);
CREATE INDEX IF NOT EXISTS entity_fields_type ON entity_fields (type_id, position);

CREATE TABLE IF NOT EXISTS entities (
  id              TEXT PRIMARY KEY,
  type_id         TEXT NOT NULL,
  title           TEXT NOT NULL,
  fields          JSONB NOT NULL DEFAULT '{}'::jsonb,
  -- Same model as journal.user_scope (NULL = global).
  user_scope      TEXT,
  -- v2 journal-emergence provenance; carried so nothing blocks it.
  origin_entry_id TEXT,
  created_by      TEXT NOT NULL,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS entities_type  ON entities (type_id, created_at);
CREATE INDEX IF NOT EXISTS entities_scope ON entities (user_scope);
CREATE INDEX IF NOT EXISTS entities_fields_gin ON entities USING GIN (fields jsonb_path_ops);

-- Phase 1 mail archive. Sync state (JMAP state strings, backfill cursor,
-- backoff bookkeeping) lives on mail_accounts, and the account credential is
-- a cc_credentials row named by cred_id. user_scope is NEVER NULL for mail.
CREATE TABLE IF NOT EXISTS blobs (
  hash       TEXT PRIMARY KEY,
  size       BIGINT NOT NULL DEFAULT 0,
  mime       TEXT NOT NULL DEFAULT 'application/octet-stream',
  data       BYTEA,
  created_at TEXT NOT NULL
);
-- Attachment bytes arrive pre-compressed: skip TOAST compression.
ALTER TABLE blobs ALTER COLUMN data SET STORAGE EXTERNAL;

CREATE TABLE IF NOT EXISTS mail_accounts (
  id              TEXT PRIMARY KEY,
  owner           TEXT NOT NULL,
  address         TEXT NOT NULL,
  jmap_url        TEXT NOT NULL DEFAULT '',
  jmap_username   TEXT,
  jmap_account_id TEXT NOT NULL DEFAULT '',
  cred_id         TEXT,
  email_state     TEXT,
  mailbox_state   TEXT,
  backfill_status TEXT NOT NULL DEFAULT 'pending',
  backfill_cursor JSONB,
  attempts        BIGINT NOT NULL DEFAULT 0,
  next_attempt_at TEXT,
  last_error      TEXT,
  last_synced_at  TEXT,
  last_status     TEXT,
  enabled         BOOLEAN NOT NULL DEFAULT TRUE,
  created_at      TEXT NOT NULL,
  updated_at      TEXT NOT NULL,
  UNIQUE (owner, address)
);
CREATE INDEX IF NOT EXISTS mail_accounts_owner ON mail_accounts (owner, address);

CREATE TABLE IF NOT EXISTS mail_mailboxes (
  id         TEXT PRIMARY KEY,
  account_id TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
  jmap_id    TEXT NOT NULL,
  name       TEXT NOT NULL,
  role       TEXT,
  ingest     BOOLEAN NOT NULL DEFAULT FALSE,
  sort_order BIGINT NOT NULL DEFAULT 0,
  UNIQUE (account_id, jmap_id)
);
CREATE INDEX IF NOT EXISTS mail_mailboxes_account ON mail_mailboxes (account_id, sort_order);

CREATE TABLE IF NOT EXISTS mail_messages (
  id               TEXT PRIMARY KEY,
  account_id       TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
  jmap_id          TEXT NOT NULL,
  jmap_thread_id   TEXT NOT NULL,
  message_id_hdr   TEXT,
  in_reply_to      TEXT,
  references_json  TEXT NOT NULL DEFAULT '[]',
  from_addr        TEXT NOT NULL DEFAULT '',
  from_name        TEXT,
  to_json          TEXT NOT NULL DEFAULT '[]',
  cc_json          TEXT NOT NULL DEFAULT '[]',
  reply_to_json    TEXT NOT NULL DEFAULT '[]',
  subject          TEXT NOT NULL DEFAULT '',
  sent_at          TEXT,
  received_at      TEXT NOT NULL,
  mailbox_ids_json TEXT NOT NULL DEFAULT '[]',
  keywords_json    TEXT NOT NULL DEFAULT '{}',
  body_text        TEXT NOT NULL DEFAULT '',
  body_source      TEXT NOT NULL DEFAULT 'plain',
  snippet          TEXT NOT NULL DEFAULT '',
  size             BIGINT NOT NULL DEFAULT 0,
  has_attachments  BOOLEAN NOT NULL DEFAULT FALSE,
  embed_state      TEXT NOT NULL DEFAULT 'pending',
  user_scope       TEXT NOT NULL,
  deleted_at       TEXT,
  created_at       TEXT NOT NULL,
  updated_at       TEXT NOT NULL,
  UNIQUE (account_id, jmap_id)
);
CREATE INDEX IF NOT EXISTS mail_messages_scope_received ON mail_messages (user_scope, received_at DESC);
CREATE INDEX IF NOT EXISTS mail_messages_account_thread ON mail_messages (account_id, jmap_thread_id);
CREATE INDEX IF NOT EXISTS mail_messages_message_id ON mail_messages (message_id_hdr);
CREATE INDEX IF NOT EXISTS mail_messages_subject ON mail_messages (subject);
CREATE INDEX IF NOT EXISTS mail_messages_embed_pending
  ON mail_messages (account_id, received_at DESC)
  WHERE embed_state = 'pending' AND deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS mail_attachments (
  id             TEXT PRIMARY KEY,
  message_id     TEXT NOT NULL REFERENCES mail_messages(id) ON DELETE CASCADE,
  blob_hash      TEXT REFERENCES blobs(hash) ON DELETE SET NULL,
  jmap_blob_id   TEXT NOT NULL DEFAULT '',
  filename       TEXT NOT NULL DEFAULT '',
  mime           TEXT NOT NULL DEFAULT 'application/octet-stream',
  size           BIGINT NOT NULL DEFAULT 0,
  content_id     TEXT,
  disposition    TEXT,
  skipped_reason TEXT,
  created_at     TEXT NOT NULL,
  UNIQUE NULLS NOT DISTINCT (message_id, jmap_blob_id, content_id)
);
CREATE INDEX IF NOT EXISTS mail_attachments_message ON mail_attachments (message_id);
CREATE INDEX IF NOT EXISTS mail_attachments_blob ON mail_attachments (blob_hash);
