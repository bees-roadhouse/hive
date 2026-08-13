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

use anyhow::{Context, Result};
use hive_shared::APP_VERSION;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use uuid::Uuid;

use crate::acting::{self, ActingScope};
use crate::store::now_iso;

/// Resolve the Postgres connection string from `DATABASE_URL`. Falls back to a
/// local dev instance so tests + local runs work without extra config.
pub fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://hive:hive@localhost:5432/hive".to_string())
}

/// The org an existing single-tenant install is folded into, and the org the
/// first-run wizard puts the first admin in. Fixed rather than random so a
/// migrated install lands somewhere nameable and tests can assert on it; org
/// ids are addresses, never secrets — membership is what grants access.
pub const DEFAULT_ORG_ID: Uuid = Uuid::from_u128(1);
pub const DEFAULT_ORG_SLUG: &str = "default";

/// The unprivileged role the API serves requests as. It owns nothing and has
/// neither SUPERUSER nor BYPASSRLS, because either one silently turns every
/// policy below into a comment.
pub const APP_ROLE: &str = "hive_app";

/// Open a pool WITHOUT the acting-org stamping — the owner/admin connection
/// used for DDL at boot. Never hand this to request-serving code: it is the
/// table owner, and only `FORCE ROW LEVEL SECURITY` stands between it and
/// every row in every org.
pub async fn open_admin(url: &str) -> Result<PgPool> {
    let pool = PgPoolOptions::new().max_connections(4).connect(url).await?;
    Ok(pool)
}

/// Stamp the current task's acting scope onto a connection. Runs at the two
/// (and only two) points a connection can enter user code: `after_connect` for
/// a freshly dialled one, `before_acquire` for one coming back off the idle
/// queue. Both fire inside the acquiring task, so both see that task's scope.
///
/// No scope means empty strings, which make the policy predicate NULL: reads
/// see nothing and writes trip `org_id NOT NULL`. Deny, not bypass.
async fn stamp_scope(
    conn: &mut sqlx::PgConnection,
    fallback: Option<&ActingScope>,
) -> Result<(), sqlx::Error> {
    let scope = acting::current().or_else(|| fallback.cloned());
    let (org, user, admin) = match &scope {
        Some(s) => (
            s.org.to_string(),
            s.user.clone(),
            if s.admin { "on" } else { "off" }.to_string(),
        ),
        None => (String::new(), String::new(), "off".to_string()),
    };
    sqlx::query(
        "SELECT set_config('hive.acting_org', $1, false), \
                set_config('hive.acting_user', $2, false), \
                set_config('hive.acting_admin', $3, false)",
    )
    .bind(org)
    .bind(user)
    .bind(admin)
    .execute(conn)
    .await?;
    Ok(())
}

/// The request-serving pool: connects as [`APP_ROLE`] and re-stamps the acting
/// scope on every checkout, so a connection can never carry one request's org
/// into another's. The `after_release` clear is belt to that braces — even if
/// it never ran, the next checkout overwrites the value before any statement.
pub async fn open_app(opts: PgConnectOptions) -> Result<PgPool> {
    open_app_inner(opts, None, 16).await
}

