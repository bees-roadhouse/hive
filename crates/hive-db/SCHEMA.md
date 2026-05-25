# hive-db schema

Frozen against the post-task-8 `~/.hive/hive.db` (Cera shipped task 8 on
2026-05-15: `projects.id INTEGER PRIMARY KEY AUTOINCREMENT`,
`projects.name UNIQUE`).

This file is the human-readable companion to `src/schema.rs`. Both must agree;
when they don't, `src/schema.rs` is the source of truth and this file gets
fixed in the same PR.

## Tables

### `projects`

Project namespace for tasks and notes.

| col | type | notes |
|---|---|---|
| `id` | `INTEGER PK AUTOINCREMENT` | task-8 added; primary key |
| `name` | `TEXT NOT NULL UNIQUE` | display name; foreign-key target for tasks/notes |
| `description` | `TEXT` | nullable |
| `status` | `TEXT NOT NULL DEFAULT 'active'` | active / paused / archived |
| `owner` | `TEXT NOT NULL` | one of pia / apis / cera / nate / maggie |
| `created_at`, `updated_at` | `TEXT NOT NULL DEFAULT (datetime('now'))` | sqlite ISO-8601 |

### `tasks`

| col | type | notes |
|---|---|---|
| `id` | `INTEGER PK AUTOINCREMENT` | |
| `project` | `TEXT NOT NULL REFERENCES projects(name)` | FK on `name`, not `id` |
| `title` | `TEXT NOT NULL` | |
| `body` | `TEXT` | nullable |
| `owner` | `TEXT NOT NULL` | one of pia / apis / cera / nate / maggie |
| `status` | `TEXT NOT NULL DEFAULT 'open'` | open / in_progress / blocked / done / dropped |
| `priority` | `TEXT` | nullable; free-form |
| `due` | `TEXT` | nullable; YYYY-MM-DD |
| `block_reason` | `TEXT` | nullable; required when status=blocked |
| `created_at`, `updated_at` | `TEXT NOT NULL DEFAULT (datetime('now'))` | |
| `closed_at` | `TEXT` | set when status moves to done/dropped |

Indexes: `idx_tasks_project (project)`, `idx_tasks_owner (owner)`,
`idx_tasks_status (status)`.

### `journal_entries`

Per-AI chronological memory.

| col | type | notes |
|---|---|---|
| `id` | `INTEGER PK AUTOINCREMENT` | |
| `ai` | `TEXT NOT NULL` | one of pia / apis / cera / nate |
| `entry_date` | `TEXT NOT NULL` | YYYY-MM-DD |
| `title` | `TEXT` | nullable |
| `body` | `TEXT NOT NULL` | |
| `tags` | `TEXT` | comma-separated |
| `created_at`, `updated_at` | `TEXT NOT NULL DEFAULT (datetime('now'))` | |

Indexes: `idx_journal_ai_date (ai, entry_date DESC)`,
`idx_journal_date (entry_date DESC)`.

### `notes`

Free-form snippets, optionally project-attached.

| col | type | notes |
|---|---|---|
| `id` | `INTEGER PK AUTOINCREMENT` | |
| `author` | `TEXT NOT NULL` | one of pia / apis / cera / nate / maggie |
| `title` | `TEXT` | nullable |
| `body` | `TEXT NOT NULL` | |
| `tags` | `TEXT` | comma-separated |
| `project` | `TEXT REFERENCES projects(name)` | nullable |
| `created_at`, `updated_at` | `TEXT NOT NULL DEFAULT (datetime('now'))` | |

Indexes: `idx_notes_project (project)`, `idx_notes_author (author)`.

### `wire_events`

watch-the-wire CVE / outage / news cache.

