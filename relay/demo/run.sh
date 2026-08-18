#!/usr/bin/env bash
# End-to-end demo, and the evidence for the one claim that matters.
#
#   browser/curl --TLS--> relay :8443 --splice--> agent (terminates TLS) --> hive-api
#
# Both taps record the SAME session. The relay's copy is ciphertext; the
# instance's copy is the plaintext. Grepping both for a canary is the proof:
# it appears in one file and not the other.
#
# Prerequisite: a hive-api listening on $TARGET (default 127.0.0.1:7979).
# From a cold machine:
#
#   podman run -d --name hive-relay-pg -e POSTGRES_USER=hive \
#     -e POSTGRES_PASSWORD=hive -e POSTGRES_DB=hive -p 55432:5432 \
#     docker.io/pgvector/pgvector:pg17
#   cargo build -p hive-api -p hive-relay
#   DATABASE_URL=postgres://hive:hive@localhost:55432/hive PORT=7981 \
#     HIVE_EMBED=hash ./target/debug/hive-api &
#   TARGET=127.0.0.1:7981 ./relay/demo/run.sh
#
# `*.localtest.me` resolves to 127.0.0.1 from public DNS, so the demo needs no
# hosts-file edit to exercise a real subdomain.
set -uo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
# BIN_DIR overrides where the built binaries live; the default is the
# workspace's own target/debug. Set it when CARGO_TARGET_DIR pointed elsewhere.
bin="${BIN_DIR:-$here/../../target/debug}"
# The binaries carry a .exe suffix on Windows (git bash counts).
exe() { case "${OSTYPE:-}" in msys* | cygwin* | win32) printf '%s.exe' "$1" ;; *) printf '%s' "$1" ;; esac; }
id="${ID:-hv7bqk2m9x}"
zone="${ZONE:-relay.localtest.me}"
host="$id.$zone"
target="${TARGET:-127.0.0.1:7979}"
token="s3cret-registration-token"
canary="CANARY-$(date +%s)-loganton-pennsylvania"

relay_tap="$here/relay-observed.bin"
plain_tap="$here/instance-plaintext.bin"

cleanup() {
  [[ -n "${relay_pid:-}" ]] && kill "$relay_pid" 2>/dev/null
  [[ -n "${agent_pid:-}" ]] && kill "$agent_pid" 2>/dev/null
  return 0
}
trap cleanup EXIT

bash "$here/make-pki.sh" "$id" "$zone" >/dev/null
echo "== certificate issued to the INSTANCE for $host (the relay has no key for it)"

HIVE_RELAY_INGRESS=0.0.0.0:8443 \
HIVE_RELAY_CONTROL=127.0.0.1:7500 \
HIVE_RELAY_ADMIN=127.0.0.1:7501 \
HIVE_RELAY_ZONE="$zone" \
HIVE_RELAY_TOKENS="$id=$token" \
HIVE_RELAY_AUDIT_TAP="$relay_tap" \
RUST_LOG=hive_relay=info \
  "$bin/$(exe hive-relay)" >"$here/relay.log" 2>&1 &
relay_pid=$!

sleep 2

HIVE_RELAY_ENABLED=1 \
HIVE_RELAY_URL=127.0.0.1:7500 \
HIVE_RELAY_INSTANCE="$id" \
HIVE_RELAY_TOKEN="$token" \
HIVE_RELAY_TARGET="$target" \
HIVE_RELAY_CERT="$here/pki/instance.crt" \
HIVE_RELAY_KEY="$here/pki/instance.key" \
HIVE_RELAY_LABEL="The Roadhouse" \
HIVE_RELAY_PLAINTEXT_TAP="$plain_tap" \
RUST_LOG=hive_relay=info \
  "$bin/$(exe hive-relay-agent)" >"$here/agent.log" 2>&1 &
agent_pid=$!

sleep 3

echo
echo "== a real request, from outside, through the relay"
echo "   GET https://$host:8443/api/healthz?trace=$canary"
curl -sS -m 20 --ssl-revoke-best-effort --cacert "$here/pki/ca.crt" \
  -H "X-Trace: $canary" \
  -w '\n   [http %{http_code}] [peer %{remote_ip}:%{remote_port}]\n' \
  "https://$host:8443/api/healthz?trace=$canary"

sleep 1

echo
echo "== what the RELAY operator can see"
curl -sS -m 5 http://127.0.0.1:7501/status
echo

echo
echo "== the two captures of that same session"
ls -l "$relay_tap" "$plain_tap" 2>/dev/null | awk '{print "   " $5 " bytes  " $NF}'

fail=0
check_absent() {
  local needle="$1" file="$2" what="$3"
  if grep -qa -- "$needle" "$file"; then
    echo "   FAIL  $what found in $(basename "$file")"
    fail=1
  else
    echo "   ok    $what absent from $(basename "$file")"
  fi
}
check_present() {
  local needle="$1" file="$2" what="$3"
  if grep -qa -- "$needle" "$file"; then
    echo "   ok    $what present in $(basename "$file")"
  else
    echo "   FAIL  $what missing from $(basename "$file") ... the grep is not working"
    fail=1
  fi
}

echo
echo "== control: the canary IS in the plaintext, so the grep works"
check_present "$canary" "$plain_tap" "canary"
check_present "GET /api/healthz" "$plain_tap" "request line"
check_present "hive-rust" "$plain_tap" "response body"

echo
echo "== the claim: none of it is in what the relay forwarded"
check_absent "$canary" "$relay_tap" "canary"
check_absent "GET /api/healthz" "$relay_tap" "request line"
check_absent "HTTP/1.1" "$relay_tap" "protocol version"
check_absent "hive-rust" "$relay_tap" "response body"
check_absent "X-Trace" "$relay_tap" "request header"

echo
echo "== what the relay's bytes actually are"
echo "   first 48 bytes of the client's first record:"
head -c 400 "$relay_tap" | tail -c +47 | head -c 48 | od -A d -t x1 | head -3
echo "   0x16 = TLS handshake, 0x0301 = record version, then the ClientHello."
echo "   The SNI inside it is the only thing the relay reads."

echo
if [[ $fail -eq 0 ]]; then
  echo "RESULT: the relay carried the session and could not read it."
else
  echo "RESULT: FAILED ... see above."
fi
exit $fail