async fn open_app_inner(
    opts: PgConnectOptions,
    fallback: Option<ActingScope>,
    max_connections: u32,
) -> Result<PgPool> {
    let for_connect = fallback.clone();
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        // Leave min_connections at 0: a connection opened by the background
        // filler would run `after_connect` outside any request task. It would
        // be stamped empty (harmless), but keeping the filler off means every
        // connection in this pool was dialled by a task that had a scope.
        .after_connect(move |conn, _meta| {
            let fb = for_connect.clone();
            Box::pin(async move { stamp_scope(conn, fb.as_ref()).await })
        })
        .before_acquire(move |conn, _meta| {
            let fb = fallback.clone();
            Box::pin(async move {
                stamp_scope(conn, fb.as_ref()).await?;
                Ok(true)
            })
        })
        .after_release(|conn, _meta| {
            Box::pin(async move {
                sqlx::query(
                    "SELECT set_config('hive.acting_org', '', false), \
                            set_config('hive.acting_user', '', false), \
                            set_config('hive.acting_admin', 'off', false)",
                )
                .execute(conn)
                .await?;
                Ok(true)
            })
        })
        .connect_with(opts)
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

    -- Local embeddings for semantic search, one row per chunk (reshaped by
    -- migration 0002 — keep both in sync). `hash` is the ITEM-level content
    -- hash, identical on every chunk row, so skip logic stays one comparison.
    -- vec = packed little-endian f32 bytes (brute-force path, dual-written
    -- for BGE rows until the ANN path soaks); vec_v = native pgvector column
    -- (384-dim BGE only) behind the HNSW index. The 256-dim hash provider
    -- (dev/CI) writes vec only. owner = namespace user the row is visible to
    -- (NULL = global), stamped at insert.
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

    -- Claude Code artifacts (skills / agents / slash-commands) stored per AI
    -- identity. The plugin pulls THIS actor's enabled artifacts via the sync
    -- endpoint, authenticated by the AI-identity token. Keyed on the AI actor
    -- (people.slug), NOT the per-user memory namespace.
    -- Skills are modeled as a single SKILL.md `content` for v1 — multi-file
    -- skills (scripts, references) are out of scope for now.
    -- ===== Orgs (PLACEHOLDER — the orgs/memberships/RLS workstream owns this) =====
    -- Here only so `artifacts` can carry its org_id FK and RLS policy in their
    -- final shape from the start. Expect this block to be replaced wholesale by
    -- the real orgs + memberships + users reshape.
    CREATE TABLE IF NOT EXISTS orgs (
      id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
      slug       TEXT NOT NULL UNIQUE,
      name       TEXT NOT NULL,
      created_at TIMESTAMPTZ NOT NULL DEFAULT now()
    );

    -- ===== Artifacts: stored FILES =====
    -- Photos, scans, PDFs, audio, mail attachments. The row is the artifact;
    -- its bytes are content-addressed on disk at
    -- <data_root>/artifacts/<org_id>/<hh>/<sha256> (core/src/artifact_storage.rs),
    -- object storage later behind the same driver trait.
    --
    -- NOT the Claude Code skills/agents/commands surface — that is
    -- `identity_artifacts`, right below.
    --
    -- Dedup is (org_id, sha256) on the BYTES, one row per upload: filename,
    -- created_by, and created_at are per-upload facts, and derived artifacts
    -- (OCR text, captions, transcriptions, thumbnails — docs/ARTIFACTS.md
    -- Part 3) reference a parent artifact id, so two uploads of the same scan
    -- must stay two artifacts rather than fusing their derivation trees.
    -- THIS `id` IS THAT SEAM: a derived-artifacts table references artifacts(id)
    -- ON DELETE CASCADE and carries its own model/author/confidence columns.
    -- Nothing derived is produced here.
    CREATE TABLE IF NOT EXISTS artifacts (
      id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
      org_id     UUID NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
      sha256     TEXT NOT NULL,
      mime       TEXT NOT NULL,
      bytes      BIGINT NOT NULL,
      filename   TEXT,
      created_by TEXT,
      created_at TIMESTAMPTZ NOT NULL DEFAULT now()
    );
    CREATE INDEX IF NOT EXISTS artifacts_org ON artifacts (org_id, created_at DESC);
    -- The delete-time refcount ("any row left in this org holding these bytes?").
    CREATE INDEX IF NOT EXISTS artifacts_org_sha ON artifacts (org_id, sha256);

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
      runtime           TEXT NOT NULL DEFAULT 'claude_code',
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

    -- Browser-based OAuth handshakes for hosted agent runtimes. State rows are
    -- short-lived and redeemed exactly once by /api/runtime-oauth/:runtime/callback.
    CREATE TABLE IF NOT EXISTS runtime_oauth_states (
      state         TEXT PRIMARY KEY,
      owner         TEXT NOT NULL,
      runtime       TEXT NOT NULL,
      provider      TEXT,
      code_verifier TEXT NOT NULL,
      return_to     TEXT NOT NULL DEFAULT '/settings',
      created_at    TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS runtime_oauth_states_owner ON runtime_oauth_states (owner);
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

    -- Phase 1 mail archive. The mail sync driver (jmap-sync's consumer;
    -- returns as a module in Phase 3) writes these; sync state (JMAP state
    -- strings, backfill cursor, backoff bookkeeping) lives on mail_accounts,
    -- and the account credential is a cc_credentials row named by cred_id.
    -- user_scope is NEVER NULL for mail (there is no global mail).
    CREATE TABLE IF NOT EXISTS blobs (
      hash       TEXT PRIMARY KEY,
      size       BIGINT NOT NULL DEFAULT 0,
      mime       TEXT NOT NULL DEFAULT 'application/octet-stream',
      data       BYTEA,
      created_at TEXT NOT NULL
    );

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
    -- The embed-queue scan (drained by the worker once the 1.5 retrieval work
    -- lands): pending, live rows per account, newest first.
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
    -- Reverse lookup for the blob refcount GC sweep and cascade orphan cleanup.
    CREATE INDEX IF NOT EXISTS mail_attachments_blob ON mail_attachments (blob_hash);
"#;

/// The tenancy plane. Deliberately NOT row-level-secured: resolving a session
/// cookie to a user, and a user to an org, has to happen before an acting org
/// exists. These four tables are the only ones a request may touch unscoped,
/// and none of them holds content.
///
/// `user_identities` follows `docs/SELF-HOST.md`: `UNIQUE (issuer, subject)` so
/// a provider account maps to exactly one local identity, and `user_id` is NOT
/// unique so one identity may hold many providers. Email is nowhere in it —
/// email is a display attribute, never a join key.
const SCHEMA_ORGS: &str = r#"
    CREATE TABLE IF NOT EXISTS orgs (
      id         UUID PRIMARY KEY,
      slug       TEXT NOT NULL UNIQUE,
      name       TEXT NOT NULL,
      created_at TIMESTAMPTZ NOT NULL DEFAULT now()
    );

    -- users.id is the house `usr_<nanoid>` TEXT id, so this column is TEXT
    -- rather than the uuid the design note sketched; org_id stays UUID because
    -- the RLS predicate casts it.
    CREATE TABLE IF NOT EXISTS memberships (
      user_id    TEXT NOT NULL,
      org_id     UUID NOT NULL REFERENCES orgs(id) ON DELETE CASCADE,
      role       TEXT NOT NULL DEFAULT 'member',
      created_at TEXT NOT NULL,
      PRIMARY KEY (user_id, org_id)
    );
    CREATE INDEX IF NOT EXISTS memberships_org ON memberships (org_id);

    CREATE TABLE IF NOT EXISTS user_identities (
      id         TEXT PRIMARY KEY,
      user_id    TEXT NOT NULL,
      issuer     TEXT NOT NULL,
      subject    TEXT NOT NULL,
      created_at TEXT NOT NULL,
      UNIQUE (issuer, subject)
    );
    CREATE INDEX IF NOT EXISTS user_identities_user ON user_identities (user_id);
"#;

/// Every content table. Each one gets `org_id`, a foreign key to `orgs`, an
/// index, and the policy pair below. Adding a content table means adding it
/// here — nothing else in the tree filters by org, so an omission is a hole.
const ORG_TABLES: &[&str] = &[
    "journal",
    "anchors",
    "projects",
    "topics",
    "phases",
    "tasks",
    "decisions",
    "events",
    "inbox",
    "links",
    "wire",
    "sources",
    "outbox",
    "embeddings",
    "people",
    "shares",
    "profile",
    "identities",
    "identity_artifacts",
    "cc_sessions",
    "cc_messages",
    "cc_credentials",
    "search",
    "entity_types",
    "entity_fields",
    "entities",
    "mail_accounts",
    "mail_mailboxes",
    "mail_messages",
    "mail_attachments",
];

/// Content tables carrying a per-user namespace column. The namespace used to
/// be enforced by `Visibility` filters composed into every query in the store
/// layer — the ACL-ordering surface D16 named. It is a policy predicate now,
/// so there is no query left to compose it into wrong.
const USER_SCOPE_TABLES: &[(&str, &str)] = &[
    ("journal", "user_scope"),
    ("entities", "user_scope"),
    ("mail_messages", "user_scope"),
    ("cc_sessions", "owner"),
    ("cc_credentials", "owner"),
    ("mail_accounts", "owner"),
];

/// Unique constraints that have to be per-org or two orgs cannot both have a
/// person called `nate`. Rebuilt as `(org_id, …)`; the queries that read them
/// are unchanged because RLS already narrows the lookup to one org.
const ORG_UNIQUE_REBUILDS: &[(&str, &str, &str)] = &[
    ("people", "people_slug_key", "slug"),
    ("projects", "projects_name_key", "name"),
    ("topics", "topics_slug_key", "slug"),
    ("entity_types", "entity_types_slug_key", "slug"),
    (
        "identities",
        "identities_platform_platform_id_key",
        "platform, platform_id",
    ),
    (
        "identity_artifacts",
        "identity_artifacts_actor_kind_name_key",
        "actor, kind, name",
    ),
    (
        "mail_accounts",
        "mail_accounts_owner_address_key",
        "owner, address",
    ),
    ("shares", "shares_uniq", "scope, ref, viewer"),
];

const ACTING_ORG: &str = "nullif(current_setting('hive.acting_org', true), '')::uuid";
const ACTING_USER: &str = "nullif(current_setting('hive.acting_user', true), '')";
const ACTING_ADMIN: &str = "current_setting('hive.acting_admin', true) = 'on'";

/// `org_id = acting_org`, and inside the org, the row's namespace owner is
/// either nobody (shared), you, or you are an admin. One predicate, applied by
/// the database to every statement, with no ordering to get wrong.
fn org_predicate(table: &str) -> String {
    let org = format!("org_id = {ACTING_ORG}");
    match USER_SCOPE_TABLES.iter().find(|(t, _)| *t == table) {
        Some((_, col)) => {
            format!("({org} AND ({col} IS NULL OR {col} = {ACTING_USER} OR {ACTING_ADMIN}))")
        }
        None => format!("({org})"),
    }
}

/// The journal's READ predicate widens the namespace by explicit grant: an
/// entry shared to you, an author whose journal is shared to you, or an entry
/// that @mentions you. That was `visible_entry_ids` in the store layer — a set
/// of ids computed in Rust and then ANDed into every query by hand, which is
/// exactly the ordering surface D16 named. It is a predicate now.
///
/// The WRITE predicate does NOT widen. Someone sharing their journal with you
/// lets you read it; it does not let you write into their namespace. Splitting
/// USING from WITH CHECK is what makes that expressible at all.
fn journal_read_predicate() -> String {
    let base = org_predicate("journal");
    format!(
        "({base} OR (org_id = {ACTING_ORG} AND ( \
            EXISTS (SELECT 1 FROM shares s WHERE s.org_id = journal.org_id \
                    AND s.viewer = {ACTING_USER} \
                    AND ((s.scope = 'entry' AND s.ref = journal.id) \
                      OR (s.scope = 'journal' AND s.ref = journal.author))) \
            OR journal.mentions LIKE '%\"' || {ACTING_USER} || '\"%' \
         )))"
    )
}

/// Add `org_id` everywhere, fold pre-org rows into the default org, and turn
/// RLS on. Idempotent, and runs after the inline base DDL because it has to
/// reference tables that DDL creates (sqlx migrations run before it).
async fn apply_org_scoping(pool: &PgPool) -> Result<()> {
    sqlx::raw_sql(SCHEMA_ORGS).execute(pool).await?;

    crate::pgq::query(
        "INSERT INTO orgs (id, slug, name) VALUES (?, ?, ?) ON CONFLICT (id) DO NOTHING",
    )
    .bind(DEFAULT_ORG_ID)
    .bind(DEFAULT_ORG_SLUG)
    .bind("Default")
    .execute(pool)
    .await?;

    // The credential, not the request, decides the acting org: a session and a
    // token each pin exactly one. Nothing reads an org from a header or a body.
    for ddl in [
        "ALTER TABLE sessions         ADD COLUMN IF NOT EXISTS org_id UUID",
        "ALTER TABLE api_tokens       ADD COLUMN IF NOT EXISTS org_id UUID",
        "ALTER TABLE oauth_auth_codes ADD COLUMN IF NOT EXISTS org_id UUID",
    ] {
        sqlx::raw_sql(ddl).execute(pool).await?;
    }
    for table in ["sessions", "api_tokens", "oauth_auth_codes"] {
        sqlx::raw_sql(&format!(
            "UPDATE {table} SET org_id = '{DEFAULT_ORG_ID}'::uuid WHERE org_id IS NULL"
        ))
        .execute(pool)
        .await?;
    }
    // A token and an auth code are minted inside a request that already has an
    // acting org, so the default carries it across without a query change —
    // and, more to the point, without a place to pass the WRONG one.
    for table in ["api_tokens", "oauth_auth_codes"] {
        sqlx::raw_sql(&format!(
            "ALTER TABLE {table} ALTER COLUMN org_id \
             SET DEFAULT nullif(current_setting('hive.acting_org', true), '')::uuid"
        ))
        .execute(pool)
        .await?;
    }

    // Pass one: the column, everywhere. A policy may reference another table
    // (the journal's read predicate joins `shares`), so every `org_id` has to
    // exist before any policy is written.
    for table in ORG_TABLES {
        let stmts = [
            format!("ALTER TABLE {table} ADD COLUMN IF NOT EXISTS org_id UUID"),
            format!("UPDATE {table} SET org_id = '{DEFAULT_ORG_ID}'::uuid WHERE org_id IS NULL"),
            // The default is what keeps ~180 hand-written INSERTs correct
            // without editing (and forgetting) any of them: omit org_id and
            // the row lands in the acting org, or fails NOT NULL if there
            // isn't one.
            format!(
                "ALTER TABLE {table} ALTER COLUMN org_id \
                 SET DEFAULT nullif(current_setting('hive.acting_org', true), '')::uuid"
            ),
            format!("ALTER TABLE {table} ALTER COLUMN org_id SET NOT NULL"),
            format!("CREATE INDEX IF NOT EXISTS {table}_org ON {table} (org_id)"),
        ];
        for sql in stmts {
            sqlx::raw_sql(&sql)
                .execute(pool)
                .await
                .with_context(|| format!("org-scoping {table}: {sql}"))?;
        }
        // The FK is added separately: a pre-existing one makes ADD CONSTRAINT
        // fail, and Postgres has no ADD CONSTRAINT IF NOT EXISTS.
        let fk = format!("{table}_org_fk");
        sqlx::raw_sql(&format!(
            "DO $$ BEGIN \
               IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = '{fk}') THEN \
                 ALTER TABLE {table} ADD CONSTRAINT {fk} FOREIGN KEY (org_id) REFERENCES orgs(id); \
               END IF; \
             END $$"
        ))
        .execute(pool)
        .await
        .with_context(|| format!("org FK on {table}"))?;
    }

    // Pass two: the policies.
    for table in ORG_TABLES {
        let write = org_predicate(table);
        let read = if *table == "journal" {
            journal_read_predicate()
        } else {
            write.clone()
        };
        for sql in [
            format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY"),
            // Without FORCE, the owner — which is whoever ran this DDL —
            // sails past every policy.
            format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY"),
            format!("DROP POLICY IF EXISTS {table}_org ON {table}"),
            // WITH CHECK as well as USING: USING filters reads, and without
            // WITH CHECK a caller can INSERT into an org it cannot read.
            format!("CREATE POLICY {table}_org ON {table} USING {read} WITH CHECK {write}"),
        ] {
            sqlx::raw_sql(&sql)
                .execute(pool)
                .await
                .with_context(|| format!("org policy on {table}: {sql}"))?;
        }
    }

    for (table, index, cols) in ORG_UNIQUE_REBUILDS {
        for sql in [
            format!("ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {index}"),
            format!("DROP INDEX IF EXISTS {index}"),
            format!(
                "CREATE UNIQUE INDEX IF NOT EXISTS {table}_org_uniq ON {table} (org_id, {cols})"
            ),
        ] {
            sqlx::raw_sql(&sql)
                .execute(pool)
                .await
                .with_context(|| format!("per-org uniqueness on {table}: {sql}"))?;
        }
    }
    // cc_sessions' capture-dedup index is partial, so it is rebuilt on its own.
    for sql in [
        "DROP INDEX IF EXISTS cc_sessions_captured_ext",
        "CREATE UNIQUE INDEX IF NOT EXISTS cc_sessions_captured_ext ON cc_sessions \
         (org_id, runtime, claude_session_id) WHERE origin = 'captured'",
    ] {
        sqlx::raw_sql(sql).execute(pool).await?;
    }
    // `profile` is keyed on an actor slug, which is only unique inside an org
    // now — so its PRIMARY KEY has to move too, or two orgs cannot both have a
    // person called `nate`.
    sqlx::raw_sql(
        "DO $$ BEGIN \
           IF EXISTS (SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid \
                      WHERE i.indrelid = 'profile'::regclass AND i.indisprimary \
                        AND i.indnatts = 1) THEN \
             ALTER TABLE profile DROP CONSTRAINT profile_pkey; \
             ALTER TABLE profile ADD PRIMARY KEY (org_id, actor); \
           END IF; \
         END $$",
    )
    .execute(pool)
    .await
    .context("per-org profile key")?;

    // Every existing login joins the default org, so an upgraded install keeps
    // working the moment it boots.
    crate::pgq::query(
        "INSERT INTO memberships (user_id, org_id, role, created_at) \
         SELECT u.id, ?, CASE WHEN u.role = 'admin' THEN 'admin' ELSE 'member' END, ? \
         FROM users u ON CONFLICT (user_id, org_id) DO NOTHING",
    )
    .bind(DEFAULT_ORG_ID)
    .bind(now_iso())
    .execute(pool)
    .await?;

    grant_app_role(pool).await
}

/// The serving role's password, derived rather than stored.
///
/// Deriving it from `DATABASE_URL` has three properties worth the oddity: it is
/// identical on every instance sharing the database (so two API processes never
/// fight over it), it needs no storage (so there is no file or row to leak it
/// from), and it is never weaker than what it protects — anyone who can guess
/// `DATABASE_URL` already holds the owner's credentials and does not need this
/// one. `HIVE_APP_DATABASE_URL` opts out for operators who provision the role
/// themselves.
fn app_password(url: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(format!("hive-app-role-v1:{url}").as_bytes()))
}

/// Idempotent, concurrency-safe `CREATE ROLE`. Roles are cluster-wide, so two
/// instances (or two parallel tests) boot against the same row in `pg_authid`;
/// every write here is guarded by a check so the steady state is zero writes
/// and a genuine race is `duplicate_object`, which is the outcome we wanted.
async fn ensure_app_role(pool: &PgPool, password: &str) -> Result<()> {
    let mut tx = pool.begin().await?;
    // Roles are cluster-wide. Guards make each write conditional, but two boots
    // can still evaluate the guard and write in the same instant, which
    // Postgres reports as "tuple concurrently updated". An advisory lock makes
    // the whole check-then-write atomic across processes; it releases at
    // commit, so a crashed boot cannot wedge the next one.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtext('hive:app-role'))")
        .execute(&mut *tx)
        .await?;
    sqlx::raw_sql(&format!(
        "DO $$ BEGIN \
           IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{APP_ROLE}') THEN \
             BEGIN \
               CREATE ROLE {APP_ROLE} LOGIN PASSWORD '{password}'; \
             EXCEPTION WHEN duplicate_object THEN NULL; \
             END; \
           END IF; \
           IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{APP_ROLE}' \
                      AND (rolsuper OR rolbypassrls OR rolcreatedb OR rolcreaterole \
                           OR NOT rolcanlogin)) THEN \
             ALTER ROLE {APP_ROLE} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS; \
           END IF; \
         END $$"
    ))
    .execute(&mut *tx)
    .await
    .context(
        "creating the unprivileged API role (the DATABASE_URL role needs CREATEROLE, or set \
         HIVE_APP_DATABASE_URL to a role you provisioned yourself)",
    )?;
    tx.commit().await?;
    Ok(())
}

/// Give [`APP_ROLE`] exactly DML on the current schema — never ownership, never
/// DDL. Re-run every boot so a table added since the last one is covered.
async fn grant_app_role(pool: &PgPool) -> Result<()> {
    ensure_app_role(pool, &app_password(&database_url())).await?;
    let schema: String = crate::pgq::query_scalar::<String>("SELECT current_schema()")
        .fetch_one(pool)
        .await?;
    // `public` is shared by every schema on the cluster, so the grant on it is
    // conditional: an unconditional one is a write to a row two parallel boots
    // both want, and Postgres answers that with "tuple concurrently updated".
    sqlx::raw_sql(&format!(
        "DO $$ BEGIN \
           IF NOT has_schema_privilege('{APP_ROLE}', 'public', 'USAGE') THEN \
             GRANT USAGE ON SCHEMA public TO {APP_ROLE}; \
           END IF; \
         END $$"
    ))
    .execute(pool)
    .await
    .with_context(|| format!("granting {APP_ROLE} usage on public"))?;
    for sql in [
        format!("GRANT USAGE ON SCHEMA \"{schema}\" TO {APP_ROLE}"),
        format!(
            "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA \"{schema}\" TO {APP_ROLE}"
        ),
        format!("GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA \"{schema}\" TO {APP_ROLE}"),
        format!(
            "ALTER DEFAULT PRIVILEGES IN SCHEMA \"{schema}\" \
             GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {APP_ROLE}"
        ),
    ] {
        sqlx::raw_sql(&sql)
            .execute(pool)
            .await
            .with_context(|| format!("granting {APP_ROLE}: {sql}"))?;
    }
    Ok(())
}

/// Connect options for the serving role. Nothing is rotated and nothing is
/// written in the steady state, so two instances booting at once do not fight.
/// `HIVE_APP_DATABASE_URL` opts out entirely for deployments that provision the
/// role themselves.
async fn app_connect_options(admin: &PgPool, url: &str) -> Result<PgConnectOptions> {
    if let Ok(explicit) = std::env::var("HIVE_APP_DATABASE_URL") {
        if !explicit.trim().is_empty() {
            return Ok(explicit.parse::<PgConnectOptions>()?);
        }
    }
    let password = app_password(url);
    ensure_app_role(admin, &password).await?;
    Ok(url
        .parse::<PgConnectOptions>()?
        .username(APP_ROLE)
        .password(&password))
}

/// Open the serving pool, healing a role whose password predates this scheme.
/// The heal is triggered by a failed connection rather than by inspecting
/// `pg_authid` (superuser-only) and writes the same deterministic value every
/// instance would, so a concurrent heal is a no-op rather than a fight.
async fn connect_app(admin: &PgPool, opts: PgConnectOptions, url: &str) -> Result<PgPool> {
    match open_app(opts.clone()).await {
        Ok(pool) => Ok(pool),
        Err(e) if is_auth_failure(&e) => {
            let mut tx = admin.begin().await?;
            sqlx::query("SELECT pg_advisory_xact_lock(hashtext('hive:app-role'))")
                .execute(&mut *tx)
                .await?;
            sqlx::raw_sql(&format!(
                "ALTER ROLE {APP_ROLE} WITH PASSWORD '{}'",
                app_password(url)
            ))
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            open_app(opts).await
        }
        Err(e) => Err(e),
    }
}

fn is_auth_failure(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::Database(db)) if db.code().as_deref() == Some("28P01")
    )
}

