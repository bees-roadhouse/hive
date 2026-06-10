// SQLite is the whole datastore — parity port of packages/api/src/db.ts.
// Schema statements mirror the Node API byte-for-byte (CREATE TABLE IF NOT
// EXISTS + idempotent column adds) so this binary boots cleanly on a database
// the Node API created, and vice versa. The only addition is the `identities`
// table (cross-platform identity mapping, a Rust-branch feature).

use anyhow::Result;
use hive_shared::APP_VERSION;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::auth::now_iso;

/// Resolve the database path: $HIVE_DB or ./data/hive.db next to the workspace.
pub fn db_path() -> String {
    std::env::var("HIVE_DB").unwrap_or_else(|_| "data/hive.db".to_string())
}

pub async fn open(path: &str) -> Result<SqlitePool> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(30));
    // better-sqlite3 is single-connection; SQLite WAL handles one writer at a
    // time. A small pool keeps reads concurrent without writer contention.
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
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
      created_at TEXT NOT NULL
    );

    -- A span of a journal entry that produced a structured entity.
    CREATE TABLE IF NOT EXISTS anchors (
      id         TEXT PRIMARY KEY,
      entry_id   TEXT NOT NULL,
      start      INTEGER NOT NULL,
      "end"      INTEGER NOT NULL,
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
      position   INTEGER NOT NULL DEFAULT 0,
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
      interval_secs INTEGER NOT NULL DEFAULT 900,
      notify        TEXT,
      enabled       INTEGER NOT NULL DEFAULT 1,
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
      attempts     INTEGER NOT NULL DEFAULT 0,
      last_error   TEXT,
      run_after    TEXT NOT NULL,
      created_at   TEXT NOT NULL,
      completed_at TEXT
    );
    CREATE INDEX IF NOT EXISTS outbox_pending ON outbox (status, run_after);

    -- Local embeddings for semantic search (vec = packed little-endian f32 BLOB).
    CREATE TABLE IF NOT EXISTS embeddings (
      ref_kind   TEXT NOT NULL,
      ref_id     TEXT NOT NULL,
      model      TEXT NOT NULL,
      dim        INTEGER NOT NULL,
      vec        BLOB NOT NULL,
      hash       TEXT NOT NULL,
      created_at TEXT NOT NULL,
      PRIMARY KEY (ref_kind, ref_id)
    );

    -- Single-row worker heartbeat / last-run stats, surfaced in the GUI.
    CREATE TABLE IF NOT EXISTS worker_status (
      id         INTEGER PRIMARY KEY CHECK (id = 1),
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
      last_used_at TEXT
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
      used_at        TEXT
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

    -- Cross-platform identity mapping (Rust-branch addition): Discord/Telegram/
    -- Slack user ids → a people.slug.
    CREATE TABLE IF NOT EXISTS identities (
      id          TEXT PRIMARY KEY,
      platform    TEXT NOT NULL,
      platform_id TEXT NOT NULL,
      actor       TEXT NOT NULL,
      created_at  TEXT NOT NULL,
      UNIQUE (platform, platform_id)
    );
"#;

/// Unified full-text index — created separately because sqlx's prepared-statement
/// path can't batch a CREATE VIRTUAL TABLE with other statements.
const SCHEMA_FTS: &str = r#"
    CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
      kind UNINDEXED,
      ref_id UNINDEXED,
      title,
      body,
      tokenize = 'porter unicode61'
    );
