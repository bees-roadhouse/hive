-- UUIDv7 PKs across every hive-db table from 0001. Pure RFC-9562 layout,
-- time-basis from each row's existing created_at (or first_seen_at / sent_at
-- for tables that lack created_at). No display_id sidecar.
--
-- Strategy in this single transaction:
--   1. Define a postgres-side gen_uuid_v7() that takes an optional timestamp.
--   2. Add new uuid columns (uid + per-FK *_uid) alongside the existing
--      BIGINT shape, backfill from the per-row causal timestamp.
--   3. Drop old FK + PK constraints; drop old integer id columns; rename the
--      uuid columns into place.
--   4. Re-add primary keys + foreign keys on the new uuid columns.
--
-- All inside BEGIN/COMMIT so a failure rolls the schema back to the previous
-- (post-0001) shape.

BEGIN;

-- ---------------------------------------------------------------------------
-- 0. UUIDv7 generator. RFC 9562 layout:
--      48-bit unix_ms | 4-bit ver=7 | 12-bit rand_a |
--       2-bit var=10 | 62-bit rand_b
--    pgcrypto's gen_random_bytes gives us the entropy; bit twiddling sets
--    the version + variant nibbles. App code generates UUIDv7s client-side
--    via the rust `uuid` crate (v7 feature); this function is here for
--    hand-written INSERTs (psql, ad-hoc tooling) and the backfill below.
-- ---------------------------------------------------------------------------

CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE OR REPLACE FUNCTION gen_uuid_v7(p_ts timestamptz DEFAULT clock_timestamp())
RETURNS uuid
LANGUAGE plpgsql
AS $$
DECLARE
  unix_ms bigint;
  ts_hex  text;
  rand    bytea;
  b6      int;
  b8      int;
BEGIN
  unix_ms := floor(extract(epoch from p_ts) * 1000)::bigint;
  ts_hex  := lpad(to_hex(unix_ms), 12, '0');

  rand := gen_random_bytes(10);
  b6 := (get_byte(rand, 0) & 15) | 112;       -- version 7 in high nibble of byte 6
  rand := set_byte(rand, 0, b6);
  b8 := (get_byte(rand, 2) & 63) | 128;       -- variant 10 in top two bits of byte 8
  rand := set_byte(rand, 2, b8);

  RETURN (
    substr(ts_hex, 1, 8)  || '-' ||
    substr(ts_hex, 9, 4)  || '-' ||
    encode(substring(rand from 1 for 2), 'hex') || '-' ||
    encode(substring(rand from 3 for 2), 'hex') || '-' ||
    encode(substring(rand from 5 for 6), 'hex')
  )::uuid;
END;
$$;

COMMENT ON FUNCTION gen_uuid_v7(timestamptz) IS
  'RFC 9562 UUIDv7. Time-basis ms-since-epoch from the provided timestamp '
  '(defaults to clock_timestamp()), random tail from pgcrypto.';

-- ---------------------------------------------------------------------------
-- 1. Add new uuid columns alongside the old BIGINT shape, populate from each
--    row's causal timestamp so the embedded ms-since-epoch matches that of
--    the row's lifetime.
-- ---------------------------------------------------------------------------

ALTER TABLE projects        ADD COLUMN uid uuid;
ALTER TABLE tasks           ADD COLUMN uid uuid;
ALTER TABLE journal_entries ADD COLUMN uid uuid;
ALTER TABLE notes           ADD COLUMN uid uuid;
ALTER TABLE wire_events     ADD COLUMN uid uuid;
ALTER TABLE messages        ADD COLUMN uid uuid;
ALTER TABLE messages        ADD COLUMN in_reply_to_uid uuid;
ALTER TABLE links           ADD COLUMN uid uuid;
ALTER TABLE links           ADD COLUMN source_uid uuid;
ALTER TABLE links           ADD COLUMN target_uid uuid;
ALTER TABLE embeddings      ADD COLUMN uid uuid;
ALTER TABLE embeddings      ADD COLUMN source_uid uuid;

-- Per-table PK backfill. wire_events uses first_seen_at; messages uses sent_at.
UPDATE projects        SET uid = gen_uuid_v7(created_at);
UPDATE tasks           SET uid = gen_uuid_v7(created_at);
UPDATE journal_entries SET uid = gen_uuid_v7(created_at);
UPDATE notes           SET uid = gen_uuid_v7(created_at);
UPDATE wire_events     SET uid = gen_uuid_v7(first_seen_at);
UPDATE messages        SET uid = gen_uuid_v7(sent_at);
UPDATE links           SET uid = gen_uuid_v7(created_at);
UPDATE embeddings      SET uid = gen_uuid_v7(created_at);

