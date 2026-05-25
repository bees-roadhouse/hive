-- Auth Phase 1 (hive-auth-mcp-design.md §5 data model / §8 Phase 1):
-- the AS token-signing keypairs. EdDSA (Ed25519) per open decision #4.
--
-- Only the signing_keys table lands now. The rest of the auth data model
-- (users, sessions, refresh_tokens, ai_identities, ...) arrives in Phase 2+
-- when the AS core is built. This table exists so Phase 1 can mint a local
-- keypair, publish it at /jwks.json, and verify EdDSA JWTs against it.
--
-- private_key_der: PKCS#8 v2 Ed25519 private key (DER). Stored as bytea.
--   NOTE: Phase 1 stores it as-is so the single-node local AS can sign on
--   restart. Encrypting-at-rest (per §5 signing_keys.private_key_enc) is a
--   Phase 2 hardening item once the server key/KMS story lands; tracked there.
-- public_jwk: the JWK (OKP/Ed25519) published at /jwks.json, kid-addressed.

CREATE TABLE signing_keys (
  kid             TEXT PRIMARY KEY,
  alg             TEXT NOT NULL DEFAULT 'EdDSA' CHECK (alg = 'EdDSA'),
  private_key_der BYTEA NOT NULL,
  public_jwk      JSONB NOT NULL,
  active          BOOLEAN NOT NULL DEFAULT TRUE,
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- At most one active signing key at a time in Phase 1 (key rotation arrives
-- with the AS core). Partial unique index enforces the single-active invariant.
CREATE UNIQUE INDEX signing_keys_one_active_idx ON signing_keys (active) WHERE active;
