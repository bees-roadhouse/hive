// Minimal OIDC relying-party. Dormant unless OIDC_ISSUER is set. Hand-rolled on
// node:crypto so the runtime image stays dependency-free; verifies the ID token's
// RS256 signature against the provider JWKS plus iss/aud/exp/nonce. (RS256 only —
// the common case; swap in a JWT lib if a provider signs with something else.)
import { createPublicKey, createVerify } from "node:crypto";

export interface OidcConfig {
  issuer: string;
  clientId: string;
  clientSecret: string;
  redirectUri: string;
  allowedDomains: string[];
}

/** Read OIDC config from env, or null when unconfigured (feature off). */
export function oidcConfig(): OidcConfig | null {
  const issuer = process.env.OIDC_ISSUER;
  if (!issuer) return null;
  return {
    issuer: issuer.replace(/\/$/, ""),
    clientId: process.env.OIDC_CLIENT_ID ?? "",
    clientSecret: process.env.OIDC_CLIENT_SECRET ?? "",
    redirectUri: process.env.OIDC_REDIRECT_URI ?? "",
    allowedDomains: (process.env.OIDC_ALLOWED_DOMAINS ?? "")
      .split(",")
      .map((s) => s.trim().toLowerCase())
      .filter(Boolean),
  };
}

interface Discovery {
  authorization_endpoint: string;
  token_endpoint: string;
  jwks_uri: string;
  issuer: string;
}
let discoveryCache: Discovery | null = null;

export async function discover(cfg: OidcConfig): Promise<Discovery> {
  if (discoveryCache) return discoveryCache;
  const res = await fetch(`${cfg.issuer}/.well-known/openid-configuration`);
  if (!res.ok) throw new Error(`oidc discovery failed: ${res.status}`);
  discoveryCache = (await res.json()) as Discovery;
  return discoveryCache;
}

export function authUrl(authEndpoint: string, cfg: OidcConfig, state: string, nonce: string): string {
  const p = new URLSearchParams({
    response_type: "code",
    client_id: cfg.clientId,
    redirect_uri: cfg.redirectUri,
    scope: "openid email profile",
    state,
    nonce,
  });
  return `${authEndpoint}?${p}`;
}

export async function exchangeCode(cfg: OidcConfig, tokenEndpoint: string, code: string): Promise<{ id_token: string }> {
  const body = new URLSearchParams({
    grant_type: "authorization_code",
    code,
    redirect_uri: cfg.redirectUri,
    client_id: cfg.clientId,
    client_secret: cfg.clientSecret,
  });
  const res = await fetch(tokenEndpoint, {
    method: "POST",
    headers: { "content-type": "application/x-www-form-urlencoded" },
    body,
  });
  if (!res.ok) throw new Error(`oidc token exchange failed: ${res.status}`);
  return (await res.json()) as { id_token: string };
}

const decodeSeg = (seg: string): Record<string, unknown> =>
  JSON.parse(Buffer.from(seg, "base64url").toString("utf8")) as Record<string, unknown>;

export interface IdClaims {
  email: string;
  email_verified?: boolean;
  name?: string;
}

/** Verify an ID token's signature (RS256 via JWKS) and iss/aud/exp/nonce. */
export async function verifyIdToken(
  cfg: OidcConfig,
  jwksUri: string,
  idToken: string,
  nonce: string,
): Promise<IdClaims> {
  const [h, p, s] = idToken.split(".");
  if (!h || !p || !s) throw new Error("malformed id_token");
  const header = decodeSeg(h) as { kid?: string; alg?: string };
  const payload = decodeSeg(p) as Record<string, unknown>;

  const jwks = (await (await fetch(jwksUri)).json()) as { keys: Array<Record<string, unknown> & { kid?: string }> };
  const jwk = jwks.keys.find((k) => k.kid === header.kid) ?? jwks.keys[0];
  if (!jwk) throw new Error("no jwks key");
  // eslint-disable-next-line @typescript-eslint/no-explicit-any -- node's JsonWebKeyInput.key type differs from the global JsonWebKey
  const key = createPublicKey({ key: jwk as any, format: "jwk" });
  const v = createVerify("RSA-SHA256");
  v.update(`${h}.${p}`);
  v.end();
  if (!v.verify(key, Buffer.from(s, "base64url"))) throw new Error("bad id_token signature");

  const iss = String(payload.iss ?? "").replace(/\/$/, "");
  if (iss !== cfg.issuer) throw new Error("iss mismatch");
  const aud = Array.isArray(payload.aud) ? (payload.aud as string[]) : [payload.aud as string];
  if (!aud.includes(cfg.clientId)) throw new Error("aud mismatch");
  if (Number(payload.exp) * 1000 < Date.now()) throw new Error("id_token expired");
  if (payload.nonce !== nonce) throw new Error("nonce mismatch");
  const email = payload.email as string | undefined;
  if (!email) throw new Error("no email in id_token");
  return { email, email_verified: payload.email_verified as boolean | undefined, name: payload.name as string | undefined };
}
