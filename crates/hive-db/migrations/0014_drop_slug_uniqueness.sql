-- Drop slug uniqueness on the 4 content entity tables.
--
-- Identity tables (`people`, `ai`) keep UNIQUE slugs ... humans + AIs have
-- one canonical handle each. The content tables drop uniqueness because
-- two tasks can share the title "Fix the build" and we now anchor by UUID
-- in the prose; the slug is just a URL-friendly label. The compose picker
-- writes `[[type:<uuid>|<title>]]` for new selections, and the resolver
-- tries UUID-first, then slug (newest-on-collision wins).
--
-- PG auto-names UNIQUE constraints as `<table>_<col>_key`. Verified against
-- the live DB at 127.0.0.1:5433 before writing this migration. The
-- IF EXISTS makes the migration idempotent ... re-running is a no-op once
-- the constraints are gone.
--
-- The supporting `idx_<table>_slug` btree indexes (created in 0012) stay
-- in place: lookups by slug are still common, just not unique.
--
-- sqlx runs each migration inside its own transaction; no explicit
-- BEGIN/COMMIT here.

ALTER TABLE tasks           DROP CONSTRAINT IF EXISTS tasks_slug_key;
ALTER TABLE notes           DROP CONSTRAINT IF EXISTS notes_slug_key;
ALTER TABLE events          DROP CONSTRAINT IF EXISTS events_slug_key;
ALTER TABLE journal_entries DROP CONSTRAINT IF EXISTS journal_entries_slug_key;
