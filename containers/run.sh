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
if [ -n "$public_host" ]; then
  pod_yaml="$(mktemp --suffix=.yaml)"
  sed -E "/FLUX_WT_PUBLIC_HOST/{n;s|value: \".*\"|value: \"$public_host\"|}" "$here/flux-pod.yaml" > "$pod_yaml"
  echo "WebTransport public host: $public_host"
fi

podman kube down "$here/flux-pod.yaml" >/dev/null 2>&1 || true
podman kube play "$pod_yaml"

echo "Relay UI: http://localhost:8080"
