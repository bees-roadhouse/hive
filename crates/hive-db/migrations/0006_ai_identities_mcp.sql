-- Auth Phase 6 (hive-auth-mcp-design.md §1.5 AI identity, §3.4 MCP token
-- issuance, §5.5 revocation): AI principals as first-class auth identities,
-- the per-(AI, owner) access grants, the revocation handles for the
-- non-expiring MCP-token class, and the sessions-table extension that lets one
-- session row describe either a human login or an AI's MCP connection.
--
-- Scope choice (per the lead, a deliberate simplification of the design's
-- ai_ownership join): an AI is owned by a user via ai_identities.owned_by, and
-- the per-(AI, user) thing a human grants their AI is ai_access_grants. One AI
-- can still be reachable by multiple humans: each human who owns/grants gets
-- their own ai_access_grants row, and the grant used at issue time is the
-- CONNECTING human's row. The owned_by column is the primary owner; co-grants
-- are modeled by additional grant rows (ai_ownership-as-a-join is a later
-- refinement if a true many-owners-without-a-grant case appears, §9 #9).
--
-- UUIDv7 PKs via gen_uuid_v7() (migration 0002), matching the codebase.

BEGIN;

-- AI identities (principal_type = 'ai', §1.5). No password/TOTP: an AI never
-- self-authenticates; it only gets a token via an owning human connecting over
-- MCP (§3.4). `name` is the handle ('pia' | 'cera' | 'apis' | ...). `kind`
-- carries a coarse classifier (trusted-fleet vs restricted/external) for future
-- default-visibility policy; free-text for now, defaulted to 'assistant'.
CREATE TABLE ai_identities (
  id          uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  name        TEXT NOT NULL UNIQUE CHECK (name ~ '^[A-Za-z0-9._-]{1,64}$'),
  display_name TEXT,
  kind        TEXT NOT NULL DEFAULT 'assistant',
  owned_by    uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  status      TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','disabled')),
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX ai_identities_owned_by_idx ON ai_identities(owned_by);

-- Per-(AI, user) access config (§3.4): what THIS human grants THIS AI to do AS
-- them. The connecting human's grant is the one applied at MCP-token-issue
-- time. granted_scopes is the scope ceiling the human hands the AI;
-- data_visibility is the RLS lever (§5.6, enforced in Phase 8);
-- mcp_token_no_expiry defaults TRUE (Nate: persistent agent connections) — a
-- human can opt their AI into expiry by setting it FALSE.
CREATE TABLE ai_access_grants (
  id                 uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  ai_id              uuid NOT NULL REFERENCES ai_identities(id) ON DELETE CASCADE,
  user_id            uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  granted_scopes     TEXT[] NOT NULL DEFAULT '{}',
  data_visibility    TEXT NOT NULL DEFAULT 'owner' CHECK (data_visibility IN ('shared','owner','custom')),
  mcp_token_no_expiry BOOLEAN NOT NULL DEFAULT TRUE,
  created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
  revoked_at         TIMESTAMPTZ,
  UNIQUE (ai_id, user_id)
);
CREATE INDEX ai_access_grants_ai_id_idx ON ai_access_grants(ai_id);
CREATE INDEX ai_access_grants_user_id_idx ON ai_access_grants(user_id);

-- Extend sessions (0005 was human-only) to also describe an AI's MCP connection
-- (§2 two-token-classes, §5.5 revocation). One row, branched on `kind`:
--   human : user_id set, ai_id/act_user_id NULL, expires_at set (always).
--   mcp_ai: ai_id set, act_user_id = the connecting human, user_id NULL,
--           expires_at NULL by default (non-expiring) unless the grant opts in.
-- user_id loses its NOT NULL so an AI session needn't borrow a human row; a
-- CHECK keeps each kind well-formed. revoked_at is the off-switch (load-bearing
-- for the non-expiring class).
ALTER TABLE sessions ALTER COLUMN user_id DROP NOT NULL;
ALTER TABLE sessions ALTER COLUMN expires_at DROP NOT NULL;
ALTER TABLE sessions ADD COLUMN ai_id uuid REFERENCES ai_identities(id) ON DELETE CASCADE;
ALTER TABLE sessions ADD COLUMN act_user_id uuid REFERENCES users(id) ON DELETE CASCADE;
ALTER TABLE sessions ADD COLUMN jti uuid;          -- the issued access token's jti (revocation handle, mcp_ai)
ALTER TABLE sessions ADD COLUMN last_seen_at TIMESTAMPTZ;
ALTER TABLE sessions ADD COLUMN revoked_by uuid REFERENCES users(id);
ALTER TABLE sessions ADD COLUMN revoke_reason TEXT;
ALTER TABLE sessions
  ADD CONSTRAINT sessions_kind_shape CHECK (
    (kind = 'human'  AND user_id IS NOT NULL AND ai_id IS NULL AND expires_at IS NOT NULL)
    OR
    (kind = 'mcp_ai' AND ai_id IS NOT NULL AND act_user_id IS NOT NULL)
  );
CREATE INDEX sessions_ai_id_idx ON sessions(ai_id);
CREATE INDEX sessions_act_user_id_idx ON sessions(act_user_id);
CREATE INDEX sessions_jti_idx ON sessions(jti);

-- Explicit revocation handles, keyed by the access-token jti (§5.5). The
-- non-expiring MCP class leans on this entirely: every AI-token validation
-- checks its jti here. subject_id is denormalized (the AI id) for fast per-AI
-- sweeps; reason distinguishes manual ('owner','admin') from future risk-forced
-- ('risk:<signal>', §5.7).
CREATE TABLE revocations (
  jti         uuid PRIMARY KEY,
  session_id  uuid REFERENCES sessions(id) ON DELETE CASCADE,
  subject_id  uuid NOT NULL,
  act_user_id uuid REFERENCES users(id),
  revoked_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
  revoked_by  uuid REFERENCES users(id),
  reason      TEXT
);
CREATE INDEX revocations_subject_id_idx ON revocations(subject_id);

COMMIT;
