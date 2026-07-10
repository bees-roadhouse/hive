#!/usr/bin/env bash
# Provision the CI Stalwart container for mail/tests/stalwart_e2e.rs:
# wait for health, create the example.test domain + the mailtest individual
# account over the management API, then prove the account can actually
# complete JMAP session discovery (the exact call the daemon makes first).
#
# Idempotent: principal creates answer HTTP 200 either way, with {"data":N}
# on success or {"error":"fieldAlreadyExists"} on a re-run — both are fine.
# Anything else is a hard fail. Credentials must match ci/stalwart/config.toml
# (admin) and the HIVE_TEST_STALWART_USER/PASS env the e2e job passes (account).
set -euo pipefail

STALWART_URL="${STALWART_URL:-http://localhost:8080}"
ADMIN_USER="${STALWART_ADMIN_USER:-admin}"
ADMIN_PASS="${STALWART_ADMIN_PASS:-ci-admin-pass}"
MAIL_DOMAIN="${STALWART_MAIL_DOMAIN:-example.test}"
MAIL_USER="${STALWART_MAIL_USER:-mailtest@example.test}"
MAIL_PASS="${STALWART_MAIL_PASS:-mailtest-pass}"

echo "waiting for stalwart at ${STALWART_URL} ..."
for i in $(seq 1 60); do
  if curl -sf -m 2 "${STALWART_URL}/healthz/ready" >/dev/null; then
    echo "stalwart ready after ${i}s"
    break
  fi
  if [ "$i" -eq 60 ]; then
    echo "stalwart never became ready" >&2
    exit 1
  fi
  sleep 1
done

# POST /api/principal (verified against stalwartlabs/stalwart:v0.15.5 —
# domains and individuals are both principals since the 0.11 RBAC rework).
create_principal() {
  local payload="$1" label="$2" body
  body=$(curl -sS -m 10 -u "${ADMIN_USER}:${ADMIN_PASS}" \
    -X POST "${STALWART_URL}/api/principal" \
    -H 'Content-Type: application/json' \
    -d "${payload}")
  case "${body}" in
    *'"data"'*) echo "created ${label}" ;;
    *fieldAlreadyExists*) echo "${label} already exists (ok)" ;;
    *)
      echo "creating ${label} failed: ${body}" >&2
      exit 1
      ;;
  esac
}

create_principal \
  "{\"type\":\"domain\",\"name\":\"${MAIL_DOMAIN}\",\"description\":\"CI mail e2e domain\"}" \
  "domain ${MAIL_DOMAIN}"

create_principal \
  "{\"type\":\"individual\",\"name\":\"${MAIL_USER}\",\"secrets\":[\"${MAIL_PASS}\"],\"emails\":[\"${MAIL_USER}\"],\"description\":\"CI mail e2e account\",\"roles\":[\"user\"]}" \
  "account ${MAIL_USER}"

# The account must be able to do what hive-mail does first: session discovery.
if ! curl -sfL -m 10 -u "${MAIL_USER}:${MAIL_PASS}" \
  "${STALWART_URL}/.well-known/jmap" >/dev/null; then
  echo "JMAP session discovery as ${MAIL_USER} failed" >&2
  exit 1
fi
echo "provisioned: ${MAIL_USER} can complete JMAP session discovery"