/// Refuse to serve as a role that outranks the policies. A superuser or a
/// BYPASSRLS role reads every org's rows and no policy fires, so this is a
/// hard stop rather than a warning. Unknown answers count as privileged.
pub async fn assert_rls_applies(pool: &PgPool) -> Result<()> {
    let role: String = crate::pgq::query_scalar::<String>("SELECT current_user::text")
        .fetch_one(pool)
        .await?;
    let privileged: bool = crate::pgq::query_scalar::<bool>(
        "SELECT COALESCE((SELECT bool_or(rolsuper OR rolbypassrls) FROM pg_roles \
                          WHERE rolname = current_user), true)",
    )
    .fetch_one(pool)
    .await?;
    anyhow::ensure!(
        !privileged,
        "the API is connecting as {role}, which is SUPERUSER or BYPASSRLS — row-level security \
         does not apply to it and every org would be readable. Point HIVE_APP_DATABASE_URL at an \
         unprivileged role."
    );
    Ok(())
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    // sqlx-managed reshape migrations run FIRST, before the inline base DDL
    // below (hybrid convention — see core/migrations/0001_baseline_marker.sql).
    // The migrator holds a Postgres advisory lock, so api + worker booting at
    // the same time serialize here instead of racing. A failed migration
    // aborts init: the binary exits and compose crash-loops loudly — fail
    // closed rather than run against a half-reshaped schema.
    sqlx::migrate!("./migrations").run(pool).await?;

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
        "ALTER TABLE cc_sessions ADD COLUMN IF NOT EXISTS runtime TEXT NOT NULL DEFAULT 'claude_code'",
        // Conversation capture (SessionEnd ingest of local Claude Code sessions)
        // rides the cc_sessions table: origin marks how a row got here
        // ('hosted' = runner-driven, 'captured' = local-session ingest),
        // summary + reflected_at are the reflection loop's rolling summary and
        // cursor (NULL reflected_at = queued for reflection).
        "ALTER TABLE cc_sessions ADD COLUMN IF NOT EXISTS origin TEXT NOT NULL DEFAULT 'hosted'",
        "ALTER TABLE cc_sessions ADD COLUMN IF NOT EXISTS summary TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE cc_sessions ADD COLUMN IF NOT EXISTS reflected_at TEXT",
        // Idempotent capture key: a resumed local session re-fires SessionEnd
        // with the same app session id — one captured row per (runtime,
        // claude_session_id). Partial: hosted rows are exempt (their
        // claude_session_id arrives late and isn't a dedup key).
        "CREATE UNIQUE INDEX IF NOT EXISTS cc_sessions_captured_ext ON cc_sessions (runtime, claude_session_id) WHERE origin = 'captured'",
        "ALTER TABLE cc_credentials ADD COLUMN IF NOT EXISTS runtime TEXT NOT NULL DEFAULT 'claude_code'",
        "ALTER TABLE cc_credentials ADD COLUMN IF NOT EXISTS provider TEXT",
        "CREATE TABLE IF NOT EXISTS runtime_oauth_states (state TEXT PRIMARY KEY, owner TEXT NOT NULL, runtime TEXT NOT NULL, provider TEXT, code_verifier TEXT NOT NULL, return_to TEXT NOT NULL DEFAULT '/settings', created_at TEXT NOT NULL)",
        "CREATE INDEX IF NOT EXISTS runtime_oauth_states_owner ON runtime_oauth_states (owner)",
        "ALTER TABLE mail_accounts ADD COLUMN IF NOT EXISTS jmap_username TEXT",
        // Attachment bytes arrive pre-compressed (images, PDFs, zips):
        // skipping TOAST compression on the BYTEA saves CPU on both sides.
        "ALTER TABLE blobs ALTER COLUMN data SET STORAGE EXTERNAL",
        // Row-level security on artifacts. FORCE, because the API role owns the
        // tables and an owner bypasses its own policies without it; WITH CHECK
        // as well as USING, or a caller could INSERT into an org it cannot read.
        // `CREATE POLICY` has no IF NOT EXISTS, hence the probe.
        //
        // The policy reads `hive.acting_org`, set per transaction by
        // Store::org_tx today and per request by the auth middleware once the
        // orgs workstream lands. Note that RLS is only as good as the role: a
        // superuser or BYPASSRLS role defeats it entirely, and the local dev
        // role is currently both.
        "ALTER TABLE artifacts ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE artifacts FORCE ROW LEVEL SECURITY",
        "DO $$ BEGIN \
           IF NOT EXISTS ( \
             SELECT 1 FROM pg_policies \
             WHERE schemaname = current_schema() AND tablename = 'artifacts' \
               AND policyname = 'artifacts_org' \
           ) THEN \
             CREATE POLICY artifacts_org ON artifacts \
               USING      (org_id = current_setting('hive.acting_org')::uuid) \
               WITH CHECK (org_id = current_setting('hive.acting_org')::uuid); \
           END IF; \
         END $$",
    ] {
        sqlx::raw_sql(ddl).execute(pool).await?;
    }

    // Tenancy last: it adds a column to every table the DDL above creates, so
    // it cannot be a sqlx migration (those run first, against a DB where the
    // base tables may not exist yet).
    apply_org_scoping(pool).await?;

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
///
/// Two connections, two roles: DDL runs as the owner, and the pool handed back
/// for serving requests is the unprivileged one the policies actually apply to.
pub async fn init() -> Result<PgPool> {
    let url = database_url();
    let admin = open_admin(&url).await?;
    migrate(&admin).await?;
    let opts = app_connect_options(&admin, &url).await?;
    let pool = connect_app(&admin, opts, &url).await?;
    admin.close().await;

    assert_rls_applies(&pool).await?;
    Ok(pool)
}

