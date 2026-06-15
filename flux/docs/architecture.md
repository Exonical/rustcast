# Flux Architecture

## Overview

Flux follows a modular, layered architecture where each concern is isolated into its own crate. This enables:

- Independent testing of each subsystem
- Easy addition of new backends (e.g. new GPU vendors, capture methods)
- Clear dependency boundaries (no circular dependencies)
- Platform-specific code isolated behind trait interfaces

## Data Flow

### Server (Host) Pipeline

```
┌─────────────┐     ┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│   Screen     │     │   Color     │     │    Video     │     │   RTP       │
│   Capture    │────▶│   Convert   │────▶│   Encoder    │────▶│ Packetizer  │──▶ UDP
│  (DXGI/PW)  │     │  (GPU CSC)  │     │(NVENC/AMF/..)│     │  + FEC      │
└─────────────┘     └─────────────┘     └──────────────┘     └─────────────┘

┌─────────────┐     ┌─────────────┐
│   Audio     │     │    Opus     │
│   Capture   │────▶│   Encoder   │──────────────────────────────────────────▶ UDP
│(WASAPI/PW)  │     │             │
└─────────────┘     └─────────────┘

◀── UDP ──── Input Events ◀── Decrypt ◀── Deserialize ──▶ InputSink (inject)
```

### Client (Viewer) Pipeline

```
UDP ──▶ Depacketizer ──▶ FEC Reconstruct ──▶ Video Decoder ──▶ Renderer ──▶ Display
UDP ──▶ Opus Decoder ──▶ Audio Output
Input Capture ──▶ Serialize ──▶ Encrypt ──▶ UDP ──▶ Server
```

## Crate Dependency Graph

```
flux-server ──┬── flux-capture ──── flux-core
              ├── flux-encode  ──── flux-core
              ├── flux-audio   ──── flux-core
              ├── flux-input   ──── flux-core
              ├── flux-transport ── flux-core
              ├── flux-crypto  ──── flux-core
              └── flux-protocol ─── flux-core

flux-client ──┬── flux-transport ── flux-core
              ├── flux-crypto  ──── flux-core
              ├── flux-protocol ─── flux-core
              └── flux-input   ──── flux-core
```

## Key Design Decisions

### 1. Trait-Based Backend Abstraction

Every hardware-specific subsystem (capture, encoding, audio) is accessed through traits:

- `ScreenCapture` / `CaptureSession` — capture backends
- `VideoEncoder` / `EncodeSession` — encoder backends
- `AudioCaptureSession` / `AudioEncoder` — audio backends

This allows runtime backend selection and makes testing possible with mock implementations.

### 2. Zero-Copy GPU Pipeline

The ideal path keeps frame data on the GPU throughout the pipeline:

1. **Capture** produces a GPU handle (DXGI shared texture or DMA-BUF fd)
2. **Color conversion** runs as a Vulkan compute shader on the same GPU
3. **Encoding** consumes the GPU image directly (NVENC/AMF/VA-API/Vulkan Video)
4. **Only the compressed bitstream** is read back to CPU for network transmission

This avoids expensive GPU↔CPU copies that would add latency and consume bandwidth.

### 3. Dedicated Pipeline Thread

The video capture→encode loop runs on a dedicated OS thread (not a Tokio task) because:

- GPU API calls (NVENC, VA-API, Vulkan) are synchronous and may block
- Frame timing is critical — we don't want scheduler interference
- The thread communicates with async I/O via `crossbeam-channel` / `tokio::sync::mpsc`

### 4. QUIC for Signaling, UDP for Media

- **QUIC** (via `quinn`): Session negotiation, control messages, input events.
  Provides built-in TLS 1.3, multiplexed streams, and 0-RTT reconnection.
- **Raw UDP + RTP**: Video and audio media streams.
  Avoids QUIC congestion control overhead for real-time media.
  FEC provides loss resilience without retransmission latency.
- **QUIC Datagrams** (RFC 9221): Optional alternative media path that rides
  on the same QUIC connection, useful for NAT traversal.

### 5. Forward Error Correction (FEC)

Reed-Solomon erasure coding generates parity packets alongside data packets.
If any packets are lost in transit, the receiver can reconstruct missing data
without requesting retransmission — critical for maintaining low latency.

The FEC overhead is configurable (default 20%). For a 10-packet frame, 2
parity packets are generated, allowing recovery from any 2 lost packets.

## Session Lifecycle

```
1. Client connects via QUIC
2. Client sends Hello (protocol version, capabilities)
3. Server responds with Welcome (server capabilities)
4. If new client: PIN pairing flow
5. Client sends SessionRequest (codec, resolution, bitrate preferences)
6. Server validates, allocates resources, starts pipeline
7. Server responds with SessionAccepted (negotiated params, port assignments)
8. Client connects to video/audio UDP ports
9. Streaming begins
10. Keepalive ping/pong maintains the session
11. Either side can send SessionEnd for graceful teardown
```

## Encoder Selection Priority

The server selects the best available encoder at startup:

1. **NVENC** (NVIDIA GPUs) — lowest latency, best quality per bit
2. **AMF** (AMD GPUs on Windows) — good quality, low latency
3. **VA-API** (AMD/Intel GPUs on Linux) — standard Linux hardware encode
4. **Vulkan Video** (any vendor) — portable, emerging standard
5. **Software** (CPU) — last resort fallback

## Security Model

- All signaling uses TLS 1.3 (via QUIC)
- New clients must pair via a 4-digit PIN displayed on the host
- Paired clients are identified by their TLS certificate fingerprint (SHA-256)
- Control messages and input events are encrypted with AES-128-GCM
- Video/audio RTP can optionally be encrypted (configurable, off by default for performance)
