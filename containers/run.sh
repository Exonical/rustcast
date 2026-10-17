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

podman kube down "$here/flux-pod.yaml" >/dev/null 2>&1 || true
podman kube play "$pod_yaml"

echo "Relay UI: http://localhost:8080"
if [ -n "$cert_dir" ]; then
  echo "Relay UI: https://localhost:8443"
  if [ -n "$public_host" ]; then
    echo "Relay UI: https://$public_host:8443"
  fi
fi