/// Drops the schema it names when it goes out of scope. Constructed the moment
/// `CREATE SCHEMA` returns, so a helper that panics half-way through migrating
/// still takes its schema with it.
///
/// `Drop` cannot await, and every test here runs on `#[tokio::test]`'s
/// current-thread runtime, where `block_in_place` panics — so the DROP goes to
/// its own thread with its own runtime and its own connection. It must not
/// touch the test's pool: those sockets are registered with the test's runtime,
/// and driving them from another thread would deadlock against the very thread
/// this one is blocking. A private thread per guard rather than one shared
/// teardown worker, so two tests finishing at once do not queue behind each
/// other.
struct SchemaGuard {
    schema: String,
    url: String,
}

impl Drop for SchemaGuard {
    fn drop(&mut self) {
        let (schema, url) = (self.schema.clone(), self.url.clone());
        // `Builder::spawn` rather than `thread::spawn`, which PANICS when the
        // OS refuses a thread — and nothing in this function may panic (below).
        let spawned = std::thread::Builder::new()
            .name("test-schema-teardown".to_string())
            .spawn(move || -> Result<()> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("teardown runtime")?;
                rt.block_on(async move {
                    let conn = PgPoolOptions::new()
                        .max_connections(1)
                        .connect(&url)
                        .await
                        .context("teardown connect")?;
                    // CASCADE because the schema holds every table the migrate
                    // path built, plus their indexes, policies and default
                    // ACLs. The test's own connections are idle by now and
                    // hold no locks, so this does not wait on them.
                    let dropped =
                        sqlx::raw_sql(&format!("DROP SCHEMA IF EXISTS \"{schema}\" CASCADE"))
                            .execute(&conn)
                            .await
                            .with_context(|| format!("dropping test schema {schema}"));
                    conn.close().await;
                    dropped.map(|_| ())
                })
            });
        // Never panic here. A failed teardown leaks exactly one schema; a panic
        // while already unwinding a failed test aborts the process and buries
        // the real failure. Say it loudly on stderr instead.
        match spawned.map(|handle| handle.join()) {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => eprintln!("test schema teardown failed: {e:#}"),
            Ok(Err(_)) => eprintln!("test schema teardown thread panicked"),
            Err(e) => eprintln!("test schema teardown thread not spawned: {e}"),
        }
    }
}

