-- Auth Phase 5 (hive-auth-mcp-design.md §3.2): RFC 8628 Device Authorization
-- Grant. The CLI (a public client with no browser) starts a flow, shows the
-- user a short user_code + a verification URL, and polls /token while the user
-- approves it in a browser on another device.
--
-- The device_code is the high-entropy bearer secret the CLI polls with — stored
-- HASHED (sha256 hex, like refresh tokens). The user_code is the short,
-- human-typed code (low entropy, looked up directly) the human enters on the
-- verification page; it's unique among live rows. status walks
-- pending → approved | denied; approval binds the row to the approving user_id.

BEGIN;

CREATE TABLE device_codes (
  id               uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  device_code_hash TEXT NOT NULL UNIQUE,          -- sha256 hex of the polled secret
  user_code        TEXT NOT NULL UNIQUE,          -- short human-entered code (e.g. WDJB-MJHT)
  client_id        TEXT NOT NULL,
  scopes           TEXT[] NOT NULL DEFAULT '{}',
  resource         TEXT,
  status           TEXT NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending','approved','denied')),
  user_id          uuid REFERENCES users(id) ON DELETE CASCADE,  -- set on approval
  amr              TEXT[] NOT NULL DEFAULT '{}',   -- approving human's auth methods (pwd[,otp])
  interval_secs    INTEGER NOT NULL DEFAULT 5,     -- min seconds between polls
  last_polled_at   TIMESTAMPTZ,                    -- backs slow_down enforcement
  expires_at       TIMESTAMPTZ NOT NULL,
  created_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX device_codes_user_code_idx ON device_codes(user_code);
CREATE INDEX device_codes_expires_at_idx ON device_codes(expires_at);

COMMIT;
