import Database from "better-sqlite3";
import { mkdirSync } from "node:fs";
import { dirname, resolve } from "node:path";

// SQLite is the whole datastore — zero infra, spins up instantly in a fresh
// container. The rust hive moved to postgres+pgvector for production scale;
// this fun rewrite keeps it to a single file. FTS5 (bundled in better-sqlite3)
// gives us the hybrid-search-lite that hive_search.py used to provide.

const DB_PATH = resolve(
  process.env.HIVE_DB ?? new URL("../../../data/hive.db", import.meta.url).pathname,
);

mkdirSync(dirname(DB_PATH), { recursive: true });

export const db = new Database(DB_PATH);
db.pragma("journal_mode = WAL");
db.pragma("foreign_keys = ON");

export function migrate(): void {
  db.exec(`
    CREATE TABLE IF NOT EXISTS projects (
      id         TEXT PRIMARY KEY,
      name       TEXT NOT NULL UNIQUE,
      created_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS tasks (
      id         TEXT PRIMARY KEY,
      project    TEXT,
      title      TEXT NOT NULL,
      body       TEXT NOT NULL DEFAULT '',
      status     TEXT NOT NULL DEFAULT 'todo',
      priority   TEXT NOT NULL DEFAULT 'normal',
      tags       TEXT NOT NULL DEFAULT '[]',
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS journal (
      id         TEXT PRIMARY KEY,
      project    TEXT,
      body       TEXT NOT NULL,
      tags       TEXT NOT NULL DEFAULT '[]',
      created_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS notes (
      id         TEXT PRIMARY KEY,
      title      TEXT NOT NULL,
      body       TEXT NOT NULL DEFAULT '',
      tags       TEXT NOT NULL DEFAULT '[]',
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS decisions (
      id           TEXT PRIMARY KEY,
      title        TEXT NOT NULL,
      context      TEXT NOT NULL DEFAULT '',
      decision     TEXT NOT NULL,
      consequences TEXT NOT NULL DEFAULT '',
      status       TEXT NOT NULL DEFAULT 'proposed',
      project      TEXT,
      supersedes   TEXT,
      tags         TEXT NOT NULL DEFAULT '[]',
      created_at   TEXT NOT NULL,
      updated_at   TEXT NOT NULL
    );

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

    -- Unified full-text index across the three writable entity kinds.
    CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
      kind UNINDEXED,
      ref_id UNINDEXED,
      title,
      body,
      tokenize = 'porter unicode61'
    );
  `);
}

/** Wrap a unit of work in a transaction. */
export function tx<T>(fn: () => T): T {
  return db.transaction(fn)();
}
