-- 0002: pgvector embeddings reshape — chunked rows, owner stamping, native
-- vector column + HNSW ANN index. Hybrid rules in 0001_baseline_marker.sql:
-- this runs BEFORE the inline DDL, so it must tolerate a fresh database (no
-- embeddings table yet) and an old-shape one.
--
-- Final shape (the inline SCHEMA constant in api/src/db.rs matches):
--   embeddings(ref_kind, ref_id, chunk_idx INT NOT NULL DEFAULT 0, model,
--              dim, owner TEXT NULL, vec BYTEA NULL, vec_v vector(384) NULL,
--              hash /* item-level, same on every chunk row */, created_at,
--              PRIMARY KEY (ref_kind, ref_id, chunk_idx),
--              CHECK (vec IS NOT NULL OR vec_v IS NOT NULL))
--   + HNSW on vec_v (cosine, m=16, ef_construction=64), btree on owner and
--     ref_kind.
--
-- Two-dimension resolution: vec_v is vector(384) for the BGE ONNX model and
-- carries the HNSW index; the 256-dim hash provider (dev/CI) keeps writing
-- only the BYTEA `vec` and stays on the brute-force path.

-- SCHEMA public is load-bearing: test schemas connect with
-- search_path = '{schema},public', so one shared install of the type resolves
-- from every schema.
CREATE EXTENSION IF NOT EXISTS vector SCHEMA public;

DO $$
DECLARE
  pk_name text;
BEGIN
  -- Fresh database: migrations run before the inline DDL, so the table may
  -- not exist yet — the inline constant creates it directly in the final
  -- shape and this whole block is moot.
  IF NOT EXISTS (
    SELECT 1 FROM information_schema.tables
    WHERE table_schema = current_schema() AND table_name = 'embeddings'
  ) THEN
    RETURN;
  END IF;

  -- Old shape (pre-chunking): wipe, then reshape. Chunking changes row
  -- identity, so old whole-item vectors are dead weight; the corpus is a few
  -- thousand rows and the worker re-embeds it in minutes. The wipe is safe
  -- against a broken ONNX model: the transformers latch pauses the backfill
  -- rather than refilling the table with mislabeled hash vectors.
  IF NOT EXISTS (
    SELECT 1 FROM information_schema.columns
    WHERE table_schema = current_schema() AND table_name = 'embeddings'
      AND column_name = 'chunk_idx'
  ) THEN
    DELETE FROM embeddings;
    ALTER TABLE embeddings ADD COLUMN chunk_idx INT NOT NULL DEFAULT 0;
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM information_schema.columns
    WHERE table_schema = current_schema() AND table_name = 'embeddings'
      AND column_name = 'owner'
  ) THEN
    ALTER TABLE embeddings ADD COLUMN owner TEXT;
  END IF;

  IF NOT EXISTS (
    SELECT 1 FROM information_schema.columns
    WHERE table_schema = current_schema() AND table_name = 'embeddings'
      AND column_name = 'vec_v'
  ) THEN
    ALTER TABLE embeddings ADD COLUMN vec_v public.vector(384);
  END IF;

  -- BGE rows dual-write vec + vec_v until the ANN path soaks; hash rows are
  -- vec-only. Either column may be NULL, never both.
  ALTER TABLE embeddings ALTER COLUMN vec DROP NOT NULL;
  IF NOT EXISTS (
    SELECT 1 FROM information_schema.table_constraints
    WHERE table_schema = current_schema() AND table_name = 'embeddings'
      AND constraint_name = 'embeddings_vec_present'
  ) THEN
    ALTER TABLE embeddings ADD CONSTRAINT embeddings_vec_present
      CHECK (vec IS NOT NULL OR vec_v IS NOT NULL);
  END IF;

  -- PK swap to (ref_kind, ref_id, chunk_idx), probed rather than assumed so
  -- a table already on the final shape no-ops.
  IF NOT EXISTS (
    SELECT 1
    FROM information_schema.table_constraints c
    JOIN information_schema.key_column_usage k
      ON k.constraint_name = c.constraint_name
     AND k.table_schema = c.table_schema
     AND k.table_name = c.table_name
    WHERE c.table_schema = current_schema() AND c.table_name = 'embeddings'
      AND c.constraint_type = 'PRIMARY KEY' AND k.column_name = 'chunk_idx'
  ) THEN
    SELECT c.constraint_name INTO pk_name
    FROM information_schema.table_constraints c
    WHERE c.table_schema = current_schema() AND c.table_name = 'embeddings'
      AND c.constraint_type = 'PRIMARY KEY';
    IF pk_name IS NOT NULL THEN
      EXECUTE format('ALTER TABLE embeddings DROP CONSTRAINT %I', pk_name);
    END IF;
    ALTER TABLE embeddings ADD PRIMARY KEY (ref_kind, ref_id, chunk_idx);
  END IF;

  -- ANN + filter indexes. Also in the inline DDL (IF NOT EXISTS both sides)
  -- so fresh installs get them without this block running.
  CREATE INDEX IF NOT EXISTS embeddings_vec_hnsw ON embeddings
    USING hnsw (vec_v public.vector_cosine_ops) WITH (m = 16, ef_construction = 64);
  CREATE INDEX IF NOT EXISTS embeddings_owner ON embeddings (owner);
  CREATE INDEX IF NOT EXISTS embeddings_kind ON embeddings (ref_kind);
END $$;
