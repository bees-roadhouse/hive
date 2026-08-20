-- 0003: journal.mentions TEXT -> JSONB, plus a GIN (jsonb_path_ops) index.
--
-- The journal RLS read predicate and the store's mention probes matched
-- @mentions with LIKE '%"slug"%' against the serialized JSON array — brittle
-- (a space after a comma in the serialization breaks the match) and
-- unindexable (every journal read predicate-evaluated a LIKE scan). The
-- predicate is now containment: mentions @> jsonb_build_array(acting_user),
-- which is exact and GIN-indexable.
--
-- Hybrid rules in 0001_baseline_marker.sql: this runs BEFORE the inline DDL,
-- so it must tolerate a fresh database (no journal table yet — the inline
-- constant creates it directly in the final shape, index included) and an
-- old-shape one (TEXT mentions, converted here without losing rows).
DO $$
BEGIN
  -- Fresh database: migrations run before the inline DDL, so the table may
  -- not exist yet — the inline constant builds the final shape and this
  -- whole block is moot.
  IF NOT EXISTS (
    SELECT 1 FROM information_schema.tables
    WHERE table_schema = current_schema() AND table_name = 'journal'
  ) THEN
    RETURN;
  END IF;

  -- Probed rather than assumed, so a table already on the final shape
  -- no-ops. Every existing value is a serde_json-serialized string array,
  -- so the I/O-conversion cast cannot fail on real data.
  IF EXISTS (
    SELECT 1 FROM information_schema.columns
    WHERE table_schema = current_schema() AND table_name = 'journal'
      AND column_name = 'mentions' AND data_type = 'text'
  ) THEN
    -- The text default has no assignment cast to jsonb ("default for column
    -- cannot be cast automatically"), so it is dropped and re-added.
    ALTER TABLE journal ALTER COLUMN mentions DROP DEFAULT;
    ALTER TABLE journal ALTER COLUMN mentions TYPE jsonb USING mentions::jsonb;
    ALTER TABLE journal ALTER COLUMN mentions SET DEFAULT '[]'::jsonb;
  END IF;

  -- Also in the inline DDL (IF NOT EXISTS both sides) so fresh installs get
  -- it without this block running.
  CREATE INDEX IF NOT EXISTS journal_mentions_gin ON journal USING gin (mentions jsonb_path_ops);
END $$;
