#!/usr/bin/env bash
# Build the relay and host images and (re)start the Flux pod.
#
# FLUX_ICE_PUBLIC_IPS overrides the WebRTC host-candidate IP advertised by the
# relay. When unset and a podman machine (WSL2) is in use, the VM's eth0 address
# is used: WSL2 forwards published TCP ports to Windows localhost but not UDP,
# so a Windows browser must reach the media port at the VM address. Otherwise
# the value in flux-pod.yaml (127.0.0.1) is kept.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(dirname "$here")"

podman build -f "$here/relay/Containerfile" -t localhost/flux-relay:latest "$root"
podman build -f "$here/host/Containerfile" -t localhost/flux-host:latest "$root"

ice_ips="${FLUX_ICE_PUBLIC_IPS:-}"
if [ -z "$ice_ips" ] && podman machine inspect >/dev/null 2>&1; then
  ice_ips="$(podman machine ssh "ip -4 -o addr show eth0 | awk '{print \$4}' | cut -d/ -f1" 2>/dev/null | tr -d '\r\n' || true)"
fi

pod_yaml="$here/flux-pod.yaml"
if [ -n "$ice_ips" ]; then
  pod_yaml="$(mktemp --suffix=.yaml)"
  sed -E "/FLUX_ICE_PUBLIC_IPS/{n;s|value: \".*\"|value: \"$ice_ips\"|}" "$here/flux-pod.yaml" > "$pod_yaml"
  echo "WebRTC host candidate IP: $ice_ips"
fi

podman kube down "$here/flux-pod.yaml" >/dev/null 2>&1 || true
podman kube play "$pod_yaml"

echo "Relay UI: http://localhost:8080"