-- ---------------------------------------------------------------------------
-- 2. FK backfill: join through the old BIGINT relations to populate the uuid
--    FK columns. Polymorphic FKs (links.source_id, links.target_id,
--    embeddings.source_id) fan out across the five referenced tables.
-- ---------------------------------------------------------------------------

UPDATE messages m
   SET in_reply_to_uid = parent.uid
  FROM messages parent
 WHERE m.in_reply_to IS NOT NULL
   AND m.in_reply_to = parent.id;

UPDATE links l SET source_uid = t.uid FROM tasks           t WHERE l.source_table = 'tasks'           AND l.source_id = t.id;
UPDATE links l SET source_uid = j.uid FROM journal_entries j WHERE l.source_table = 'journal_entries' AND l.source_id = j.id;
UPDATE links l SET source_uid = n.uid FROM notes           n WHERE l.source_table = 'notes'           AND l.source_id = n.id;
UPDATE links l SET source_uid = w.uid FROM wire_events     w WHERE l.source_table = 'wire_events'     AND l.source_id = w.id;
UPDATE links l SET source_uid = p.uid FROM projects        p WHERE l.source_table = 'projects'        AND l.source_id = p.id;

UPDATE links l SET target_uid = t.uid FROM tasks           t WHERE l.target_table = 'tasks'           AND l.target_id = t.id;
UPDATE links l SET target_uid = j.uid FROM journal_entries j WHERE l.target_table = 'journal_entries' AND l.target_id = j.id;
UPDATE links l SET target_uid = n.uid FROM notes           n WHERE l.target_table = 'notes'           AND l.target_id = n.id;
UPDATE links l SET target_uid = w.uid FROM wire_events     w WHERE l.target_table = 'wire_events'     AND l.target_id = w.id;
UPDATE links l SET target_uid = p.uid FROM projects        p WHERE l.target_table = 'projects'        AND l.target_id = p.id;

-- embeddings: only journal_entries + notes per hive_db::queries::embeddings::VALID_SOURCE_TABLES
UPDATE embeddings e SET source_uid = j.uid FROM journal_entries j WHERE e.source_table = 'journal_entries' AND e.source_id = j.id;
UPDATE embeddings e SET source_uid = n.uid FROM notes           n WHERE e.source_table = 'notes'           AND e.source_id = n.id;

-- ---------------------------------------------------------------------------
-- 3. Lock the new columns down before swapping shapes.
-- ---------------------------------------------------------------------------

ALTER TABLE projects        ALTER COLUMN uid SET NOT NULL;
ALTER TABLE tasks           ALTER COLUMN uid SET NOT NULL;
ALTER TABLE journal_entries ALTER COLUMN uid SET NOT NULL;
ALTER TABLE notes           ALTER COLUMN uid SET NOT NULL;
ALTER TABLE wire_events     ALTER COLUMN uid SET NOT NULL;
ALTER TABLE messages        ALTER COLUMN uid SET NOT NULL;
ALTER TABLE links           ALTER COLUMN uid       SET NOT NULL;
ALTER TABLE links           ALTER COLUMN source_uid SET NOT NULL;
ALTER TABLE links           ALTER COLUMN target_uid SET NOT NULL;
ALTER TABLE embeddings      ALTER COLUMN uid SET NOT NULL;
ALTER TABLE embeddings      ALTER COLUMN source_uid SET NOT NULL;
-- messages.in_reply_to_uid stays nullable (mirrors the old in_reply_to)

-- ---------------------------------------------------------------------------
-- 4. Drop old FKs + PKs + uniques + indices that reference BIGINT ids.
-- ---------------------------------------------------------------------------

-- tasks.project -> projects.name is FK on a text column (not on id), so it stays.
-- notes.project -> projects.name same.

ALTER TABLE messages DROP CONSTRAINT messages_in_reply_to_fkey;

ALTER TABLE projects        DROP CONSTRAINT projects_pkey;
ALTER TABLE tasks           DROP CONSTRAINT tasks_pkey;
ALTER TABLE journal_entries DROP CONSTRAINT journal_entries_pkey;
ALTER TABLE notes           DROP CONSTRAINT notes_pkey;
ALTER TABLE wire_events     DROP CONSTRAINT wire_events_pkey;
ALTER TABLE messages        DROP CONSTRAINT messages_pkey;
ALTER TABLE links           DROP CONSTRAINT links_pkey;
ALTER TABLE embeddings      DROP CONSTRAINT embeddings_pkey;

ALTER TABLE links DROP CONSTRAINT links_source_table_source_id_target_table_target_id_link_ty_key;
ALTER TABLE embeddings DROP CONSTRAINT embeddings_source_table_source_id_model_key;

