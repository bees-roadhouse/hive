-- Auth Phase 9 (hive-auth-mcp-design.md §6, §6.1): the external-IdP federation
-- SEAM. This is the LAST phase of the 9-phase plan and is deliberately a seam,
-- not a provider: it lands the one table the external-authZ branch reads, so a
-- concrete OIDC provider drops in later as config + a thin adapter, with no
-- enforcement rewrite.
--
-- What already exists (do NOT re-add): users.external_idp + users.external_sub
-- (migration 0005) link a local user to an external identity (null in builtin
-- mode); auth_policy.auth_mode (builtin|external) + auth_policy.authz_mode
-- (internal|external) (migration 0005) are the mode switches. Phase 9 only adds
-- the mapping table + wires the trait/chokepoint around these existing columns.
--
-- idp_permission_map: maps an external IdP claim value (e.g. a group/role) to
-- hive's INTERNAL permission vocabulary (scopes + data_visibility + is_admin).
-- INERT until auth_policy.authz_mode = 'external' AND a provider populates it.
-- Empty map = deny-by-default for external authZ (the resolver grants nothing
-- for an unmapped claim), so turning the mode on with no rows fails closed.

BEGIN;

CREATE TABLE idp_permission_map (
  id              uuid PRIMARY KEY DEFAULT gen_uuid_v7(),
  provider        TEXT NOT NULL,                 -- matches users.external_idp (e.g. 'authentik')
  claim           TEXT NOT NULL,                 -- which claim carries roles, e.g. 'groups' | 'roles'
  claim_value     TEXT NOT NULL,                 -- e.g. 'hive-admins' | 'household'
  grant_scopes    TEXT[] NOT NULL DEFAULT '{}',  -- -> ResolvedPermissions.scopes
  data_visibility TEXT CHECK (data_visibility IS NULL OR data_visibility IN ('shared','owner','custom')),
  is_admin        BOOLEAN NOT NULL DEFAULT FALSE,
  priority        INTEGER NOT NULL DEFAULT 100,  -- merge order when a user matches multiple rows
  created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- A (provider, claim, claim_value) maps to exactly one row.
  UNIQUE (provider, claim, claim_value)
);
CREATE INDEX idp_permission_map_lookup_idx ON idp_permission_map(provider, claim);

COMMIT;
