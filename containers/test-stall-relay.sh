#!/usr/bin/env bash
# Sums the relay's keyframe-request reasons (granted + rate-limited) and
# recoveries for one upstream between two UTC times. Every frame skipped while
# waiting for a keyframe files one awaiting-idr request, so that count is the
# number of frames the viewer never received.
#   ./containers/test-stall-relay.sh default 2026-10-04T08:27:00Z 2026-10-04T08:28:30Z
set -eu
id=$1 since=$2 until=$3
log=$(MSYS_NO_PATHCONV=1 podman logs --since "$since" --until "$until" flux-relay 2>&1)
reasons=$(grep -F "[idr:$id]" <<<"$log" | grep -oE '[a-z-]+=[0-9]+\(\+[0-9]+' |
  awk -F'[=(+]' '{s[$1] += $2 + $4} END {for (r in s) printf "%s=%d ", r, s[r]}')
echo "recovery-idr=$(grep -c -F "[webrtc:$id] recovery IDR" <<<"$log" || true) ${reasons:-no-idr-requests}"