/// A test database: one throwaway schema, the pool bound to it, and the
/// teardown that drops the schema. Holding all three in ONE value is the point
/// — the schema lives exactly as long as the test that owns it, and a test that
/// panics still cleans up, because unwinding runs `Drop`.
///
/// Keep it bound for the whole test (`let (app, store, _test_db) = …`). A
/// helper that builds a `Store` or a `Router` has to hand this back with them:
/// dropping it inside the helper drops the schema out from under what it
/// returned, and the next query fails with `relation "…" does not exist`.
#[must_use = "dropping the TestDb drops the schema — bind it for the whole test"]
pub struct TestDb {
    pub pool: PgPool,
    guard: SchemaGuard,
}

impl TestDb {
    /// The schema this database lives in — `current_schema()` for every
    /// connection in [`TestDb::pool`].
    pub fn schema(&self) -> &str {
        &self.guard.schema
    }
}

/// Build a fresh, uniquely-named schema, migrate it, and hand back a pool on
/// it as the unprivileged serving role. Each test gets full isolation against
/// one shared Postgres (DATABASE_URL or the local dev default). Public (not
/// cfg(test)) so integration tests can use it; never called from the binaries.
async fn test_pool_with(fallback: Option<ActingScope>, max_connections: u32) -> TestDb {
    let (schema, guard, base) = create_test_schema().await;

    // Pin every connection in the pool to the test schema via the libpq
    // `options` startup parameter — cleaner than an after_connect hook.
    // `public` stays on the path so extension types installed there (e.g.
    // pgvector) resolve inside test schemas; the test schema is first, so all
    // created objects (including sqlx's per-schema _sqlx_migrations) land in it.
    let search_path = || [("search_path", format!("{schema},public"))];
    let url = guard.url.clone();
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect_with(base.options(search_path()))
        .await
        .expect("connect admin pool");
    migrate(&admin).await.expect("migrate");
    let opts = app_connect_options(&admin, url.as_str())
        .await
        .expect("provision app role")
        .options(search_path());
    // One connect-and-heal on the shared role before the real pool opens, so a
    // stale password from an earlier scheme is fixed once rather than by every
    // parallel test at once.
    connect_app(&admin, opts.clone(), url.as_str())
        .await
        .expect("app role usable")
        .close()
        .await;
    admin.close().await;

    // Tests run against the same unprivileged, scope-stamping pool the binary
    // serves from — a test against a superuser pool would prove nothing.
    let pool = open_app_inner(opts, fallback, max_connections)
        .await
        .expect("connect app pool");
    assert_rls_applies(&pool).await.expect("rls applies");
    TestDb { pool, guard }
}

