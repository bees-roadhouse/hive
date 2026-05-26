-- Auth Phase 7 (hive-auth-mcp-design.md §5.7): risk-based adaptive token
-- rotation for the NON-EXPIRING AI/MCP token class. Every MCP-token use is
-- scored against a per-session behavioral baseline; an anomaly FORCES A RE-KEY
-- (mint a fresh token, invalidate the old jti) rather than revoking the
-- identity. SHADOW-FIRST: scored + logged by default; only enforced behind
-- HIVE_RISK_ENFORCE.
--
-- Scope note: signals here are IP + user-agent + cadence (cheap, no external
-- dependency). Coarse geo/ASN + impossible-travel velocity are the documented
-- next signals (need a geo-IP DB, §5.7) — the schema leaves room (geo columns)
-- but Phase 7 populates IP/UA/cadence only.

BEGIN;

-- Per-use signal rows, bounded per session (we keep the last N; older rows are
-- pruned by the app, not kept forever — §5.7 "bounded per token"). One row per
-- scored MCP-token request.
CREATE TABLE token_usage_signals (
  id           uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  session_id   uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  jti          uuid,                          -- the token in use at capture time
  ip           TEXT,                          -- client IP (X-Forwarded-For via trusted proxy, else peer)
  user_agent   TEXT,
  geo_country  TEXT,                           -- reserved for the geo seam (null in Phase 7)
  asn          INTEGER,                        -- reserved for the geo seam (null in Phase 7)
  score        INTEGER NOT NULL DEFAULT 0,     -- the computed risk score for this use
  band         TEXT NOT NULL DEFAULT 'low',    -- low | medium | high
  used_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX token_usage_signals_session_idx ON token_usage_signals(session_id, used_at DESC);

-- Per-session behavioral baseline (the rolling aggregate scoring reads). Lives
-- on the sessions row as denormalized summary so the hot path is one row read.
-- seen_ips / seen_uas are bounded sets the app maintains; use_count drives the
-- warmup suppression (cadence signals stay off until the token has history).
ALTER TABLE sessions ADD COLUMN risk_seen_ips   TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE sessions ADD COLUMN risk_seen_uas   TEXT[] NOT NULL DEFAULT '{}';
ALTER TABLE sessions ADD COLUMN risk_use_count  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN risk_first_seen_at TIMESTAMPTZ;
ALTER TABLE sessions ADD COLUMN risk_last_seen_at  TIMESTAMPTZ;
-- needs_rekey: set when an enforced risk decision invalidated the live token;
-- the next MCP connect sees this, mints a fresh token, and clears it. The grant
-- + AI identity are untouched — this is re-key, not revoke.
ALTER TABLE sessions ADD COLUMN needs_rekey BOOLEAN NOT NULL DEFAULT FALSE;

-- Audit trail of risk decisions (CAEP-shaped: §5.7 reuses CAEP event vocab).
-- mode records whether the decision was shadow-only or actually enforced, so a
-- shadow-period analysis can see what WOULD have happened.
CREATE TABLE risk_events (
  id           uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  session_id   uuid REFERENCES sessions(id) ON DELETE CASCADE,
  jti          uuid,
  subject_id   uuid,                           -- the AI principal
  act_user_id  uuid REFERENCES users(id),      -- the connecting human
  band         TEXT NOT NULL,                  -- low | medium | high
  score        INTEGER NOT NULL,
  reasons      TEXT[] NOT NULL DEFAULT '{}',   -- which signals fired
  mode         TEXT NOT NULL,                  -- 'shadow' | 'enforced'
  action       TEXT NOT NULL,                  -- 'observed' | 'rekey_forced'
  created_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX risk_events_session_idx ON risk_events(session_id, created_at DESC);
CREATE INDEX risk_events_subject_idx ON risk_events(subject_id);

COMMIT;
