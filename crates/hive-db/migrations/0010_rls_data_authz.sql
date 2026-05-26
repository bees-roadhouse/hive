-- Auth Phase 8 (hive-auth-mcp-design.md §5.6): row-level data authorization on
-- the shared content tables, driven by per-request session GUCs.
--
-- ###############################################################################
-- # SHADOW-SAFE BY CONSTRUCTION. Read this before touching it.                  #
-- ###############################################################################
-- These tables are the SHARED hive (pia/cera/apis + the CLI + UI all read/write
-- them). Turning RLS on must NOT break existing tokenless / warn-mode / no-
-- principal access. The policies below are DEFAULT-ALLOW: they permit the row
-- UNLESS a request has BOTH (a) opted into enforcement (the app.rls_enforce GUC
-- = 'on') AND (b) set a non-shared visibility. An ordinary connection sets none
-- of these GUCs, so `current_setting(name, true)` returns NULL and the first
-- predicate clause is TRUE → full access, exactly as today.
--
-- The proof that an UNSET GUC allows: `current_setting('app.rls_enforce', true)`
-- with missing_ok=true returns NULL when the GUC was never SET; and
-- `NULL IS DISTINCT FROM 'on'` is TRUE. So the policy's first OR-branch is TRUE
-- for any connection that didn't explicitly arm enforcement. Migrations, the
-- CLI, the UI in warn mode, and psql all fall in this bucket.
--
-- We use FORCE ROW LEVEL SECURITY so the policy is actually evaluated even for
-- the table-owning app role (without FORCE, the owner bypasses RLS and this
-- whole phase would be a silent no-op). FORCE is safe precisely because the
-- policy is default-allow.
--
-- REVERSAL (if ever needed): the down path is
--   ALTER TABLE <t> NO FORCE ROW LEVEL SECURITY;
--   ALTER TABLE <t> DISABLE ROW LEVEL SECURITY;
--   DROP POLICY <t>_rls ON <t>;
-- for each table. Listed here, not run, since sqlx migrations are forward-only.
--
-- GUC contract (set per-request by hive-api inside the request txn, §5.6):
--   app.rls_enforce      'on' to actually filter; anything else / unset = allow.
--   app.visibility       'shared' (see all) | 'owner' (see own handles) | 'custom'.
--   app.principal_handles comma-separated owner-tags the principal may see under
--                         'owner'/'custom' (e.g. 'nate,pia'): the connecting
--                         human's username + the AI handle(s) in play. Computed
--                         app-side from the resolved principal so the policy does
--                         no joins on the hot path.
--
-- Ownership signal per table (confirmed against the live schema, NOT invented):
--   journal_entries.ai     TEXT  (writer handle: pia|cera|apis|nate)
--   tasks.owner            TEXT  (pia|apis|cera|nate|maggie)
--   notes.author           TEXT  (pia|apis|cera|nate|maggie)
--   wire_events            HAS NO OWNERSHIP COLUMN — it's shared situational
--     awareness (source/category/severity only). Under 'owner'/'custom'
--     visibility there is nothing to key on, so its policy treats narrowed
--     visibility as "see none" EXCEPT it still default-allows when unenforced.
--     Flagged to the lead: if per-principal wire scoping is ever wanted, the
--     table needs an owner/affects-principal column first.

BEGIN;

-- Shared helper: is RLS NOT being enforced on this connection? (the shadow gate)
-- True when the enforce GUC is unset or not exactly 'on'. Default-allow leans on
-- this returning true for ordinary connections.
CREATE OR REPLACE FUNCTION hive_rls_unenforced() RETURNS boolean
  LANGUAGE sql STABLE AS
$$ SELECT current_setting('app.rls_enforce', true) IS DISTINCT FROM 'on' $$;

-- Shared helper: does the current request have 'shared' visibility (or none)?
CREATE OR REPLACE FUNCTION hive_rls_shared() RETURNS boolean
  LANGUAGE sql STABLE AS
$$ SELECT coalesce(current_setting('app.visibility', true), 'shared') = 'shared' $$;

-- Shared helper: the set of owner-handles the principal may see (under owner/
-- custom visibility). Empty when unset.
CREATE OR REPLACE FUNCTION hive_rls_handles() RETURNS text[]
  LANGUAGE sql STABLE AS
$$ SELECT string_to_array(coalesce(current_setting('app.principal_handles', true), ''), ',') $$;

-- journal_entries: owner tag = ai
ALTER TABLE journal_entries ENABLE ROW LEVEL SECURITY;
ALTER TABLE journal_entries FORCE ROW LEVEL SECURITY;
CREATE POLICY journal_entries_rls ON journal_entries
  USING (
    hive_rls_unenforced()      -- shadow / no principal context => allow (default)
    OR hive_rls_shared()       -- 'shared' visibility => whole hive
    OR ai = ANY (hive_rls_handles())  -- 'owner'/'custom' => only the principal's handles
  );

-- tasks: owner tag = owner
ALTER TABLE tasks ENABLE ROW LEVEL SECURITY;
ALTER TABLE tasks FORCE ROW LEVEL SECURITY;
CREATE POLICY tasks_rls ON tasks
  USING (
    hive_rls_unenforced()
    OR hive_rls_shared()
    OR owner = ANY (hive_rls_handles())
  );

-- notes: owner tag = author
ALTER TABLE notes ENABLE ROW LEVEL SECURITY;
ALTER TABLE notes FORCE ROW LEVEL SECURITY;
CREATE POLICY notes_rls ON notes
  USING (
    hive_rls_unenforced()
    OR hive_rls_shared()
    OR author = ANY (hive_rls_handles())
  );

-- wire_events: NO ownership column (see header note). Default-allow when
-- unenforced; under enforcement only 'shared' visibility sees it (a narrowed
-- principal sees no wire events, since there's no owner tag to match). This
-- keeps the trusted fleet (shared) working and fails safe for a narrowed one.
ALTER TABLE wire_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE wire_events FORCE ROW LEVEL SECURITY;
CREATE POLICY wire_events_rls ON wire_events
  USING (
    hive_rls_unenforced()
    OR hive_rls_shared()
  );

COMMIT;
