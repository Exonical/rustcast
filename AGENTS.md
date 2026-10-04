# Agent notes

## Layout
- `flux/` Rust workspace (server, capture, encode, input, transport, client). `flux-web/` Go WebRTC relay + Next.js viewer (`flux-web/ui`). `drivers/flux-idd/` Rust IddCx driver. `containers/` Rocky 10 GNOME host + relay pod.

## Build / test
- Relay: `cd flux-web && go vet ./... && go test ./...` (Go 1.27; the Windows checkout is CRLF, so `gofmt -l` flags every file — check an LF copy instead).
- Rust host build runs inside `containers/host/Containerfile` (features `capture-mutter,encoder-ffmpeg`); Cargo workspace root is `flux/`.

## Deploy (Windows, Podman machine on WSL2; no Docker)
- Full rebuild + pod replace: `./containers/run.sh` (auto-detects the WSL VM IP for `FLUX_ICE_PUBLIC_IPS`; re-run after WSL restarts).
- Relay-only swap keeping the host container: `podman build -f containers/relay/Containerfile -t localhost/flux-relay:latest . && podman rm -f flux-relay && podman run -d --pod flux --name flux-relay -e FLUX_ICE_PUBLIC_IPS=<vm-ip> --user flux-web localhost/flux-relay:latest`
- From Git Bash, prefix podman commands that pass absolute container paths with `MSYS_NO_PATHCONV=1`.
- Server log: `MSYS_NO_PATHCONV=1 podman exec flux-host su flux -c 'XDG_RUNTIME_DIR=/run/user/1000 journalctl --user -u flux-server'`; relay: `podman logs flux-relay`.

## Stall / smoothness measurement
- `bash containers/test-stall-source.sh` (looping test clip with player controls in the container's Firefox), then
  `node containers/test-stall.js "machine=default&movePattern=burst&phases=idle:10,mouse:45,idle:10,mouse:45,idle:10" report.json`
  and `bash containers/test-stall-relay.sh default <since-UTC> <until-UTC>` for relay keyframe-recovery counts.
- The server's `Perf: N fps` is a cumulative average since start, not the current rate.
