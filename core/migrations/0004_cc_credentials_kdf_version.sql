-- 0004: cc_credentials gains kdf_version, so the vault-key derivation can
-- harden (1 = bare SHA-256, 2 = scrypt under a fixed domain-separation salt)
-- without stranding rows sealed under the original derivation. Hybrid rules
-- in 0001_baseline_marker.sql: this runs BEFORE the inline DDL, so it must
-- tolerate a fresh database (no cc_credentials table yet — the inline
-- constant creates it in the final shape) and an old-shape one.
--
-- DEFAULT 1 is the backfill: every row that predates the column was encrypted
-- under SHA-256(HIVE_CRED_KEY), so the default labels legacy rows correctly
-- with no data rewrite. The write path binds kdf_version explicitly, so the
-- default never describes a new row.

DO $$
BEGIN
  -- Fresh database: migrations run before the inline DDL, so the table may
  -- not exist yet — the inline constant creates it directly in the final
  -- shape and this block is moot.
  IF NOT EXISTS (
    SELECT 1 FROM information_schema.tables
    WHERE table_schema = current_schema() AND table_name = 'cc_credentials'
  ) THEN
    RETURN;
  END IF;

  ALTER TABLE cc_credentials
    ADD COLUMN IF NOT EXISTS kdf_version INTEGER NOT NULL DEFAULT 1;
END $$;