"#;

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    // Was this a brand-new database? `journal` is the oldest core table, so its
    // absence before this migrate run means a genuinely fresh install (→ run
    // onboarding). A DB that predates v0.1.1 already has it (→ skip onboarding).
    let fresh = sqlx::query("SELECT 1 FROM sqlite_master WHERE type='table' AND name='journal'")
        .fetch_optional(pool)
        .await?
        .is_none();

    // Storage-format migration: embeddings.vec moved from JSON-text to packed
    // little-endian f32 BLOB. Drop a stale TEXT-format table so the worker
    // re-backfills it in the new format.
    let vec_col: Option<String> =
        sqlx::query_scalar("SELECT type FROM pragma_table_info('embeddings') WHERE name = 'vec'")
            .fetch_optional(pool)
            .await?;
    if let Some(t) = vec_col {
        if t.to_uppercase() != "BLOB" {
            sqlx::raw_sql("DROP TABLE embeddings").execute(pool).await?;
        }
    }

    sqlx::raw_sql(SCHEMA).execute(pool).await?;
    sqlx::raw_sql(SCHEMA_FTS).execute(pool).await?;

    // Idempotent column additions for DBs created before these columns existed.
    add_column_if_missing(
        pool,
        "sources",
        "owner",
        "ALTER TABLE sources ADD COLUMN owner TEXT",
    )
    .await?;
    add_column_if_missing(
        pool,
        "tasks",
        "phase",
        "ALTER TABLE tasks ADD COLUMN phase TEXT",
    )
    .await?;
    add_column_if_missing(
        pool,
        "tasks",
        "due",
        "ALTER TABLE tasks ADD COLUMN due TEXT",
    )
    .await?;
    add_column_if_missing(
        pool,
        "projects",
        "slug",
        "ALTER TABLE projects ADD COLUMN slug TEXT NOT NULL DEFAULT ''",
    )
    .await?;
    add_column_if_missing(
        pool,
        "people",
        "owner",
        "ALTER TABLE people ADD COLUMN owner TEXT",
    )
    .await?;
    add_column_if_missing(
        pool,
        "people",
        "bio",
        "ALTER TABLE people ADD COLUMN bio TEXT",
    )
    .await?;
    add_column_if_missing(
        pool,
        "people",
        "role",
        "ALTER TABLE people ADD COLUMN role TEXT",
    )
    .await?;
    // v0.1.2: OAuth tokens share the api_tokens table; existing PATs read as
    // kind NULL (treated as 'pat') with no expiry.
    for (col, ddl) in [
        ("kind", "ALTER TABLE api_tokens ADD COLUMN kind TEXT"),
        (
            "client_id",
            "ALTER TABLE api_tokens ADD COLUMN client_id TEXT",
        ),
        (
            "granted_by",
            "ALTER TABLE api_tokens ADD COLUMN granted_by TEXT",
        ),
        (
            "expires_at",
            "ALTER TABLE api_tokens ADD COLUMN expires_at TEXT",
        ),
        ("scope", "ALTER TABLE api_tokens ADD COLUMN scope TEXT"),
    ] {
        add_column_if_missing(pool, "api_tokens", col, ddl).await?;
    }

    // Onboarding gate: stamp the app version once; a fresh DB needs the wizard,
    // a DB that predates v0.1.1 is treated as already set up.
    let has_version: Option<String> =
        sqlx::query_scalar("SELECT value FROM config WHERE key = 'app.version'")
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

async fn add_column_if_missing(pool: &SqlitePool, table: &str, col: &str, ddl: &str) -> Result<()> {
    let has = sqlx::query("SELECT 1 FROM pragma_table_info(?) WHERE name = ?")
        .bind(table)
        .bind(col)
        .fetch_optional(pool)
        .await?;
    if has.is_none() {
        sqlx::raw_sql(ddl).execute(pool).await?;
    }
    Ok(())
}

async fn set_config(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
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

/// Open + migrate in one call (the boot path for both api and tests).
pub async fn init() -> Result<SqlitePool> {
    let path = db_path();
    let pool = open(&path).await?;
    migrate(&pool).await?;
    Ok(pool)
}

/// Verify the FTS5 module is actually available in the bundled SQLite.
pub async fn assert_fts5(pool: &SqlitePool) -> Result<()> {
    let n: i64 = sqlx::query("SELECT count(*) AS n FROM pragma_module_list WHERE name = 'fts5'")
        .fetch_one(pool)
        .await?
        .try_get("n")?;
    anyhow::ensure!(n > 0, "bundled SQLite lacks FTS5 — search cannot work");
    Ok(())
}
