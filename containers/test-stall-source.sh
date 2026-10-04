#!/usr/bin/env bash
# Prepares the stall-test source inside the running flux-host container: a
# looping 720p30 test clip in Firefox (kiosk) with native player controls, so
# mouse movement over the video shows/hides hover UI as it does for a real user.
# The page prints its own cadence to /tmp/firefox.log once a second
# (FLUXSRC lines: rAF, presented and dropped video frames).
# ffmpeg is installed into the running container only to generate the clip.
set -euo pipefail
export MSYS_NO_PATHCONV=1
host=flux-host
session='export XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-0 DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus MOZ_ENABLE_WAYLAND=1'

podman exec "$host" sh -c 'rpm -q ffmpeg >/dev/null 2>&1 || dnf -y -q --setopt=install_weak_deps=False install ffmpeg >/dev/null'
podman exec "$host" su flux -c 'test -s /home/flux/test.mp4 || ffmpeg -hide_banner -loglevel error -y -f lavfi -i testsrc2=size=1280x720:rate=30 -t 240 -c:v libx264 -preset veryfast -pix_fmt yuv420p -g 60 /home/flux/test.mp4'
podman exec -i "$host" su flux -c 'cat > /home/flux/stall-source.html' <<'EOF'
<!doctype html>
<meta charset="utf-8">
<title>flux-stall-source</title>
<style>html,body{margin:0;background:#000;overflow:hidden}video{position:fixed;inset:0;width:100vw;height:100vh;object-fit:contain}</style>
<video id="v" src="test.mp4" autoplay loop muted controls></video>
<script>
const v = document.getElementById('v');
let raf = 0, presented = 0, maxGap = 0, last = performance.now();
const tick = () => { raf++; const now = performance.now(); maxGap = Math.max(maxGap, now - last); last = now; requestAnimationFrame(tick); };
requestAnimationFrame(tick);
const onFrame = () => { presented++; v.requestVideoFrameCallback(onFrame); };
v.requestVideoFrameCallback(onFrame);
let prevQ = v.getVideoPlaybackQuality();
setInterval(() => {
  const q = v.getVideoPlaybackQuality();
  console.log(`FLUXSRC ${new Date().toISOString()} raf=${raf} rafMaxGapMs=${maxGap.toFixed(0)} presented=${presented} dropped=${q.droppedVideoFrames - prevQ.droppedVideoFrames} total=${q.totalVideoFrames - prevQ.totalVideoFrames} t=${v.currentTime.toFixed(2)}`);
  raf = 0; presented = 0; maxGap = 0; prevQ = q;
}, 1000);
</script>
EOF
# Firefox writes page console output to stdout only with these prefs; the
# profile exists once Firefox has started at least once.
podman exec "$host" su flux -c "$session; timeout 8 firefox --headless about:blank >/dev/null 2>&1 || true"
podman exec "$host" su flux -c 'for p in /home/flux/.mozilla/firefox/*.default*; do printf "user_pref(\"devtools.console.stdout.content\", true);\n" > "$p/user.js"; done'
podman exec "$host" sh -c 'pkill -x firefox || true; sleep 2'
podman exec -d "$host" su flux -c "$session; exec firefox --kiosk file:///home/flux/stall-source.html >/tmp/firefox.log 2>&1"
echo "Source running; cadence: podman exec $host grep FLUXSRC /tmp/firefox.log"
