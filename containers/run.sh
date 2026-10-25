#!/usr/bin/env bash
# Build the relay and host images and (re)start the Flux pod.
#
# FLUX_WT_PUBLIC_HOST overrides the host advertised by the relay. When unset
# and a podman machine (WSL2) is in use, the VM's eth0 address is used because
# WSL2 does not forward published UDP ports to Windows localhost.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"

podman build -f "$here/relay/Containerfile" -t localhost/flux-relay:latest "$root"
podman build -f "$here/host/Containerfile" -t localhost/flux-host:latest "$root"

public_host="${FLUX_WT_PUBLIC_HOST:-}"
if [ -z "$public_host" ] && podman machine inspect >/dev/null 2>&1; then
  public_host="$(podman machine ssh "ip -4 -o addr show eth0 | awk '{print \$4}' | cut -d/ -f1" 2>/dev/null | tr -d '\r\n' || true)"
fi

# Authentik/OIDC secrets live outside git; generate them once per checkout.
env_file="$here/.env.authentik"
if [ ! -f "$env_file" ]; then
  gen_secret() { LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 48; }
  {
    echo "AUTHENTIK_SECRET_KEY=$(gen_secret)"
    echo "PG_PASS=$(gen_secret)"
    echo "AUTHENTIK_BOOTSTRAP_PASSWORD=$(gen_secret)"
    echo "AUTHENTIK_BOOTSTRAP_TOKEN=$(gen_secret)"
    echo "FLUX_OIDC_CLIENT_SECRET=$(gen_secret)"
    echo "FLUX_SESSION_SECRET=$(gen_secret)"
    echo "FLUX_RELAY_TOKEN=$(gen_secret)"
    echo "FLUX_DEVUSER_PASSWORD=$(gen_secret)"
    echo "FLUX_AK_ENROLLMENT_TOKEN=$(gen_secret)"
  } > "$env_file"
  chmod 600 "$env_file"
  echo "Generated $env_file (akadmin password, OIDC secrets; gitignored)."
else
  # Backfill keys added after the file was first generated.
  gen_secret() { LC_ALL=C tr -dc 'a-f0-9' </dev/urandom | head -c 48; }
  for key in FLUX_DEVUSER_PASSWORD FLUX_AK_ENROLLMENT_TOKEN FLUX_LDAP_OUTPOST_TOKEN FLUX_LDAP_BIND_PASSWORD; do
    if ! grep -q "^$key=" "$env_file"; then
      echo "$key=$(gen_secret)" >> "$env_file"
    fi
  done
fi
set -a
# shellcheck disable=SC1090
. "$env_file"
set +a

pod_yaml="$here/flux-pod.yaml"
cert_dir=""
if command -v mkcert >/dev/null 2>&1; then
  mkcert -install
  cert_dir="$here/certs"
  mkdir -p "$cert_dir"
  host_marker="$cert_dir/public-host.txt"
  existing_public_host=""
  if [ -f "$host_marker" ]; then
    IFS= read -r existing_public_host < "$host_marker" || true
  fi
  if [ ! -s "$cert_dir/relay.pem" ] || [ ! -s "$cert_dir/relay-key.pem" ] || [ "$existing_public_host" != "$public_host" ]; then
    sans=(localhost 127.0.0.1 ::1)
    if [ -n "$public_host" ]; then
      sans+=("$public_host")
    fi
    mkcert -cert-file "$cert_dir/relay.pem" -key-file "$cert_dir/relay-key.pem" "${sans[@]}"
    printf '%s\n' "$public_host" > "$host_marker"
  fi
else
  echo "mkcert not found; install with 'winget install FiloSottile.mkcert' and run 'mkcert -install' to enable trusted HTTPS."
fi

