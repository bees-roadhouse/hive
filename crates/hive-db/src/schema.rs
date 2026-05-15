//! Canonical CREATE statements for the hive DB.
//!
//! Verbatim port of the python `SCHEMA` constant in `~/.hive/hive.py`,
//! frozen against the post-task-8 layout (projects has INTEGER PK id +
//! UNIQUE name).
//!
//! All statements use `IF NOT EXISTS` so `hive init` is idempotent and safe
//! to run against an existing DB.

pub const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS projects (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT NOT NULL UNIQUE,
  description TEXT,
  status TEXT NOT NULL DEFAULT 'active',
  owner TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS tasks (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  project TEXT NOT NULL REFERENCES projects(name),
  title TEXT NOT NULL,
  body TEXT,
  owner TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',
  priority TEXT,
  due TEXT,
  block_reason TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now')),
  closed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_tasks_project ON tasks(project);
CREATE INDEX IF NOT EXISTS idx_tasks_owner ON tasks(owner);
CREATE INDEX IF NOT EXISTS idx_tasks_status ON tasks(status);

CREATE TABLE IF NOT EXISTS journal_entries (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  ai TEXT NOT NULL,
  entry_date TEXT NOT NULL,
  title TEXT,
  body TEXT NOT NULL,
  tags TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_journal_ai_date ON journal_entries(ai, entry_date DESC);
CREATE INDEX IF NOT EXISTS idx_journal_date ON journal_entries(entry_date DESC);

CREATE TABLE IF NOT EXISTS notes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  author TEXT NOT NULL,
  title TEXT,
  body TEXT NOT NULL,
  tags TEXT,
  project TEXT REFERENCES projects(name),
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_notes_project ON notes(project);
CREATE INDEX IF NOT EXISTS idx_notes_author ON notes(author);

CREATE TABLE IF NOT EXISTS wire_events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source TEXT NOT NULL,
  category TEXT,
  external_id TEXT UNIQUE,
  title TEXT NOT NULL,
  body TEXT,
  url TEXT,
  severity TEXT,
  affects TEXT,
  acknowledged INTEGER NOT NULL DEFAULT 0,
  pinged_discord INTEGER NOT NULL DEFAULT 0,
  first_seen_at TEXT NOT NULL DEFAULT (datetime('now')),
  last_seen_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_wire_source ON wire_events(source);
CREATE INDEX IF NOT EXISTS idx_wire_severity ON wire_events(severity);
CREATE INDEX IF NOT EXISTS idx_wire_seen ON wire_events(last_seen_at DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS journal_fts USING fts5(title, body, tags, content=journal_entries, content_rowid=id);
CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(title, body, tags, content=notes, content_rowid=id);

CREATE TRIGGER IF NOT EXISTS journal_ai AFTER INSERT ON journal_entries BEGIN
  INSERT INTO journal_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS journal_ad AFTER DELETE ON journal_entries BEGIN
  INSERT INTO journal_fts(journal_fts, rowid, title, body, tags) VALUES('delete', old.id, old.title, old.body, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS journal_au AFTER UPDATE ON journal_entries BEGIN
  INSERT INTO journal_fts(journal_fts, rowid, title, body, tags) VALUES('delete', old.id, old.title, old.body, old.tags);
  INSERT INTO journal_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS notes_ai AFTER INSERT ON notes BEGIN
  INSERT INTO notes_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS notes_ad AFTER DELETE ON notes BEGIN
  INSERT INTO notes_fts(notes_fts, rowid, title, body, tags) VALUES('delete', old.id, old.title, old.body, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS notes_au AFTER UPDATE ON notes BEGIN
  INSERT INTO notes_fts(notes_fts, rowid, title, body, tags) VALUES('delete', old.id, old.title, old.body, old.tags);
  INSERT INTO notes_fts(rowid, title, body, tags) VALUES (new.id, new.title, new.body, new.tags);
END;

CREATE TABLE IF NOT EXISTS links (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_table TEXT NOT NULL,
  source_id INTEGER NOT NULL,
  target_table TEXT NOT NULL,
  target_id INTEGER NOT NULL,
  link_type TEXT,
  note TEXT,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (source_table, source_id, target_table, target_id, link_type)
);
CREATE INDEX IF NOT EXISTS idx_links_source ON links(source_table, source_id);
CREATE INDEX IF NOT EXISTS idx_links_target ON links(target_table, target_id);
CREATE INDEX IF NOT EXISTS idx_links_type ON links(link_type);

CREATE TABLE IF NOT EXISTS embeddings (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  source_table TEXT NOT NULL,
  source_id INTEGER NOT NULL,
  model TEXT NOT NULL,
  dim INTEGER NOT NULL,
  embedding BLOB NOT NULL,
  content_hash TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (datetime('now')),
  UNIQUE (source_table, source_id, model)
);
CREATE INDEX IF NOT EXISTS idx_embeddings_source ON embeddings(source_table, source_id);
CREATE INDEX IF NOT EXISTS idx_embeddings_model ON embeddings(model);
"#;
