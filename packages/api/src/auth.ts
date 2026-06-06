// Auth primitives for v0.1.1 — password hashing and opaque tokens, built on
// node:crypto so the image stays dependency-free. Passwords use scrypt with a
// per-password random salt; session + API tokens are random and stored only as
// a sha256 hash (the plaintext is shown to the client once and never persisted).
import { createHash, randomBytes, scryptSync, timingSafeEqual } from "node:crypto";

/** PKCE S256 verify: base64url(sha256(verifier)) === challenge (constant-time). */
export function verifyPkce(verifier: string, challenge: string): boolean {
  const computed = createHash("sha256").update(verifier).digest("base64url");
  const a = Buffer.from(computed);
  const b = Buffer.from(challenge);
  return a.length === b.length && timingSafeEqual(a, b);
}

const SCRYPT_KEYLEN = 64;

/** Hash a password as `scrypt$<saltHex>$<hashHex>`. */
export function hashPassword(password: string): string {
  const salt = randomBytes(16);
  const derived = scryptSync(password, salt, SCRYPT_KEYLEN);
  return `scrypt$${salt.toString("hex")}$${derived.toString("hex")}`;
}

/** Constant-time verify against a stored `scrypt$salt$hash` string. */
export function verifyPassword(password: string, stored: string): boolean {
  const [scheme, saltHex, hashHex] = stored.split("$");
  if (scheme !== "scrypt" || !saltHex || !hashHex) return false;
  const expected = Buffer.from(hashHex, "hex");
  const actual = scryptSync(password, Buffer.from(saltHex, "hex"), expected.length);
  return expected.length === actual.length && timingSafeEqual(expected, actual);
}

/** A URL-safe opaque token, e.g. `hive_pat_<random>`. */
export function generateToken(prefix: string): string {
  return `${prefix}_${randomBytes(24).toString("base64url")}`;
}

/** sha256 hex — how tokens are stored and looked up. */
export function tokenHash(token: string): string {
  return createHash("sha256").update(token).digest("hex");
}

export const SESSION_PREFIX = "hive_sess";
export const API_TOKEN_PREFIX = "hive_pat";
export const AUTH_CODE_PREFIX = "hive_ac";
export const SESSION_COOKIE = "hive_session";
/** Session lifetime: 30 days. */
export const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;
/** OAuth authorization-code lifetime: 60 seconds (single-use). */
export const AUTH_CODE_TTL_MS = 60 * 1000;
/** OAuth access-token lifetime: 1 year (renewable via re-consent). */
export const OAUTH_TOKEN_TTL_MS = 365 * 24 * 60 * 60 * 1000;
