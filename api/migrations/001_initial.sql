-- Hive v0.2.0 Rust schema — journal-first memory system

-- People (writers; human or ai)
CREATE TABLE IF NOT EXISTS people (
    id          TEXT PRIMARY KEY,
    slug        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('human', 'ai')),
    owner       TEXT,
    bio         TEXT,
    role        TEXT,
    created_at  TEXT NOT NULL
);

-- External identity mappings: platform user IDs → centralized actor
CREATE TABLE IF NOT EXISTS identities (
    id          TEXT PRIMARY KEY,
    platform    TEXT NOT NULL,
    platform_id TEXT NOT NULL,
    actor       TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_identities_platform ON identities (platform, platform_id);
CREATE INDEX IF NOT EXISTS idx_identities_actor ON identities (actor);

-- Profile cards (mutable per-actor identity store)
CREATE TABLE IF NOT EXISTS profile (
    actor           TEXT PRIMARY KEY,
    kind            TEXT NOT NULL,
    display_name    TEXT NOT NULL,
    body            TEXT NOT NULL DEFAULT '{}',
    source          TEXT NOT NULL DEFAULT 'manual',
    derived_at      TEXT,
    updated_at      TEXT NOT NULL
);

-- Projects
CREATE TABLE IF NOT EXISTS projects (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    slug        TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL
);

-- Phases (columns within a project)
CREATE TABLE IF NOT EXISTS phases (
    id          TEXT PRIMARY KEY,
    project     TEXT NOT NULL,
    name        TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);

-- Journal entries (the source of truth)
CREATE TABLE IF NOT EXISTS journal (
    id          TEXT PRIMARY KEY,
    author      TEXT NOT NULL,
    body        TEXT NOT NULL,
    tags        TEXT NOT NULL DEFAULT '[]',
    mentions    TEXT NOT NULL DEFAULT '[]',
    created_at  TEXT NOT NULL
);

-- Anchors (spans of text that produced structured entities)
CREATE TABLE IF NOT EXISTS anchors (
    id          TEXT PRIMARY KEY,
    entry_id    TEXT NOT NULL,
    start       INTEGER NOT NULL,
    "end"       INTEGER NOT NULL,
    text        TEXT NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('task', 'decision', 'event')),
    ref_id      TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_anchors_entry ON anchors (entry_id);
CREATE INDEX IF NOT EXISTS idx_anchors_ref ON anchors (ref_id);

-- Tasks
CREATE TABLE IF NOT EXISTS tasks (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    body            TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'todo' CHECK (status IN ('todo', 'doing', 'blocked', 'done')),
    priority        TEXT NOT NULL DEFAULT 'normal' CHECK (priority IN ('low', 'normal', 'high', 'urgent')),
    tags            TEXT NOT NULL DEFAULT '[]',
    assignees       TEXT NOT NULL DEFAULT '[]',
    project         TEXT,
    phase           TEXT,
    due             TEXT,
    origin_entry_id TEXT,
    anchor_text     TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks (project);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks (status);
CREATE INDEX IF NOT EXISTS idx_tasks_assignees ON tasks (assignees);

-- Decisions
CREATE TABLE IF NOT EXISTS decisions (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    context         TEXT NOT NULL,
    decision        TEXT NOT NULL,
    consequences    TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'proposed' CHECK (status IN ('proposed', 'accepted', 'rejected', 'superseded')),
    tags            TEXT NOT NULL DEFAULT '[]',
    assignees       TEXT NOT NULL DEFAULT '[]',
    project         TEXT,
    supersedes      TEXT,
    origin_entry_id TEXT,
    anchor_text     TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- Events
CREATE TABLE IF NOT EXISTS events (
    id              TEXT PRIMARY KEY,
    title           TEXT NOT NULL,
    body            TEXT NOT NULL,
    at              TEXT,
    tags            TEXT NOT NULL DEFAULT '[]',
    assignees       TEXT NOT NULL DEFAULT '[]',
    origin_entry_id TEXT,
    anchor_text     TEXT,
    created_at      TEXT NOT NULL
);

-- Links (relationships between entities)
CREATE TABLE IF NOT EXISTS links (
    id          TEXT PRIMARY KEY,
    source_kind TEXT NOT NULL,
    source_id   TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_id   TEXT NOT NULL,
    rel         TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_links_source ON links (source_kind, source_id);
CREATE INDEX IF NOT EXISTS idx_links_target ON links (target_kind, target_id);

-- Inbox (per-actor notifications)
CREATE TABLE IF NOT EXISTS inbox (
    id          TEXT PRIMARY KEY,
    recipient   TEXT NOT NULL,
    "from"      TEXT NOT NULL,
    reason      TEXT NOT NULL CHECK (reason IN ('mention', 'assignment', 'decision', 'event')),
    ref_kind    TEXT NOT NULL,
    ref_id      TEXT NOT NULL,
    entry_id    TEXT,
    snippet     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    read_at     TEXT
);
CREATE INDEX IF NOT EXISTS idx_inbox_recipient ON inbox (recipient);
CREATE INDEX IF NOT EXISTS idx_inbox_unread ON inbox (recipient, read_at) WHERE read_at IS NULL;

-- Shares (access grants)
CREATE TABLE IF NOT EXISTS shares (
    id          TEXT PRIMARY KEY,
    scope       TEXT NOT NULL CHECK (scope IN ('entry', 'journal')),
    ref         TEXT NOT NULL,
    viewer      TEXT NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_shares_unique ON shares (scope, ref, viewer);

-- Search index (FTS5)
CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
    kind,
    ref_id,
    title,
    body,
    content='',
    content_rowid=rowid
);

-- Embeddings (vector storage for semantic search)
CREATE TABLE IF NOT EXISTS embeddings (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    ref_id      TEXT NOT NULL,
    model       TEXT NOT NULL,
    dim         INTEGER NOT NULL,
    embedding   BLOB NOT NULL,
    created_at  TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_embeddings_ref ON embeddings (kind, ref_id, model);

-- Wire log (event log for SSE fan-out)
CREATE TABLE IF NOT EXISTS wire (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    actor       TEXT NOT NULL,
    payload     TEXT NOT NULL,
    created_at  TEXT NOT NULL
);

-- Sources (RSS/feeds to poll)
CREATE TABLE IF NOT EXISTS sources (
    id          TEXT PRIMARY KEY,
    slug        TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    url         TEXT NOT NULL,
    kind        TEXT NOT NULL,
    interval_min INTEGER NOT NULL DEFAULT 60,
    last_run_at TEXT,
    last_error  TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);

-- Topics (auto-extracted from journal)
CREATE TABLE IF NOT EXISTS topics (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    slug        TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL
);

-- Users (login accounts)
CREATE TABLE IF NOT EXISTS users (
    id              TEXT PRIMARY KEY,
    actor           TEXT NOT NULL UNIQUE,
    email           TEXT NOT NULL UNIQUE,
    name            TEXT NOT NULL,
    role            TEXT NOT NULL DEFAULT 'member' CHECK (role IN ('admin', 'member')),
    password_hash   TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    last_login_at   TEXT
);

-- Sessions (browser cookie auth)
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    token_hash  TEXT NOT NULL,
    user_id     TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    last_seen   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_sessions_token ON sessions (token_hash);

-- API tokens (programmatic auth)
CREATE TABLE IF NOT EXISTS api_tokens (
    id          TEXT PRIMARY KEY,
    actor       TEXT NOT NULL,
    label       TEXT NOT NULL,
    created_by  TEXT NOT NULL,
    created_at  TEXT NOT NULL,
    last_used_at TEXT,
    kind        TEXT,
    client_id   TEXT,
    granted_by  TEXT,
    scope       TEXT,
    expires_at  TEXT
);
CREATE INDEX IF NOT EXISTS idx_tokens_actor ON api_tokens (actor);

-- OAuth clients (dynamic registration)
CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id       TEXT PRIMARY KEY,
    client_name     TEXT NOT NULL,
    redirect_uris   TEXT NOT NULL,
    grant_types     TEXT NOT NULL DEFAULT '["authorization_code"]',
    created_at      TEXT NOT NULL
);

-- OAuth authorization codes
CREATE TABLE IF NOT EXISTS oauth_codes (
    code            TEXT PRIMARY KEY,
    client_id       TEXT NOT NULL,
    redirect_uri    TEXT NOT NULL,
    code_challenge  TEXT NOT NULL,
    ai_actor        TEXT NOT NULL,
    granted_by      TEXT NOT NULL,
    scope           TEXT NOT NULL DEFAULT 'mcp',
    created_at      TEXT NOT NULL,
    expires_at      TEXT NOT NULL,
    used_at         TEXT
);

-- Config (key/value instance settings)
CREATE TABLE IF NOT EXISTS config (
    key         TEXT PRIMARY KEY,
    value       TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