DROP INDEX idx_links_source;
DROP INDEX idx_links_target;
DROP INDEX idx_embeddings_source;
DROP INDEX idx_messages_reply;

-- ---------------------------------------------------------------------------
-- 5. Drop the BIGINT id + FK columns, rename uuid columns into place.
-- ---------------------------------------------------------------------------

ALTER TABLE projects        DROP COLUMN id;
ALTER TABLE tasks           DROP COLUMN id;
ALTER TABLE journal_entries DROP COLUMN id;
ALTER TABLE notes           DROP COLUMN id;
ALTER TABLE wire_events     DROP COLUMN id;
ALTER TABLE messages        DROP COLUMN id;
ALTER TABLE messages        DROP COLUMN in_reply_to;
ALTER TABLE links           DROP COLUMN id;
ALTER TABLE links           DROP COLUMN source_id;
ALTER TABLE links           DROP COLUMN target_id;
ALTER TABLE embeddings      DROP COLUMN id;
ALTER TABLE embeddings      DROP COLUMN source_id;

ALTER TABLE projects        RENAME COLUMN uid TO id;
ALTER TABLE tasks           RENAME COLUMN uid TO id;
ALTER TABLE journal_entries RENAME COLUMN uid TO id;
ALTER TABLE notes           RENAME COLUMN uid TO id;
ALTER TABLE wire_events     RENAME COLUMN uid TO id;
ALTER TABLE messages        RENAME COLUMN uid TO id;
ALTER TABLE messages        RENAME COLUMN in_reply_to_uid TO in_reply_to;
ALTER TABLE links           RENAME COLUMN uid TO id;
ALTER TABLE links           RENAME COLUMN source_uid TO source_id;
ALTER TABLE links           RENAME COLUMN target_uid TO target_id;
ALTER TABLE embeddings      RENAME COLUMN uid TO id;
ALTER TABLE embeddings      RENAME COLUMN source_uid TO source_id;

-- ---------------------------------------------------------------------------
-- 6. Re-add the PKs, FKs, uniques, indices on the new uuid columns.
-- ---------------------------------------------------------------------------

ALTER TABLE projects        ADD PRIMARY KEY (id);
ALTER TABLE tasks           ADD PRIMARY KEY (id);
ALTER TABLE journal_entries ADD PRIMARY KEY (id);
ALTER TABLE notes           ADD PRIMARY KEY (id);
ALTER TABLE wire_events     ADD PRIMARY KEY (id);
ALTER TABLE messages        ADD PRIMARY KEY (id);
ALTER TABLE links           ADD PRIMARY KEY (id);
ALTER TABLE embeddings      ADD PRIMARY KEY (id);

ALTER TABLE messages
  ADD CONSTRAINT messages_in_reply_to_fkey
  FOREIGN KEY (in_reply_to) REFERENCES messages(id);

ALTER TABLE links
  ADD CONSTRAINT links_unique_edge
  UNIQUE (source_table, source_id, target_table, target_id, link_type);

ALTER TABLE embeddings
  ADD CONSTRAINT embeddings_unique_source
  UNIQUE (source_table, source_id, model);

CREATE INDEX idx_links_source ON links(source_table, source_id);
CREATE INDEX idx_links_target ON links(target_table, target_id);
CREATE INDEX idx_embeddings_source ON embeddings(source_table, source_id);
CREATE INDEX idx_messages_reply ON messages(in_reply_to);

-- ---------------------------------------------------------------------------
-- 7. DEFAULT gen_uuid_v7() so hand-written INSERTs and ad-hoc tooling get a
--    sane id without having to plumb the uuid crate in. App code still
--    generates client-side for the wire-shape guarantee.
-- ---------------------------------------------------------------------------

ALTER TABLE projects        ALTER COLUMN id SET DEFAULT gen_uuid_v7();
ALTER TABLE tasks           ALTER COLUMN id SET DEFAULT gen_uuid_v7();
ALTER TABLE journal_entries ALTER COLUMN id SET DEFAULT gen_uuid_v7();
ALTER TABLE notes           ALTER COLUMN id SET DEFAULT gen_uuid_v7();
ALTER TABLE wire_events     ALTER COLUMN id SET DEFAULT gen_uuid_v7();
ALTER TABLE messages        ALTER COLUMN id SET DEFAULT gen_uuid_v7();
ALTER TABLE links           ALTER COLUMN id SET DEFAULT gen_uuid_v7();
ALTER TABLE embeddings      ALTER COLUMN id SET DEFAULT gen_uuid_v7();

COMMIT;
