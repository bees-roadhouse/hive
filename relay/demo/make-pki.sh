#!/usr/bin/env bash
# Demo PKI.
#
# This throwaway CA stands in for the publicly-trusted certificate a real
# instance would obtain itself. In production the instance runs ACME with a
# DNS-01 challenge against the relay zone, and the KEY IS GENERATED IN THE
# HOUSE and never sent anywhere. A relay that ever held an instance's private
# key could impersonate that instance later, which would quietly turn "the
# relay cannot read your journal" into "the relay cannot read your journal
# today".
set -euo pipefail

# Git Bash rewrites anything that looks like a unix path, which turns
# `-subj /CN=...` into `C:/Program Files/Git/CN=...`.
export MSYS2_ARG_CONV_EXCL='*'

here="$(cd "$(dirname "$0")" && pwd)"
pki="$here/pki"
id="${1:-hv7bqk2m9x}"
zone="${2:-relay.localtest.me}"
host="$id.$zone"

rm -rf "$pki"
mkdir -p "$pki"
cd "$pki"

openssl req -x509 -newkey rsa:2048 -sha256 -days 2 -nodes \
  -keyout ca.key -out ca.crt -subj "/CN=hive relay demo CA" 2>/dev/null

openssl req -newkey rsa:2048 -nodes -keyout instance.key -out instance.csr \
  -subj "/CN=$host" 2>/dev/null

printf 'subjectAltName=DNS:%s\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n' \
  "$host" > ext.cnf

openssl x509 -req -in instance.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
  -out instance.crt -days 2 -sha256 -extfile ext.cnf 2>/dev/null

echo "issued for $host"
openssl x509 -in instance.crt -noout -subject -issuer -ext subjectAltName
