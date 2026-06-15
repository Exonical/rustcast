# Flux — GPU-Accelerated Remote Streaming

Flux is a high-performance remote desktop streaming solution built in Rust, designed for ultra-low-latency desktop capture, encoding, and transmission across networks. It supports GPU-accelerated video encoding on both NVIDIA and AMD GPUs, and runs on Windows and Linux.

## Features

- **GPU-Accelerated Encoding** — NVENC (NVIDIA), AMF (AMD/Windows), VA-API (AMD+Intel/Linux), Vulkan Video (cross-vendor)
- **Zero-Copy Capture** — DXGI Desktop Duplication (Windows), PipeWire/DRM (Linux) with DMA-BUF passthrough
- **Low-Latency Transport** — RTP over UDP with Reed-Solomon FEC, QUIC for signaling and reliable control
- **Multi-Codec** — H.264, H.265/HEVC, AV1 (where hardware supports it)
- **HDR Support** — 10-bit encoding with BT.2020 color space
- **Full Input Forwarding** — Keyboard, mouse, and gamepad (virtual Xbox 360 via ViGEmBus/uinput)
- **Opus Audio** — Low-latency audio capture and streaming via WASAPI (Windows) / PipeWire (Linux)
- **End-to-End Encryption** — TLS 1.3 (QUIC), AES-GCM for control/input streams
- **PIN-Based Pairing** — Secure device pairing with certificate pinning

## Architecture

Flux is organized as a Cargo workspace with modular crates:

```
flux/
├── crates/
│   ├── flux-core/        # Shared types, errors, config, platform detection
│   ├── flux-capture/     # Screen capture (DXGI, PipeWire, DRM)
│   ├── flux-encode/      # GPU video encoding (NVENC, AMF, VA-API, Vulkan Video, software)
│   ├── flux-audio/       # Audio capture (WASAPI, PipeWire) + Opus encoding
│   ├── flux-input/       # Input forwarding (keyboard, mouse, gamepad)
│   ├── flux-transport/   # Networking (RTP, FEC, QUIC, packetizer)
│   ├── flux-crypto/      # TLS certificates, AES-GCM, PIN authentication
│   ├── flux-protocol/    # Wire protocol messages, session negotiation
│   ├── flux-server/      # Server binary (host)
│   └── flux-client/      # Client binary (viewer)
└── docs/
    └── architecture.md
```

## Building

```bash
# Build all crates
cargo build --workspace

# Build server only
cargo build -p flux-server

# Build client only
cargo build -p flux-client

# Run tests
cargo test --workspace
```

## Running

### Server (Host)

```bash
# Generate default config and start
flux-server --generate-config
flux-server -c flux.toml
```

### Client (Viewer)

```bash
# Connect to a server
flux-client --server 192.168.1.100:8443 --codec h265 --resolution 1920x1080 --fps 60
```

## Configuration

The server reads a TOML configuration file (`flux.toml`). Run `flux-server --generate-config` to create a default config.

Key settings:

| Section | Key | Default | Description |
|---------|-----|---------|-------------|
| `video` | `codec` | `H265` | Preferred codec (H264, H265, Av1) |
| `video` | `max_fps` | `60` | Maximum framerate |
| `video` | `bitrate_kbps` | `20000` | Target video bitrate |
| `video` | `fec_percentage` | `20` | FEC overhead (0-100%) |
| `audio` | `codec` | `Opus` | Audio codec |
| `audio` | `sample_rate` | `48000` | Audio sample rate |
| `security` | `pin_pairing` | `true` | Require PIN for new clients |

## Platform Support

| Feature | Windows | Linux |
|---------|---------|-------|
| Screen Capture | DXGI Desktop Duplication | PipeWire, DRM/KMS |
| NVIDIA Encoding | NVENC | NVENC |
| AMD Encoding | AMF | VA-API |
| Intel Encoding | — | VA-API |
| Vulkan Video | ✓ | ✓ |
| Audio Capture | WASAPI | PipeWire / PulseAudio |
| Input Injection | SendInput + ViGEmBus | uinput / libei |

## License

MIT OR Apache-2.0