| col | type | notes |
|---|---|---|
| `id` | `INTEGER PK AUTOINCREMENT` | |
| `source` | `TEXT NOT NULL` | feed/source identifier |
| `category` | `TEXT` | nullable |
| `external_id` | `TEXT UNIQUE` | nullable; dedupe key |
| `title` | `TEXT NOT NULL` | |
| `body` | `TEXT` | nullable |
| `url` | `TEXT` | nullable |
| `severity` | `TEXT` | one of critical / high / medium / low / info |
| `affects` | `TEXT` | nullable; affected stack |
| `acknowledged` | `INTEGER NOT NULL DEFAULT 0` | 0/1 |
| `pinged_discord` | `INTEGER NOT NULL DEFAULT 0` | 0/1 |
| `first_seen_at`, `last_seen_at` | `TEXT NOT NULL DEFAULT (datetime('now'))` | |

Indexes: `idx_wire_source (source)`, `idx_wire_severity (severity)`,
`idx_wire_seen (last_seen_at DESC)`.

### `links`

Cross-domain edges between hive entities.

| col | type | notes |
|---|---|---|
| `id` | `INTEGER PK AUTOINCREMENT` | |
| `source_table` | `TEXT NOT NULL` | one of tasks, journal_entries, notes, wire_events, projects |
| `source_id` | `INTEGER NOT NULL` | row id (or projects.id since task 8) |
| `target_table` | `TEXT NOT NULL` | same domain |
| `target_id` | `INTEGER NOT NULL` | row id |
| `link_type` | `TEXT` | nullable; free-form |
| `note` | `TEXT` | nullable |
| `created_at` | `TEXT NOT NULL DEFAULT (datetime('now'))` | |
| `UNIQUE (source_table, source_id, target_table, target_id, link_type)` | | dedupe |

Indexes: `idx_links_source (source_table, source_id)`,
`idx_links_target (target_table, target_id)`, `idx_links_type (link_type)`.

### `embeddings`

Vector store for hybrid search (per-source-row, per-model).

| col | type | notes |
|---|---|---|
| `id` | `INTEGER PK AUTOINCREMENT` | |
| `source_table` | `TEXT NOT NULL` | journal_entries or notes |
| `source_id` | `INTEGER NOT NULL` | row id |
| `model` | `TEXT NOT NULL` | model tag (e.g. `bge-small-en-v1.5`) |
| `dim` | `INTEGER NOT NULL` | vector dimension (384 for bge-small) |
| `embedding` | `BLOB NOT NULL` | raw little-endian f32 bytes |
| `content_hash` | `TEXT NOT NULL` | sha256 of `title \|\| body \|\| tags`; staleness key |
| `created_at` | `TEXT NOT NULL DEFAULT (datetime('now'))` | |
| `UNIQUE (source_table, source_id, model)` | | one embedding per row per model |

Indexes: `idx_embeddings_source (source_table, source_id)`,
`idx_embeddings_model (model)`.

## FTS5 virtual tables + triggers

`journal_fts` and `notes_fts` are FTS5 virtual tables over the `(title, body,
tags)` columns of their respective parents (`content=`, `content_rowid=id`).

Triggers keep them in sync:
- `journal_ai` / `notes_ai`: AFTER INSERT, mirror the new row into FTS.
- `journal_ad` / `notes_ad`: AFTER DELETE, send a `'delete'` op to FTS.
- `journal_au` / `notes_au`: AFTER UPDATE, delete-then-insert into FTS.

The CREATE TABLE / CREATE TRIGGER statements are embedded in `src/schema.rs`.
They use `IF NOT EXISTS` so `hive init` is idempotent against an existing
hive.db.

## Conventions

- All timestamps are sqlite's `datetime('now')` ... ISO-8601 in UTC, second
  precision. The python tooling treats these as opaque strings; rust does the
  same. Don't parse to `chrono::DateTime` unless a query needs it.
- Validation (owner, ai, severity, status) lives in the **types** layer, not
  the schema. Sqlite has no enum constraints; the rust types hold the closed
  set and reject invalid values at parse time.
- Foreign keys are ON. Connections must `PRAGMA foreign_keys = ON` ... see
  `src/pool.rs`.
- Hive-rs targets the schema as-is. Schema migrations are out of scope for
  v1; if a future change is needed, write a one-shot script the way the
  python migrations did.