/// `CREATE SCHEMA` + the guard that will drop it. Split out so the guard exists
/// before anything that can panic (migrate, role provisioning) runs against it.
async fn create_test_schema() -> (String, SchemaGuard, PgConnectOptions) {
    let url = database_url();
    let schema = format!("t_{}", uuid::Uuid::new_v4().simple());

    let boot = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connect admin");
    sqlx::raw_sql(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&boot)
        .await
        .expect("create schema");
    boot.close().await;

    let base: PgConnectOptions = url.parse().expect("parse DATABASE_URL");
    let guard = SchemaGuard {
        schema: schema.clone(),
        url,
    };
    (schema, guard, base)
}

/// The general test pool. A task with NO acting scope falls back to the default
/// org, so store-level tests (which never pass through the auth middleware, and
/// so never open a scope) keep exercising real SQL.
///
/// That fallback exists ONLY here. `init()` calls `open_app`, which hardcodes
/// `None`, so in the binary a request that fails to establish a scope reads
/// nothing. Anything asserting on isolation must use [`test_pool_strict`].
pub async fn test_pool() -> TestDb {
    test_pool_with(Some(ActingScope::new(DEFAULT_ORG_ID, "tests", true)), 5).await
}

/// No fallback scope: an unscoped task sees nothing, exactly as the binary
/// behaves. Use for anything that asserts on isolation, so the assertion is
/// about the policy and not about a helper.
pub async fn test_pool_strict() -> TestDb {
    test_pool_with(None, 5).await
}