if [ -n "$public_host" ] || [ -n "$cert_dir" ]; then
  pod_yaml="$(mktemp --suffix=.yaml)"
  cert_file=""
  cert_key=""
  if [ -n "$cert_dir" ]; then
    cert_file="/etc/flux/certs/relay.pem"
    cert_key="/etc/flux/certs/relay-key.pem"
  fi
  awk -v public_host="$public_host" -v cert_file="$cert_file" -v cert_key="$cert_key" -v cert_dir="$cert_dir" '
    /^        - name: FLUX_WT_PUBLIC_HOST$/ {
      print
      getline
      if (public_host != "") sub(/value: ".*"/, "value: \"" public_host "\"")
      print
      if (cert_file != "") {
        print "        - name: FLUX_TLS_CERT"
        print "          value: \"" cert_file "\""
        print "        - name: FLUX_TLS_KEY"
        print "          value: \"" cert_key "\""
      }
      next
    }
    /^    - name: relay$/ { in_relay = 1 }
    /^    - name: host$/ { in_relay = 0 }
    in_relay && cert_file != "" && /^      ports:$/ {
      print "      volumeMounts:"
      print "        - name: relay-certs"
      print "          mountPath: /etc/flux/certs"
      print "          readOnly: true"
    }
    { print }
    cert_file != "" && /^  volumes:$/ {
      print "    - name: relay-certs"
      print "      hostPath:"
      print "        path: \"" cert_dir "\""
      print "        type: Directory"
    }
  ' "$here/flux-pod.yaml" > "$pod_yaml"
fi
if [ -n "$public_host" ]; then
  echo "WebTransport public host: $public_host"
fi

templated="$(mktemp --suffix=.yaml)"
sed \
  -e "s|%%AUTHENTIK_SECRET_KEY%%|$AUTHENTIK_SECRET_KEY|g" \
  -e "s|%%PG_PASS%%|$PG_PASS|g" \
  -e "s|%%AUTHENTIK_BOOTSTRAP_PASSWORD%%|$AUTHENTIK_BOOTSTRAP_PASSWORD|g" \
  -e "s|%%AUTHENTIK_BOOTSTRAP_TOKEN%%|$AUTHENTIK_BOOTSTRAP_TOKEN|g" \
  -e "s|%%FLUX_OIDC_CLIENT_SECRET%%|$FLUX_OIDC_CLIENT_SECRET|g" \
  -e "s|%%FLUX_SESSION_SECRET%%|$FLUX_SESSION_SECRET|g" \
  -e "s|%%FLUX_RELAY_TOKEN%%|$FLUX_RELAY_TOKEN|g" \
  -e "s|%%FLUX_DEVUSER_PASSWORD%%|$FLUX_DEVUSER_PASSWORD|g" \
  -e "s|%%FLUX_AK_ENROLLMENT_TOKEN%%|$FLUX_AK_ENROLLMENT_TOKEN|g" \
  -e "s|%%FLUX_LDAP_OUTPOST_TOKEN%%|$FLUX_LDAP_OUTPOST_TOKEN|g" \
  -e "s|%%FLUX_LDAP_BIND_PASSWORD%%|$FLUX_LDAP_BIND_PASSWORD|g" \
  -e "s|%%BLUEPRINT_DIR%%|$here/authentik/blueprints|g" \
  "$pod_yaml" > "$templated"
pod_yaml="$templated"

podman kube down "$here/flux-pod.yaml" >/dev/null 2>&1 || true
# SELinux hosts: the blueprint dir must be readable by the authentik containers.
command -v chcon >/dev/null 2>&1 && chcon -Rt container_file_t -l s0 "$here/authentik/blueprints" 2>/dev/null || true
podman kube play "$pod_yaml"

# Pin the LDAP outpost's API token once Authentik is up (best-effort; the
# script waits for readiness and the blueprint-applied outpost itself).
nohup bash "$here/ak-ldap-bootstrap.sh" >/tmp/flux-ak-ldap-bootstrap.log 2>&1 &

echo "Relay UI: http://localhost:8080"
if [ -n "$cert_dir" ]; then
  echo "Relay UI: https://localhost:8443"
  if [ -n "$public_host" ]; then
    echo "Relay UI: https://$public_host:8443"
  fi
fi
