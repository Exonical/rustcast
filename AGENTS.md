# Agent notes

## Layout
- `flux/` Rust workspace (server, capture, encode, input, transport, client). `flux-web/` Go WebTransport (HTTP/3) relay + Next.js viewer (`flux-web/ui`). `drivers/flux-idd/` Rust IddCx driver. `containers/` Rocky 10 GNOME host + relay pod. The host image builds Mesa 25.3.6 RADV/ANV with video codecs enabled (stage `mesa` in containers/host/Containerfile — Rocky strips VK_KHR_video_*) and flux-pod.yaml passes the host's /dev/dri render nodes through as CharDevice volumes. Desktop audio: the host image creates a PipeWire null sink `flux-sink` (rootfs/etc/pipewire/pipewire.conf.d) that flux-server captures and Opus-encodes (feature `audio-pipewire`); it travels as frame-server message type 0x04 and WebTransport uni-stream kind 0x04 to the browser's AudioWorklet player.

## Build / test
- Relay: `cd flux-web && go vet ./... && go test ./...` (Go 1.27; the Windows checkout is CRLF, so `gofmt -l` flags every file — check an LF copy instead).
- Rust host build runs inside `containers/host/Containerfile` (features `capture-mutter,encoder-ffmpeg,encoder-vulkan`); Cargo workspace root is `flux/`. Encoder check: `podman exec flux-host su flux -c 'XDG_RUNTIME_DIR=/run/user/1000 journalctl --user -u flux-server | grep "Encoder created"'`.
- Metrics: relay `FLUX_METRICS_ADDR` defaults to `:9101`, host defaults to `127.0.0.1:9102`; metric prefixes are `flux_relay_`, `flux_client_`, and `flux_host_`.
- UI package operations use `npx -y pnpm@12.9.1 ...` (or corepack; `packageManager` is pinned in package.json). Dependency overrides and the `allowBuilds` list live in `flux-web/ui/pnpm-workspace.yaml`.

## Deploy (Windows, Podman machine on WSL2; no Docker)
- Full rebuild + pod replace: `./containers/run.sh` (auto-detects the WSL VM IP for `FLUX_WT_PUBLIC_HOST`; re-run after WSL restarts).
- Relay-only swap keeping the host container: `podman build -f containers/relay/Containerfile -t localhost/flux-relay:latest . && podman rm -f flux-relay && podman run -d --pod flux --name flux-relay -e FLUX_WT_PUBLIC_HOST=<vm-ip> --user flux-web localhost/flux-relay:latest`
- On Windows, install mkcert with `winget install FiloSottile.mkcert`, run `mkcert -install`, then `./containers/run.sh`. The script creates or reuses `containers/certs/relay.pem` and `relay-key.pem`, adds localhost and the advertised VM address as SANs, mounts them read-only, and prints trusted UI URLs at `https://localhost:8443` and (when available) `https://<vm-ip>:8443`. HTTP remains available on port 8080. Without mkcert, the script prints the install hint and preserves HTTP-only behavior.
- The same `FLUX_TLS_CERT` and `FLUX_TLS_KEY` configure WebTransport and the Gin UI/API HTTPS listener; HTTPS defaults to `:8443` and can be disabled with `FLUX_HTTPS_ADDR=off`. The relay still rotates its self-signed WebTransport certificate when neither file is configured.
- For WSL2 localhost UDP forwarding, add `[wsl2]` and `networkingMode=mirrored` to `%UserProfile%\.wslconfig`, then set `FLUX_WT_PUBLIC_HOST=127.0.0.1` before running the script. Without mirrored networking, use the detected VM IP and its trusted certificate URL; Chrome flags are not needed.
- From Git Bash, prefix podman commands that pass absolute container paths with `MSYS_NO_PATHCONV=1`.
- Server log: `MSYS_NO_PATHCONV=1 podman exec flux-host su flux -c 'XDG_RUNTIME_DIR=/run/user/1000 journalctl --user -u flux-server'`; relay: `podman logs flux-relay`.

## Stall / smoothness measurement
- `bash containers/test-stall-source.sh` (looping test clip with player controls in the container's Firefox), then
  `node containers/test-stall.js "machine=default&movePattern=burst&phases=idle:10,mouse:45,idle:10,mouse:45,idle:10" report.json`
  and `bash containers/test-stall-relay.sh default <since-UTC> <until-UTC>` for relay keyframe-recovery counts.
- The server's `Perf: N fps` is a cumulative average since start, not the current rate.