/// [`test_pool_strict`] with a single connection, so every statement in a test
/// provably rides the SAME physical connection. This is what turns "a pooled
/// connection is reused across two orgs" from a hope into a test.
pub async fn test_pool_single_conn() -> TestDb {
    test_pool_with(None, 1).await
}

/// An empty schema and an OWNER-role pool on it, with [`migrate`] NOT run — for
/// the migration tests, which lay down an old-shape table by hand and then
/// migrate over it. Same schema-per-test isolation and the same teardown as the
/// migrated helpers above.
pub async fn test_pool_unmigrated() -> TestDb {
    let (schema, guard, base) = create_test_schema().await;
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(base.options([("search_path", format!("{schema},public"))]))
        .await
        .expect("connect pool");
    TestDb { pool, guard }
}

/// Test helper: an owner-role pool on the SAME schema as `pool`, for the
/// setup a test has to do from outside any org (seeding a second org, reading
/// across orgs to check a leak did not happen).
pub async fn test_admin_pool(pool: &PgPool) -> PgPool {
    let schema: String = crate::pgq::query_scalar::<String>("SELECT current_schema()")
        .fetch_one(pool)
        .await
        .expect("current schema");
    let base: PgConnectOptions = database_url().parse().expect("parse DATABASE_URL");
    PgPoolOptions::new()
        .max_connections(2)
        .connect_with(base.options([("search_path", format!("{schema},public"))]))
        .await
        .expect("connect admin pool")
}
