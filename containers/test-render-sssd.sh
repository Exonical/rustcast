#!/usr/bin/env bash
# Checks flux-render-sssd output for loopback, remote ldap:// and ldaps:// URIs.
set -u
here="$(cd "$(dirname "$0")" && pwd)"
render="$here/host/rootfs/usr/local/bin/flux-render-sssd"
fail=0
check() { # name uri want-pattern... (prefix ! = must be absent)
  local name=$1 uri=$2; shift 2
  local out; out=$(FLUX_LDAP_URI=$uri FLUX_LDAP_CA_CERT= "$render" pw)
  for p in "$@"; do
    if [ "${p:0:1}" = "!" ]; then
      if grep -qF -- "${p:1}" <<<"$out"; then echo "FAIL $name: unexpected '${p:1}'"; fail=1; fi
    elif ! grep -qF -- "$p" <<<"$out"; then echo "FAIL $name: missing '$p'"; fail=1; fi
  done
}
for u in ldap://127.0.0.1:3389 ldap://localhost:3389 'ldap://[::1]:3389'; do
  check "loopback $u" "$u" "ldap_auth_disable_tls_never_use_in_production = true" "ldap_id_use_start_tls = false" "!ldap_tls_reqcert"
done
check remote ldap://ldap.example.com "ldap_id_use_start_tls = true" "ldap_tls_reqcert = demand" "!never_use_in_production"
check ldaps ldaps://ldap.example.com "ldap_tls_reqcert = demand" "ldap_tls_cacert = /etc/pki/tls/certs/ca-bundle.crt" "!never_use_in_production" "!start_tls = true"
[ $fail = 0 ] && echo "render-sssd: all ok"
exit $fail
