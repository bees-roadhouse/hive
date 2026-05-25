-- Auth Phase 2 (hive-auth-mcp-design.md §8 Phase 2, §5 data model): the
-- built-in Authorization Server core. Humans, their password credentials,
-- sessions + rotating refresh tokens, and the single-row policy.
--
-- AI identities, ai_ownership, ai_access_grants, and the consent/revocation
-- tables arrive in Phase 6; this migration is humans + sessions only.
--
-- UUIDv7 PKs via gen_uuid_v7() (defined in 0002), matching the codebase.

BEGIN;

-- HUMAN identities (principal_type = 'human'). external_idp/external_sub are
-- nullable now (builtin mode) and carry federation linking in Mode B (§6).
-- granted_scopes is Phase 2's minimal role model: the scopes a human holds,
-- baked into their access token at mint (the role/grant tables proper land
-- later). is_admin gates the kill-switch + (Phase 4+) admin endpoints.
CREATE TABLE users (
  id                    uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  username              TEXT NOT NULL UNIQUE CHECK (username ~ '^[A-Za-z0-9._-]{1,64}$'),
  display_name          TEXT,
  email                 TEXT UNIQUE,
  status                TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active','disabled','locked')),
  is_admin              BOOLEAN NOT NULL DEFAULT FALSE,
  granted_scopes        TEXT[] NOT NULL DEFAULT '{}',
  external_idp          TEXT,
  external_sub          TEXT,
  session_lifetime_secs INTEGER CHECK (session_lifetime_secs IS NULL OR session_lifetime_secs > 0),
  created_at            TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- argon2id PHC string (salt embedded). One row per user; builtin mode only.
CREATE TABLE password_credentials (
  user_id      uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  argon2_hash  TEXT NOT NULL,
  must_change  BOOLEAN NOT NULL DEFAULT FALSE,
  updated_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- A login session. Phase 2 is human-only (kind='human'); the mcp_ai kind +
-- act_user_id + nullable expiry arrive in Phase 6. expires_at is set here
-- (humans always expire). revoked_at is the server-side off-switch.
CREATE TABLE sessions (
  id            uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  kind          TEXT NOT NULL DEFAULT 'human' CHECK (kind IN ('human','mcp_ai')),
  user_id       uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  client_id     TEXT NOT NULL,
  scopes        TEXT[] NOT NULL DEFAULT '{}',
  amr           TEXT[] NOT NULL DEFAULT '{}',
  expires_at    TIMESTAMPTZ NOT NULL,
  revoked_at    TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sessions_user_id_idx ON sessions(user_id);

-- Opaque refresh tokens, stored HASHED (sha256 hex), one chain per session.
-- Rotated on every use: the old row gets superseded_by set; presenting a
-- superseded token is reuse => the whole chain is revoked (theft detection).
CREATE TABLE refresh_tokens (
  id            uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  session_id    uuid NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
  token_hash    TEXT NOT NULL UNIQUE,
  superseded_by uuid REFERENCES refresh_tokens(id),
  expires_at    TIMESTAMPTZ NOT NULL,
  used_at       TIMESTAMPTZ,
  created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX refresh_tokens_session_id_idx ON refresh_tokens(session_id);

-- Short-TTL authorization-code state for the auth-code + PKCE flow. Rows are
-- consumed (deleted) on token exchange and swept on expiry.
CREATE TABLE authorization_codes (
  code            TEXT PRIMARY KEY,
  client_id       TEXT NOT NULL,
  user_id         uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  redirect_uri    TEXT NOT NULL,
  code_challenge  TEXT NOT NULL,
  scopes          TEXT[] NOT NULL DEFAULT '{}',
  resource        TEXT,
  expires_at      TIMESTAMPTZ NOT NULL,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Single-row policy (id is pinned to 1). Defaults per §2/§5: 8h default
-- session, 24h max, 10m access token, 14-char password minimum (BR policy).
-- mfa_mode/auth_mode/authz_mode are carried now so the columns exist when
-- Phase 4/9 wire them; Phase 2 reads only the session/password fields.
CREATE TABLE auth_policy (
  id                          INTEGER PRIMARY KEY DEFAULT 1 CHECK (id = 1),
  global_default_session_secs INTEGER NOT NULL DEFAULT 28800,
  global_max_session_secs     INTEGER NOT NULL DEFAULT 86400,
  access_token_secs           INTEGER NOT NULL DEFAULT 600,
  password_min_length         INTEGER NOT NULL DEFAULT 14,
  mfa_mode                    TEXT NOT NULL DEFAULT 'internal' CHECK (mfa_mode IN ('internal','delegated','off')),
  auth_mode                   TEXT NOT NULL DEFAULT 'builtin' CHECK (auth_mode IN ('builtin','external')),
  authz_mode                  TEXT NOT NULL DEFAULT 'internal' CHECK (authz_mode IN ('internal','external'))
);
INSERT INTO auth_policy (id) VALUES (1);

COMMIT;
