-- Mention pipeline groundwork: a new first-class `events` entity (so meetings,
-- household occurrences, milestones can be `[[event:slug]]`-referenced from
-- journal prose) and `slug` columns on tasks / notes / journal_entries so the
-- resolver has a single rule for typed mentions across every entity.
--
-- Slug pattern matches `people.slug` (migration 0003): `^[a-z][a-z0-9_-]*$`.
-- Same rule everywhere = one regex in the parser.
--
-- Slug columns are NULLABLE here on purpose. New rows get one derived from the
-- title in the queries layer; existing rows are backfilled by the DO block at
-- the bottom. A follow-up migration can NOT NULL them once every consumer
-- writes one. UNIQUE means INSERT collisions must be resolved caller-side
-- (`-2` / `-3` suffix loop in the queries layer).
--
-- sqlx runs each migration inside its own transaction; no explicit BEGIN/COMMIT
-- here so we don't fight the migration ledger.

-- ---------------------------------------------------------------------------
-- 1. events: date-anchored entity, standalone first-class. Anything that has
--    a `starts_at` but isn't a journal entry or task.
-- ---------------------------------------------------------------------------

CREATE TABLE events (
  id uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  slug TEXT NOT NULL UNIQUE CHECK (slug ~ '^[a-z][a-z0-9_-]*$'),
  title TEXT NOT NULL,
  body TEXT,
  starts_at TIMESTAMPTZ NOT NULL,
  ends_at TIMESTAMPTZ,            -- NULL = instant event
  location TEXT,
  tags TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  fts tsvector GENERATED ALWAYS AS (
    to_tsvector('english',
      coalesce(title, '') || ' ' ||
      coalesce(body, '') || ' ' ||
      coalesce(location, '') || ' ' ||
      coalesce(tags, '')
    )
  ) STORED
);
CREATE INDEX idx_events_starts_at ON events(starts_at DESC);
CREATE INDEX idx_events_slug ON events(slug);
CREATE INDEX idx_events_fts ON events USING GIN (fts);

-- ---------------------------------------------------------------------------
-- 2. Slug columns on existing entities. Nullable + UNIQUE; CHECK is permissive
--    on NULL so the backfill below can run row-by-row.
-- ---------------------------------------------------------------------------

ALTER TABLE tasks
  ADD COLUMN slug TEXT UNIQUE CHECK (slug IS NULL OR slug ~ '^[a-z][a-z0-9_-]*$');
CREATE INDEX idx_tasks_slug ON tasks(slug);

ALTER TABLE notes
  ADD COLUMN slug TEXT UNIQUE CHECK (slug IS NULL OR slug ~ '^[a-z][a-z0-9_-]*$');
CREATE INDEX idx_notes_slug ON notes(slug);

ALTER TABLE journal_entries
  ADD COLUMN slug TEXT UNIQUE CHECK (slug IS NULL OR slug ~ '^[a-z][a-z0-9_-]*$');
CREATE INDEX idx_journal_slug ON journal_entries(slug);

-- ---------------------------------------------------------------------------
-- 3. Backfill slugs from titles for existing rows. Idempotent: only touches
--    rows where slug IS NULL, so re-running the migration is a no-op once
--    everyone has one. The shape mirrors the queries-layer derivation:
--      lowercase, non-alnum -> '-', collapse repeats (regex `[^a-z0-9]+`
--      already does that with the `+`), trim '-' from ends, prefix
--      `<type>-` if empty or starts with a digit, then append `-2`, `-3`
--      on collision against the existing UNIQUE index.
-- ---------------------------------------------------------------------------

DO $$
DECLARE
  rec RECORD;
  base TEXT;
  candidate TEXT;
  counter INT;
BEGIN
  -- journal_entries: fall back to `<ai>-entry` if title is null.
  FOR rec IN
    SELECT id, title, ai FROM journal_entries WHERE slug IS NULL ORDER BY created_at, id
  LOOP
    base := regexp_replace(lower(coalesce(rec.title, rec.ai || '-entry')), '[^a-z0-9]+', '-', 'g');
    base := trim(both '-' from base);
    IF base = '' OR base ~ '^[0-9]' THEN
      base := 'entry-' || base;
      base := trim(both '-' from base);
    END IF;
    candidate := base;
    counter := 1;
    WHILE EXISTS (SELECT 1 FROM journal_entries WHERE slug = candidate) LOOP
      counter := counter + 1;
      candidate := base || '-' || counter;
    END LOOP;
    UPDATE journal_entries SET slug = candidate WHERE id = rec.id;
  END LOOP;

  -- tasks: fall back to 'task' if title is null/empty (shouldn't be ... tasks.title is NOT NULL).
  FOR rec IN
    SELECT id, title FROM tasks WHERE slug IS NULL ORDER BY created_at, id
  LOOP
    base := regexp_replace(lower(coalesce(rec.title, 'task')), '[^a-z0-9]+', '-', 'g');
    base := trim(both '-' from base);
    IF base = '' OR base ~ '^[0-9]' THEN
      base := 'task-' || base;
      base := trim(both '-' from base);
    END IF;
    candidate := base;
    counter := 1;
    WHILE EXISTS (SELECT 1 FROM tasks WHERE slug = candidate) LOOP
      counter := counter + 1;
      candidate := base || '-' || counter;
    END LOOP;
    UPDATE tasks SET slug = candidate WHERE id = rec.id;
  END LOOP;

  -- notes: fall back to 'note' if title is null.
  FOR rec IN
    SELECT id, title FROM notes WHERE slug IS NULL ORDER BY created_at, id
  LOOP
    base := regexp_replace(lower(coalesce(rec.title, 'note')), '[^a-z0-9]+', '-', 'g');
    base := trim(both '-' from base);
    IF base = '' OR base ~ '^[0-9]' THEN
      base := 'note-' || base;
      base := trim(both '-' from base);
    END IF;
    candidate := base;
    counter := 1;
    WHILE EXISTS (SELECT 1 FROM notes WHERE slug = candidate) LOOP
      counter := counter + 1;
      candidate := base || '-' || counter;
    END LOOP;
    UPDATE notes SET slug = candidate WHERE id = rec.id;
  END LOOP;
END $$;
