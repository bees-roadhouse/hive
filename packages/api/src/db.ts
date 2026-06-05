import Database from "better-sqlite3";
import { APP_VERSION } from "@hive/shared";
import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// SQLite is the whole datastore — zero infra, spins up instantly in a fresh
// container. FTS5 (bundled in better-sqlite3) gives unified search across the
// journal and every structured entity that emerges from it.

// fileURLToPath (not URL.pathname) so Windows drive paths resolve correctly —
// `.pathname` yields "/C:/…" which resolve() then turns into "C:\C:\…".
const DB_PATH = process.env.HIVE_DB
  ? resolve(process.env.HIVE_DB)
  : fileURLToPath(new URL("../../../data/hive.db", import.meta.url));

mkdirSync(dirname(DB_PATH), { recursive: true });

export const db = new Database(DB_PATH);
db.pragma("journal_mode = WAL");
db.pragma("foreign_keys = ON");

export function migrate(): void {
  // Was this a brand-new database? `journal` is the oldest core table, so its
  // absence before this migrate run means a genuinely fresh install (→ run
  // onboarding). A DB that predates v0.1.1 already has it (→ skip onboarding).
  const fresh = !db
    .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='journal'")
    .get();

  // Storage-format migration: embeddings.vec moved from JSON-text to packed
  // little-endian f32 BLOB (matching bookstack-mcp). Drop a stale TEXT-format
  // table so the worker re-backfills it in the new format. Runs once — after
  // recreation the column type is BLOB and this is a no-op.
  const vecCol = db
    .prepare("SELECT type FROM pragma_table_info('embeddings') WHERE name = 'vec'")
    .get() as { type: string } | undefined;
  if (vecCol && vecCol.type.toUpperCase() !== "BLOB") db.exec("DROP TABLE embeddings");

  db.exec(`
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

    CREATE TABLE IF NOT EXISTS people (
      id         TEXT PRIMARY KEY,
      name       TEXT NOT NULL,
      slug       TEXT NOT NULL UNIQUE,
      kind       TEXT NOT NULL DEFAULT 'human',
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

    -- Unified full-text index across journal + every structured kind.
    CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
      kind UNINDEXED,
      ref_id UNINDEXED,
      title,
      body,
      tokenize = 'porter unicode61'
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

    -- Local embeddings for semantic search (vector stored as JSON float array).
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
    -- kind='ai' rows carry owner (a human slug) for visibility scoping.
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
    -- scope='entry' → ref is a journal entry id (shared with viewer).
    -- scope='journal' → ref is an author slug (viewer sees all entries by that author).
    CREATE TABLE IF NOT EXISTS shares (
      id         TEXT PRIMARY KEY,
      scope      TEXT NOT NULL,
      ref        TEXT NOT NULL,
      viewer     TEXT NOT NULL,
      created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS shares_viewer ON shares (viewer, scope);
    CREATE UNIQUE INDEX IF NOT EXISTS shares_uniq ON shares (scope, ref, viewer);

    -- Key/value instance config (v0.1.1). Holds app.version, onboarding.completed,
    -- instance.name, and anything else that's per-deployment rather than content.
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
    -- token_hash = sha256(plaintext). The plaintext is returned once at creation.
    CREATE TABLE IF NOT EXISTS api_tokens (
      id           TEXT PRIMARY KEY,
      token_hash   TEXT NOT NULL UNIQUE,
      actor        TEXT NOT NULL,
      label        TEXT NOT NULL,
      created_by   TEXT NOT NULL,
      created_at   TEXT NOT NULL,
      last_used_at TEXT
    );

    -- OAuth 2.1 dynamic client registration (RFC 7591). MCP clients are public
    -- (PKCE, no secret), so we store no client_secret.
    CREATE TABLE IF NOT EXISTS oauth_clients (
      client_id     TEXT PRIMARY KEY,
      client_name   TEXT NOT NULL,
      redirect_uris TEXT NOT NULL,   -- JSON array of exact-match strings
      grant_types   TEXT NOT NULL,   -- JSON array
      created_at    TEXT NOT NULL
    );

    -- OAuth authorization codes: single-use, short TTL, hashed at rest, bound to
    -- the granted AI identity + the granting human.
    CREATE TABLE IF NOT EXISTS oauth_auth_codes (
      code_hash      TEXT PRIMARY KEY,   -- sha256(plaintext code)
      client_id      TEXT NOT NULL,
      redirect_uri   TEXT NOT NULL,
      code_challenge TEXT NOT NULL,      -- PKCE S256 challenge
      ai_actor       TEXT NOT NULL,      -- people.slug, kind='ai'
      granted_by     TEXT NOT NULL,      -- granting human's actor
      scope          TEXT NOT NULL,
      created_at     TEXT NOT NULL,
      expires_at     TEXT NOT NULL,
      used_at        TEXT                -- single-use marker; non-null = spent
    );
    CREATE INDEX IF NOT EXISTS oauth_codes_expiry ON oauth_auth_codes (expires_at);
  `);

  // Idempotent column additions for DBs created before owner was introduced.
  // Must run after the CREATE TABLE block so the table exists on fresh DBs.
  const hasOwner = db
    .prepare("SELECT 1 FROM pragma_table_info('sources') WHERE name='owner'")
    .get();
  if (!hasOwner) {
    db.exec("ALTER TABLE sources ADD COLUMN owner TEXT");
  }

  const hasTaskPhase = db
    .prepare("SELECT 1 FROM pragma_table_info('tasks') WHERE name='phase'")
    .get();
  if (!hasTaskPhase) {
    db.exec("ALTER TABLE tasks ADD COLUMN phase TEXT");
  }

  const hasTaskDue = db
    .prepare("SELECT 1 FROM pragma_table_info('tasks') WHERE name='due'")
    .get();
  if (!hasTaskDue) {
    db.exec("ALTER TABLE tasks ADD COLUMN due TEXT");
  }

  const hasProjectSlug = db
    .prepare("SELECT 1 FROM pragma_table_info('projects') WHERE name='slug'")
    .get();
  if (!hasProjectSlug) {
    db.exec("ALTER TABLE projects ADD COLUMN slug TEXT NOT NULL DEFAULT ''");
  }

  // people.owner — guard for DBs bootstrapped before this column was added.
  const hasPeopleOwner = db
    .prepare("SELECT 1 FROM pragma_table_info('people') WHERE name='owner'")
    .get();
  if (!hasPeopleOwner) {
    db.exec("ALTER TABLE people ADD COLUMN owner TEXT");
  }

  // Identity profile (bio + role), editable in the UI and self-updatable via MCP.
  for (const col of ["bio", "role"]) {
    const has = db.prepare("SELECT 1 FROM pragma_table_info('people') WHERE name=?").get(col);
    if (!has) db.exec(`ALTER TABLE people ADD COLUMN ${col} TEXT`);
  }

  // v0.1.2: OAuth tokens share the api_tokens table so tokens.resolve keeps
  // returning an actor for every consumer. New columns are nullable; an existing
  // PAT reads as kind NULL (treated as 'pat') with no expiry.
  for (const [col, ddl] of [
    ["kind", "ALTER TABLE api_tokens ADD COLUMN kind TEXT"],
    ["client_id", "ALTER TABLE api_tokens ADD COLUMN client_id TEXT"],
    ["granted_by", "ALTER TABLE api_tokens ADD COLUMN granted_by TEXT"],
    ["expires_at", "ALTER TABLE api_tokens ADD COLUMN expires_at TEXT"],
    ["scope", "ALTER TABLE api_tokens ADD COLUMN scope TEXT"],
  ] as const) {
    const has = db.prepare("SELECT 1 FROM pragma_table_info('api_tokens') WHERE name=?").get(col);
    if (!has) db.exec(ddl);
  }

  // ---- v0.1.1 onboarding gate ----
  // The first time this schema initializes a DB, stamp the app version and
  // decide whether onboarding is required. A fresh DB needs the wizard; a DB
  // that predates v0.1.1 (already had `journal`) is treated as already set up,
  // so existing deployments never get bounced through onboarding.
  const cfg = (key: string): string | undefined =>
    (db.prepare("SELECT value FROM config WHERE key = ?").get(key) as { value: string } | undefined)?.value;
  const setCfg = (key: string, value: string): void => {
    db.prepare(
      "INSERT INTO config (key, value, updated_at) VALUES (?, ?, ?) " +
        "ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    ).run(key, value, new Date().toISOString());
  };
  if (!cfg("app.version")) {
    setCfg("app.version", APP_VERSION);
    setCfg("onboarding.completed", fresh ? "false" : "true");
  }
}

/** Wrap a unit of work in a transaction. */
export function tx<T>(fn: () => T): T {
  return db.transaction(fn)();
}
