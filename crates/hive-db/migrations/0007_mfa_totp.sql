-- Auth Phase 4 (hive-auth-mcp-design.md §4): TOTP authenticator MFA (RFC 6238).
--
-- A user enrolls a TOTP secret (stored ENCRYPTED at rest, not plaintext — §4),
-- confirms it by submitting a current code (sets confirmed_at), and receives a
-- set of one-time recovery codes (stored hashed). At login, after the password
-- verifies, a user with a CONFIRMED credential must present a valid TOTP code
-- (or a recovery code) before tokens are issued.
--
-- The global mfa_mode toggle (internal|delegated|off) already lives on
-- auth_policy (migration 0005). Phase 4 reads it + the HIVE_MFA_MODE env
-- override; 'delegated' (external IdP owns MFA) and 'off' skip the second
-- factor. Per-user enable is implicit: a confirmed credential = MFA on for that
-- user.

BEGIN;

-- One TOTP credential per user. secret_enc is the RFC 6238 shared secret
-- encrypted with ChaCha20-Poly1305 (nonce prepended); never stored plaintext.
-- confirmed_at NULL => enrollment pending (does NOT gate login yet); set once
-- the user proves possession by submitting a valid code. failed_attempts +
-- locked_until back the rate-limit/lockout (BR policy: account lockdown).
CREATE TABLE mfa_credentials (
  user_id         uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
  secret_enc      BYTEA NOT NULL,
  confirmed_at    TIMESTAMPTZ,
  failed_attempts INTEGER NOT NULL DEFAULT 0,
  locked_until    TIMESTAMPTZ,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One-time recovery codes, stored HASHED (sha256 hex, like refresh tokens —
-- high-entropy random values, so a plain digest is right, not a slow KDF).
-- used_at marks a code consumed; a code is accepted once in place of a TOTP.
CREATE TABLE mfa_recovery_codes (
  id          uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  user_id     uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  code_hash   TEXT NOT NULL,
  used_at     TIMESTAMPTZ,
  created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX mfa_recovery_codes_user_id_idx ON mfa_recovery_codes(user_id);
-- A given hash is unique per user (so lookup-by-hash is unambiguous on redeem).
CREATE UNIQUE INDEX mfa_recovery_codes_user_hash_idx ON mfa_recovery_codes(user_id, code_hash);

-- Carry the auth methods (amr) decided at /authorize through the single-use
-- auth code to /token, so the issued session + access token record whether MFA
-- happened (['pwd'] vs ['pwd','otp']). Defaulted so pre-existing codes (none in
-- practice — codes are short-lived) and the human flow stay valid.
ALTER TABLE authorization_codes ADD COLUMN amr TEXT[] NOT NULL DEFAULT '{pwd}';

COMMIT;
