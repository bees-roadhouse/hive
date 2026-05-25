-- pgvector for embeddings
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE projects (
  id BIGSERIAL PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  description TEXT,
  status TEXT NOT NULL DEFAULT 'active',
  owner TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tasks (
  id BIGSERIAL PRIMARY KEY,
  project TEXT REFERENCES projects(name),
  title TEXT NOT NULL,
  body TEXT,
  owner TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'open',
  priority TEXT,
  due TEXT,
  block_reason TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  closed_at TIMESTAMPTZ
);
CREATE INDEX idx_tasks_project ON tasks(project);
CREATE INDEX idx_tasks_owner ON tasks(owner);
CREATE INDEX idx_tasks_status ON tasks(status);

CREATE TABLE journal_entries (
  id BIGSERIAL PRIMARY KEY,
  ai TEXT NOT NULL,
  entry_date TEXT NOT NULL,
  title TEXT,
  body TEXT NOT NULL,
  tags TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  fts tsvector GENERATED ALWAYS AS (
    to_tsvector('english', coalesce(title,'') || ' ' || coalesce(body,'') || ' ' || coalesce(tags,''))
  ) STORED
);
CREATE INDEX idx_journal_ai_date ON journal_entries(ai, entry_date DESC);
CREATE INDEX idx_journal_date ON journal_entries(entry_date DESC);
CREATE INDEX idx_journal_fts ON journal_entries USING GIN (fts);

CREATE TABLE notes (
  id BIGSERIAL PRIMARY KEY,
  author TEXT NOT NULL,
  title TEXT,
  body TEXT NOT NULL,
  tags TEXT,
  project TEXT REFERENCES projects(name),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  fts tsvector GENERATED ALWAYS AS (
    to_tsvector('english', coalesce(title,'') || ' ' || coalesce(body,'') || ' ' || coalesce(tags,''))
  ) STORED
);
CREATE INDEX idx_notes_project ON notes(project);
CREATE INDEX idx_notes_author ON notes(author);
CREATE INDEX idx_notes_fts ON notes USING GIN (fts);

CREATE TABLE wire_events (
  id BIGSERIAL PRIMARY KEY,
  source TEXT NOT NULL,
  category TEXT,
  external_id TEXT UNIQUE,
  title TEXT NOT NULL,
  body TEXT,
  url TEXT,
  severity TEXT,
  affects TEXT,
  acknowledged BOOLEAN NOT NULL DEFAULT false,
  pinged_discord BOOLEAN NOT NULL DEFAULT false,
  first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_wire_source ON wire_events(source);
CREATE INDEX idx_wire_severity ON wire_events(severity);
CREATE INDEX idx_wire_seen ON wire_events(last_seen_at DESC);

CREATE TABLE messages (
  id BIGSERIAL PRIMARY KEY,
  sender_ai TEXT NOT NULL,
  recipient_ai TEXT NOT NULL,
  kind TEXT,
  body TEXT NOT NULL,
  in_reply_to BIGINT REFERENCES messages(id),
  sent_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  read_at TIMESTAMPTZ,
  fts tsvector GENERATED ALWAYS AS (to_tsvector('english', body)) STORED
);
CREATE INDEX idx_messages_recipient ON messages(recipient_ai);
CREATE INDEX idx_messages_sender ON messages(sender_ai);
CREATE INDEX idx_messages_sent ON messages(sent_at DESC);
CREATE INDEX idx_messages_reply ON messages(in_reply_to);
CREATE INDEX idx_messages_fts ON messages USING GIN (fts);

CREATE TABLE links (
  id BIGSERIAL PRIMARY KEY,
  source_table TEXT NOT NULL,
  source_id BIGINT NOT NULL,
  target_table TEXT NOT NULL,
  target_id BIGINT NOT NULL,
  link_type TEXT,
  note TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (source_table, source_id, target_table, target_id, link_type)
);
CREATE INDEX idx_links_source ON links(source_table, source_id);
CREATE INDEX idx_links_target ON links(target_table, target_id);
CREATE INDEX idx_links_type ON links(link_type);

CREATE TABLE embeddings (
  id BIGSERIAL PRIMARY KEY,
  source_table TEXT NOT NULL,
  source_id BIGINT NOT NULL,
  model TEXT NOT NULL,
  dim INTEGER NOT NULL,
  embedding vector,
  content_hash TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (source_table, source_id, model)
);
CREATE INDEX idx_embeddings_source ON embeddings(source_table, source_id);
CREATE INDEX idx_embeddings_model ON embeddings(model);
