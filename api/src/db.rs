// PostgreSQL is the datastore. The api and worker both connect to the same
// Postgres (via DATABASE_URL), which is what lets them share state without the
// single-writer / file-ownership friction of the old shared-SQLite-file setup.
//
// Schema is created idempotently at boot (CREATE TABLE IF NOT EXISTS + ADD
// COLUMN IF NOT EXISTS), so both binaries can race the migrate path safely.
// Full-text search uses a generated `tsvector` column + GIN index in place of
// SQLite's FTS5 virtual table. Existing SQLite data migrates in via the import
// endpoint (api/src/legacy_import.rs reads the uploaded .db; this store writes
// it through the normal insert path).

use anyhow::Result;
use hive_shared::APP_VERSION;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;

use crate::auth::now_iso;

/// Resolve the Postgres connection string from `DATABASE_URL`. Falls back to a
/// local dev instance so tests + local runs work without extra config.
pub fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://hive:hive@localhost:5432/hive".to_string())
}

pub async fn open(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(url)
        .await?;
    Ok(pool)
}

const SCHEMA: &str = r#"
    -- The journal is the source of truth: append-only, write-once prose.
    CREATE TABLE IF NOT EXISTS journal (
      id         TEXT PRIMARY KEY,
      author     TEXT NOT NULL,
      body       TEXT NOT NULL,
      tags       TEXT NOT NULL DEFAULT '[]',
      mentions   TEXT NOT NULL DEFAULT '[]',
      -- Namespace owner: the human user this entry belongs to. NULL = global /
      -- "continuous" history visible to everyone. Non-NULL = visible only to
      -- that user (+ admins). See the Visibility model in middleware.rs.
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

    CREATE TABLE IF NOT EXISTS tasks (
      id              TEXT PRIMARY KEY,
      project         TEXT,
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

    -- Local embeddings for semantic search (vec = packed little-endian f32 bytes).
    CREATE TABLE IF NOT EXISTS embeddings (
      ref_kind   TEXT NOT NULL,
      ref_id     TEXT NOT NULL,
      model      TEXT NOT NULL,
      dim        BIGINT NOT NULL,
      vec        BYTEA NOT NULL,
      hash       TEXT NOT NULL,
      created_at TEXT NOT NULL,
      PRIMARY KEY (ref_kind, ref_id)
    );

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

    -- Shares: explicit visibility grants.
    CREATE TABLE IF NOT EXISTS shares (
      id         TEXT PRIMARY KEY,
      scope      TEXT NOT NULL,
      ref        TEXT NOT NULL,
      viewer     TEXT NOT NULL,
      created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS shares_viewer ON shares (viewer, scope);
    CREATE UNIQUE INDEX IF NOT EXISTS shares_uniq ON shares (scope, ref, viewer);

    -- Key/value instance config.
    CREATE TABLE IF NOT EXISTS config (
      key        TEXT PRIMARY KEY,
      value      TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );

    -- Login accounts. actor is the people.slug this user authenticates as.
    CREATE TABLE IF NOT EXISTS users (
      id            TEXT PRIMARY KEY,
      actor         TEXT NOT NULL UNIQUE,
      email         TEXT NOT NULL UNIQUE,
      name          TEXT NOT NULL,
      role          TEXT NOT NULL DEFAULT 'member',
      password_hash TEXT NOT NULL,
      created_at    TEXT NOT NULL,
      last_login_at TEXT
    );

    -- Browser sessions (cookie auth). token_hash = sha256(plaintext cookie value).
    CREATE TABLE IF NOT EXISTS sessions (
      id         TEXT PRIMARY KEY,
      token_hash TEXT NOT NULL UNIQUE,
      user_id    TEXT NOT NULL,
      created_at TEXT NOT NULL,
      expires_at TEXT NOT NULL,
      last_seen  TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS sessions_user ON sessions (user_id);

    -- Bearer tokens for programmatic clients (CLI, MCP, AI agents).
    CREATE TABLE IF NOT EXISTS api_tokens (
      id           TEXT PRIMARY KEY,
      token_hash   TEXT NOT NULL UNIQUE,
      actor        TEXT NOT NULL,
      label        TEXT NOT NULL,
      created_by   TEXT NOT NULL,
      created_at   TEXT NOT NULL,
      last_used_at TEXT,
      kind         TEXT,
      client_id    TEXT,
      granted_by   TEXT,
      expires_at   TEXT,
      scope        TEXT
    );

    -- OAuth 2.1 dynamic client registration (RFC 7591).
    CREATE TABLE IF NOT EXISTS oauth_clients (
      client_id     TEXT PRIMARY KEY,
      client_name   TEXT NOT NULL,
      redirect_uris TEXT NOT NULL,
      grant_types   TEXT NOT NULL,
      created_at    TEXT NOT NULL
    );

    -- OAuth authorization codes: single-use, short TTL, hashed at rest.
    CREATE TABLE IF NOT EXISTS oauth_auth_codes (
      code_hash      TEXT PRIMARY KEY,
      client_id      TEXT NOT NULL,
      redirect_uri   TEXT NOT NULL,
      code_challenge TEXT NOT NULL,
      ai_actor       TEXT NOT NULL,
      granted_by     TEXT NOT NULL,
      scope          TEXT NOT NULL,
      created_at     TEXT NOT NULL,
      expires_at     TEXT NOT NULL,
      used_at        TEXT,
      -- Requested access-token lifetime (seconds) carried from consent; NULL = default.
      token_ttl_secs BIGINT
    );
    CREATE INDEX IF NOT EXISTS oauth_codes_expiry ON oauth_auth_codes (expires_at);

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

    -- ===== Hosted Claude Code workspaces (hive → Claude Code) =====
    -- One row per session hive spins up and drives in an isolated sandbox.
    -- Separate from the journal; scoped per owner (see Visibility in middleware.rs).
    CREATE TABLE IF NOT EXISTS cc_sessions (
      id                TEXT PRIMARY KEY,
      owner             TEXT NOT NULL,
      created_by        TEXT NOT NULL,
      title             TEXT NOT NULL DEFAULT '',
      workdir           TEXT NOT NULL DEFAULT '',
      claude_session_id TEXT,
      status            TEXT NOT NULL DEFAULT 'provisioning',
      model             TEXT,
      usage             TEXT NOT NULL DEFAULT '{}',
      meta              TEXT NOT NULL DEFAULT '{}',
      repo_url          TEXT,
      repo_ref          TEXT,
      created_at        TEXT NOT NULL,
      updated_at        TEXT NOT NULL,
      last_activity_at  TEXT
    );
    CREATE INDEX IF NOT EXISTS cc_sessions_owner ON cc_sessions (owner);

    -- Complete chat history per session: every Agent-SDK message, lossless.
    CREATE TABLE IF NOT EXISTS cc_messages (
      id          TEXT PRIMARY KEY,
      session_id  TEXT NOT NULL,
      seq         BIGINT NOT NULL,
      role        TEXT NOT NULL,
      kind        TEXT NOT NULL,
      content     TEXT NOT NULL DEFAULT '{}',
      raw         TEXT NOT NULL DEFAULT '{}',
      tokens_in   BIGINT,
      tokens_out  BIGINT,
      created_at  TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS cc_messages_session ON cc_messages (session_id, seq);

    -- Per-user Claude Code credentials, encrypted at rest (AES-256-GCM via
    -- HIVE_CRED_KEY). Reversible (unlike PAT/password hashes): the runner must
    -- hand the real token to Claude Code. Plaintext never leaves the server.
    CREATE TABLE IF NOT EXISTS cc_credentials (
      id           TEXT PRIMARY KEY,
      owner        TEXT NOT NULL,
      kind         TEXT NOT NULL,
      label        TEXT NOT NULL DEFAULT '',
      ciphertext   TEXT NOT NULL,
      nonce        TEXT NOT NULL,
      tail         TEXT NOT NULL DEFAULT '',
      created_at   TEXT NOT NULL,
      last_used_at TEXT
    );
    CREATE INDEX IF NOT EXISTS cc_credentials_owner ON cc_credentials (owner);
"#;

/// Unified full-text index. Postgres equivalent of the old FTS5 virtual table:
/// a regular table with a generated `tsvector` column (title + body, english
/// config) and a GIN index. Maintained by the same DELETE+INSERT path as before.
const SCHEMA_SEARCH: &str = r#"
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
    -- (entities). fields is REAL JSONB — a deliberate departure from the
    -- TEXT-JSON habit: server-side operators/indexes for user-shaped data.
    -- Rust binds it as TEXT with ?::jsonb casts and reads fields::text
    -- (workspace sqlx has no 'json' feature; SELECT * would fail to decode),
    -- so row_to_entity in store/custom_entities.rs is the only read path.
    -- Kind at the seams (search.kind, links.*_kind) is entity_types.slug;
    -- instance ids are uniformly 'ent_'.
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
      -- text | number | bool | date | choice | ref (validated in Rust only,
      -- same enforcement level as tasks.status).
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
      -- Visibility: same model as journal.user_scope (NULL = global).
      user_scope      TEXT,
      -- v2 journal-emergence provenance; carried now so nothing blocks it.
      origin_entry_id TEXT,
      created_by      TEXT NOT NULL,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS entities_type  ON entities (type_id, created_at);
    CREATE INDEX IF NOT EXISTS entities_scope ON entities (user_scope);
    -- Dormant in v1 (filters run in Rust at household scale); enables ad-hoc
    -- psql @> queries and server-side filtering later without a migration.
    CREATE INDEX IF NOT EXISTS entities_fields_gin ON entities USING GIN (fields jsonb_path_ops);

    -- Phase 1 mail archive skeleton. Sync credentials/state intentionally live
    -- elsewhere; these tables only hold read-only rows already ingested by a
    -- future hive-mail process. user_scope follows journal visibility.
    CREATE TABLE IF NOT EXISTS blobs (
      id           TEXT PRIMARY KEY,
      sha256       TEXT NOT NULL UNIQUE,
      content_type TEXT NOT NULL DEFAULT 'application/octet-stream',
      byte_len     BIGINT NOT NULL DEFAULT 0,
      storage_key  TEXT NOT NULL,
      created_at   TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS mail_accounts (
      id           TEXT PRIMARY KEY,
      user_scope   TEXT NOT NULL,
      provider     TEXT NOT NULL DEFAULT 'jmap',
      email        TEXT NOT NULL,
      display_name TEXT,
      created_at   TEXT NOT NULL,
      updated_at   TEXT NOT NULL,
      UNIQUE (user_scope, email)
    );
    CREATE INDEX IF NOT EXISTS mail_accounts_scope ON mail_accounts (user_scope, email);

    CREATE TABLE IF NOT EXISTS mail_mailboxes (
      id          TEXT PRIMARY KEY,
      account_id  TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
      mailbox_id  TEXT NOT NULL,
      name        TEXT NOT NULL,
      role        TEXT,
      sort_order  BIGINT NOT NULL DEFAULT 0,
      UNIQUE (account_id, mailbox_id)
    );
    CREATE INDEX IF NOT EXISTS mail_mailboxes_account ON mail_mailboxes (account_id, sort_order);

    CREATE TABLE IF NOT EXISTS mail_messages (
      id              TEXT PRIMARY KEY,
      account_id      TEXT NOT NULL REFERENCES mail_accounts(id) ON DELETE CASCADE,
      mailbox_id      TEXT REFERENCES mail_mailboxes(id) ON DELETE SET NULL,
      thread_id       TEXT NOT NULL,
      jmap_id         TEXT NOT NULL,
      message_id      TEXT,
      subject         TEXT NOT NULL DEFAULT '',
      from_name       TEXT,
      from_email      TEXT NOT NULL DEFAULT '',
      to_json         TEXT NOT NULL DEFAULT '[]',
      cc_json         TEXT NOT NULL DEFAULT '[]',
      received_at     TEXT NOT NULL,
      snippet         TEXT NOT NULL DEFAULT '',
      body_text       TEXT NOT NULL DEFAULT '',
      has_attachments BOOLEAN NOT NULL DEFAULT FALSE,
      created_at      TEXT NOT NULL,
      updated_at      TEXT NOT NULL,
      UNIQUE (account_id, jmap_id)
    );
    CREATE INDEX IF NOT EXISTS mail_messages_account_received ON mail_messages (account_id, received_at DESC);
    CREATE INDEX IF NOT EXISTS mail_messages_thread ON mail_messages (thread_id, received_at ASC);
    CREATE INDEX IF NOT EXISTS mail_messages_subject ON mail_messages (subject);

    CREATE TABLE IF NOT EXISTS mail_attachments (
      id          TEXT PRIMARY KEY,
      message_id  TEXT NOT NULL REFERENCES mail_messages(id) ON DELETE CASCADE,
      blob_id     TEXT REFERENCES blobs(id) ON DELETE SET NULL,
      filename    TEXT NOT NULL DEFAULT '',
      content_type TEXT NOT NULL DEFAULT 'application/octet-stream',
      byte_len    BIGINT NOT NULL DEFAULT 0,
      disposition TEXT,
      cid         TEXT
    );
    CREATE INDEX IF NOT EXISTS mail_attachments_message ON mail_attachments (message_id);
"#;

pub async fn migrate(pool: &PgPool) -> Result<()> {
    // Was this a brand-new database? `journal` is the oldest core table, so its
    // absence before this migrate run means a genuinely fresh install (→ run
    // onboarding); a DB that already has it is treated as already set up.
    let fresh = sqlx::query_scalar::<_, i32>(
        "SELECT 1 FROM information_schema.tables \
         WHERE table_schema = current_schema() AND table_name = 'journal'",
    )
    .fetch_optional(pool)
    .await?
    .is_none();

    sqlx::raw_sql(SCHEMA).execute(pool).await?;
    sqlx::raw_sql(SCHEMA_SEARCH).execute(pool).await?;

    // Idempotent column additions for DBs created before these columns existed.
    // Postgres has ADD COLUMN IF NOT EXISTS, so no existence probe is needed.
    for ddl in [
        "ALTER TABLE journal    ADD COLUMN IF NOT EXISTS user_scope TEXT",
        "ALTER TABLE sources    ADD COLUMN IF NOT EXISTS owner      TEXT",
        "ALTER TABLE tasks      ADD COLUMN IF NOT EXISTS phase      TEXT",
        "ALTER TABLE tasks      ADD COLUMN IF NOT EXISTS due        TEXT",
        "ALTER TABLE projects   ADD COLUMN IF NOT EXISTS slug       TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE people     ADD COLUMN IF NOT EXISTS owner      TEXT",
        "ALTER TABLE people     ADD COLUMN IF NOT EXISTS bio        TEXT",
        "ALTER TABLE people     ADD COLUMN IF NOT EXISTS role       TEXT",
        "ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS kind       TEXT",
        "ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS client_id  TEXT",
        "ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS granted_by TEXT",
        "ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS expires_at TEXT",
        "ALTER TABLE api_tokens ADD COLUMN IF NOT EXISTS scope      TEXT",
        "ALTER TABLE oauth_auth_codes ADD COLUMN IF NOT EXISTS token_ttl_secs BIGINT",
    ] {
        sqlx::raw_sql(ddl).execute(pool).await?;
    }

    // Onboarding gate: stamp the app version once; a fresh DB needs the wizard,
    // an existing DB is treated as already set up.
    let has_version: Option<String> =
        crate::pgq::query_scalar::<String>("SELECT value FROM config WHERE key = 'app.version'")
            .fetch_optional(pool)
            .await?;
    if has_version.is_none() {
        set_config(pool, "app.version", APP_VERSION).await?;
        set_config(
            pool,
            "onboarding.completed",
            if fresh { "false" } else { "true" },
        )
        .await?;
    }

    Ok(())
}

async fn set_config(pool: &PgPool, key: &str, value: &str) -> Result<()> {
    crate::pgq::query(
        "INSERT INTO config (key, value, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(now_iso())
    .execute(pool)
    .await?;
    Ok(())
}

/// Open + migrate in one call (the boot path for both api and worker).
pub async fn init() -> Result<PgPool> {
    let url = database_url();
    let pool = open(&url).await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Test helper: a pool pinned to a fresh, uniquely-named schema, migrated and
/// ready. Each test gets full isolation against one shared Postgres (DATABASE_URL
/// or the local dev default). Public (not cfg(test)) so integration tests can
/// use it; never called from the running binaries.
pub async fn test_pool() -> PgPool {
    let url = database_url();
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());

    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect admin");
    sqlx::raw_sql(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .expect("create schema");
    admin.close().await;

    // Pin every connection in the pool to the test schema via the libpq
    // `options` startup parameter — cleaner than an after_connect hook.
    let opts: PgConnectOptions = url.parse().expect("parse DATABASE_URL");
    let opts = opts.options([("search_path", schema.as_str())]);
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await
        .expect("connect pool");
    migrate(&pool).await.expect("migrate");
    pool
}
