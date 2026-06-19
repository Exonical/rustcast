use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
#[cfg(feature = "tray")]
use parking_lot::RwLock;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

mod http;
mod pipeline;
mod session;
#[cfg(feature = "tray")]
mod tray;

use flux_core::config::FluxConfig;
use flux_core::platform::PlatformInfo;
use flux_crypto::CertificateManager;
#[cfg(feature = "tray")]
use tray::{FluxTray, TrayAction, TrayState};

#[derive(Parser, Debug)]
#[clap(name = "flux-server", version, about = "Flux Remote Streaming Server")]
struct Args {
    /// Path to configuration file.
    #[clap(short, long, default_value = "flux.toml")]
    config: PathBuf,

    /// Generate a default configuration file and exit.
    #[clap(long)]
    generate_config: bool,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(EnvFilter::from_default_env().add_directive("flux=info".parse()?))
        .init();

    let args = Args::parse();

    // Handle --generate-config.
    if args.generate_config {
        let config = FluxConfig::default();
        config.save(&args.config)?;
        tracing::info!("Default configuration written to {}", args.config.display());
        return Ok(());
    }

    // Load or create configuration.
    let config = if args.config.exists() {
        FluxConfig::from_file(&args.config)?
    } else {
        tracing::info!(
            "Config file {} not found, using defaults",
            args.config.display()
        );
        let config = FluxConfig::default();
        config.save(&args.config)?;
        config
    };

    tracing::info!("Starting Flux Server: {}", config.name);

    // Detect platform capabilities.
    let platform = PlatformInfo::detect();
    tracing::info!(
        "Platform: {:?} | GPU: {:?} | Capture: {:?} | Encoders: {:?}",
        platform.os,
        platform.gpu_vendor,
        platform.available_capture_backends,
        platform.available_encoder_backends,
    );

    // Load or generate TLS certificates.
    let cert_manager = CertificateManager::load_or_create(
        &config.security.cert_path,
        &config.security.key_path,
    )?;
    tracing::info!("TLS certificates loaded");

    // Initialize authentication.
    let mut authenticator = flux_crypto::PinAuthenticator::new();
    let paired_clients_path = config
        .security
        .cert_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("paired_clients.json");
    let _ = authenticator.load_paired_clients(&paired_clients_path);

    // Spawn the system tray on a dedicated OS thread (requires a main-thread
    // message pump on Windows). Gated behind the `tray` feature so a headless /
    // Wayland-pure build needs no GTK/X11 (`libxdo`) libraries.
    #[cfg(feature = "tray")]
    let mut tray_quit_rx = {
        let tray_state = Arc::new(RwLock::new(TrayState {
            active_sessions: 0,
            server_name: config.name.clone(),
            bind_address: format!("{}:{}", config.bind_address, config.server.signaling_port),
        }));
        let (tray_quit_tx, tray_quit_rx) = tokio::sync::oneshot::channel::<()>();
        std::thread::Builder::new()
            .name("flux-tray".into())
            .spawn(move || {
                match FluxTray::new(tray_state) {
                    Ok(tray) => {
                        tracing::info!("System tray initialized");
                        // Simple event loop — poll tray events
                        loop {
                            if let Some(action) = tray.poll_event() {
                                match action {
                                    TrayAction::ShowPin => {
                                        tracing::info!("Tray: Show PIN requested");
                                        // TODO: Generate and display PIN
                                    }
                                    TrayAction::OpenConfig => {
                                        tracing::info!("Tray: Open config requested");
                                        // TODO: Open config file in default editor
                                    }
                                    TrayAction::Quit => {
                                        tracing::info!("Tray: Quit requested");
                                        let _ = tray_quit_tx.send(());
                                        return;
                                    }
                                    TrayAction::ShowStatus => {
                                        tray.update_state();
                                    }
                                }
                            }
                            tray.update_state();
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create system tray: {}. Server will run without tray.", e);
                        // Block until quit signal
                        std::thread::park();
                    }
                }
            })?;
        tray_quit_rx
    };

    // ── Start capture → hardware H.264 encode ──────────────────────
    // Broadcast channel: capture thread sends, TCP frame server(s) receive.
    // Each message carries the frame's capture timestamp (microseconds since
    // capture start) so downstream playout can pace by true capture spacing
    // rather than bursty network arrival time.
    let (h264_tx, _) = tokio::sync::broadcast::channel::<Arc<(u64, Vec<u8>)>>(8);
    let h264_tx2 = h264_tx.clone();

    // IDR request channel: Frame server (TCP) -> Capture thread
    let (idr_tx, idr_rx) = std::sync::mpsc::channel::<()>();

    // Input event channel: Frame server (TCP) -> Capture thread (Input Sink)
    let (input_tx, input_rx) = std::sync::mpsc::channel::<flux_input::InputEvent>();

    let capture_fps = config.video.max_fps.min(60);
    std::thread::Builder::new()
        .name("flux-capture".into())
        .spawn(move || {
            capture_loop(h264_tx2, idr_rx, input_rx, capture_fps);
        })?;

    // ── Start TCP frame server (for Go WebRTC relay) ─────────────
    let frame_port = config.server.signaling_port + 2; // e.g. 8555
    let frame_addr = format!("{}:{}", config.bind_address, frame_port);
    let frame_listener = tokio::net::TcpListener::bind(&frame_addr).await?;
    tracing::info!("H.264 frame server listening on tcp://{}", frame_addr);

    let frame_handle = tokio::spawn(async move {
        frame_server(frame_listener, h264_tx, idr_tx, input_tx).await;
    });

    // Build the server.
    let server = FluxServer::new(config, platform, cert_manager, authenticator).await?;

    tracing::info!("Flux Server is ready and waiting for connections.");

    // Wait for shutdown signal (Ctrl+C or, with the tray, its Quit item).
    #[cfg(feature = "tray")]
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received Ctrl+C, shutting down...");
        }
        _ = &mut tray_quit_rx => {
            tracing::info!("Quit requested from system tray, shutting down...");
        }
    }
    #[cfg(not(feature = "tray"))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("Received Ctrl+C, shutting down...");
    }

    frame_handle.abort();
    server.shutdown().await;
    tracing::info!("Flux Server stopped.");
    Ok(())
}

/// TCP frame server: accepts connections and streams timestamped H.264 NALUs.
/// Protocol: [8-byte BE capture-timestamp µs][4-byte BE length][H.264 data] per packet.
async fn frame_server(
    listener: tokio::net::TcpListener,
    h264_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    idr_tx: std::sync::mpsc::Sender<()>,
    input_tx: std::sync::mpsc::Sender<flux_input::InputEvent>,
) {
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Frame server accept error: {}", e);
                continue;
            }
        };
        tracing::info!("Frame client connected: {}", addr);
        let mut rx = h264_tx.subscribe();
        let idr_tx = idr_tx.clone();
        let input_tx = input_tx.clone();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut reader, mut writer) = stream.into_split();
            let mut frames_sent: u64 = 0;

            // Spawn reader task to handle upstream commands
            let reader_handle = tokio::spawn(async move {
                loop {
                    // Read command byte
                    let mut cmd = [0u8; 1];
                    if reader.read_exact(&mut cmd).await.is_err() {
                        break;
                    }

                    match cmd[0] {
                        0x01 => {
                            // IDR Request
                            tracing::info!("Client {} requested IDR frame", addr);
                            let _ = idr_tx.send(());
                        }
                        0x02 => {
                            // Input Event
                            // Read 4-byte length (Big Endian)
                            let mut len_buf = [0u8; 4];
                            if reader.read_exact(&mut len_buf).await.is_err() {
                                break;
                            }
                            let len = u32::from_be_bytes(len_buf) as usize;

                            // Limit max input packet size (e.g. 1MB) to prevent OOM
                            if len > 1024 * 1024 {
                                tracing::warn!("Input event too large: {} bytes", len);
                                break;
                            }

                            let mut payload = vec![0u8; len];
                            if reader.read_exact(&mut payload).await.is_err() {
                                break;
                            }

                            // Deserialize and dispatch
                            match serde_json::from_slice::<flux_input::InputEvent>(&payload) {
                                Ok(event) => {
                                    // Log non-movement events at INFO, movements at TRACE
                                    match &event {
                                        flux_input::InputEvent::Mouse(flux_input::mouse::MouseEvent::Move { .. }) |
                                        flux_input::InputEvent::Mouse(flux_input::mouse::MouseEvent::MoveAbsolute { .. }) => {
                                            tracing::trace!("Input: Move");
                                        }
                                        _ => {
                                            tracing::info!("Input: {:?}", event);
                                        }
                                    }
                                    let _ = input_tx.send(event);
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to deserialize input event: {}", e);
                                }
                            }
                        }
                        _ => {
                            tracing::warn!("Unknown command byte: 0x{:02x}", cmd[0]);
                            // We could break or ignore. If we ignore, we might lose sync if protocol expects strict format.
                            // For now, let's assume valid stream or disconnect.
                        }
                    }
                }
            });

            loop {
                let msg = match rx.recv().await {
                    Ok(d) => d,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Frame client {} lagged by {} frames", addr, n);
                        continue;
                    }
                    Err(_) => break,
                };
                let (ts_micros, data) = (msg.0, &msg.1);

                if data.is_empty() {
                    continue;
                }

                // Write timestamped frame: [8-byte BE ts µs][4-byte BE len][data]
                let ts_bytes = ts_micros.to_be_bytes();
                let len_bytes = (data.len() as u32).to_be_bytes();
                if writer.write_all(&ts_bytes).await.is_err() {
                    break;
                }
                if writer.write_all(&len_bytes).await.is_err() {
                    break;
                }
                if writer.write_all(data).await.is_err() {
                    break;
                }
                frames_sent += 1;
            }

            reader_handle.abort();
            tracing::info!("Frame client disconnected: {} ({} frames sent)", addr, frames_sent);
        });
    }
}

/// Ordered list of encoder backends to try for this build/platform, from most
/// to least preferred, always ending in the software fallback.
///
/// On Linux with FFmpeg compiled in we prefer Vulkan video (`h264_vulkan`) —
/// real GPU offload on the system Vulkan driver, which works on hosts with no
/// VA-API driver (e.g. RHEL/Rocky) — then FFmpeg VA-API, then software.
fn encoder_fallback_chain() -> Vec<flux_core::types::EncoderBackend> {
    use flux_core::types::EncoderBackend;
    #[cfg(target_os = "windows")]
    {
        vec![EncoderBackend::Amf, EncoderBackend::Software]
    }
    #[cfg(all(target_os = "linux", feature = "encoder-ffmpeg"))]
    {
        vec![
            EncoderBackend::VulkanVideo,
            EncoderBackend::FfmpegVaapi,
            EncoderBackend::Software,
        ]
    }
    #[cfg(all(target_os = "linux", feature = "encoder-vaapi", not(feature = "encoder-ffmpeg")))]
    {
        vec![EncoderBackend::Vaapi, EncoderBackend::Software]
    }
    #[cfg(not(any(
        target_os = "windows",
        all(target_os = "linux", feature = "encoder-ffmpeg"),
        all(target_os = "linux", feature = "encoder-vaapi"),
    )))]
    {
        vec![EncoderBackend::Software]
    }
}

/// Create an encoder of the given backend and open a session, returning `None`
/// (with a warning logged) if either the encoder or the session can't be built.
fn create_encode_session(
    backend: flux_core::types::EncoderBackend,
    config: flux_encode::traits::EncodeConfig,
) -> Option<Box<dyn flux_encode::traits::EncodeSession>> {
    let encoder = match flux_encode::create_encoder(Some(backend)) {
        Ok(enc) => {
            tracing::info!("Encoder created: {} ({:?})", enc.name(), backend);
            enc
        }
        Err(e) => {
            tracing::warn!("{:?} encoder not available: {}", backend, e);
            return None;
        }
    };
    match encoder.create_session(config) {
        Ok(s) => {
            tracing::info!("{:?} H.264 encode session started", backend);
            Some(s)
        }
        Err(e) => {
            tracing::warn!("Failed to create {:?} encode session: {}", backend, e);
            None
        }
    }
}

/// Build an H.264 encode session for a concrete capture resolution, preferring
/// the platform hardware encoder and falling back to software when it can't be
/// opened (e.g. no VA-API driver). Returns the session and the backend used.
fn build_encode_session_for(
    target_fps: u32,
    resolution: flux_core::types::Resolution,
) -> (
    Option<Box<dyn flux_encode::traits::EncodeSession>>,
    flux_core::types::EncoderBackend,
) {
    let encoder_config = flux_encode::traits::EncodeConfig {
        codec: flux_core::types::VideoCodec::H264,
        resolution,
        framerate: target_fps,
        bitrate_kbps: 10_000,
        rate_control: flux_core::types::RateControlMode::Cbr,
        dynamic_range: flux_core::types::DynamicRange::Sdr,
        chroma_sampling: flux_core::types::ChromaSampling::Yuv420,
        // Emit a keyframe every ~2s so a late joiner or any decode desync
        // recovers on its own without depending solely on a PLI/IDR request.
        gop_size: target_fps.saturating_mul(2).max(1),
        b_frames: 0,
        max_ref_frames: 1,
    };

    // Try each backend in preference order (GPU offload first), falling back
    // to the next when one can't be opened (e.g. no Vulkan encode support, no
    // VA-API driver), ending at the software encoder.
    let chain = encoder_fallback_chain();
    let mut backend = flux_core::types::EncoderBackend::Software;
    let mut session = None;
    for (idx, candidate) in chain.iter().copied().enumerate() {
        backend = candidate;
        session = create_encode_session(candidate, encoder_config.clone());
        if session.is_some() {
            break;
        }
        if let Some(next) = chain.get(idx + 1) {
            tracing::warn!("{:?} encoder unavailable; falling back to {:?}", candidate, next);
        }
    }
    (session, backend)
}

/// Background thread: capture → hardware H.264 encode → broadcast channel.
/// Writes first ~5s of H.264 NALUs to a verification file.
fn capture_loop(
    h264_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    idr_rx: std::sync::mpsc::Receiver<()>,
    input_rx: std::sync::mpsc::Receiver<flux_input::InputEvent>,
    target_fps: u32,
) {
    // ── Initialize capture ──────────────────────────────────────────
    let capture = match flux_capture::create_capture(None) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("Failed to create capture backend: {}", e);
            return;
        }
    };

    let displays = match capture.enumerate_displays() {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Failed to enumerate displays: {}", e);
            return;
        }
    };
    tracing::info!("Capture: found {} display(s)", displays.len());

    let primary = displays.iter().find(|d| d.primary).unwrap_or(&displays[0]);
    
    // Initialize Input Sink (needs primary display resolution for absolute mouse positioning)
    let input_sink = match flux_input::InputSink::new(
        primary.native_resolution.width,
        primary.native_resolution.height,
    ) {
        Ok(sink) => sink,
        Err(e) => {
            tracing::error!("Failed to initialize input sink: {}", e);
            return;
        }
    };

    // Spawn a dedicated thread for input handling to ensure low latency
    // and avoid blocking the capture loop.
    std::thread::spawn(move || {
        tracing::info!("Input dispatch thread started");
        while let Ok(event) = input_rx.recv() {
            if let Err(e) = input_sink.handle_event(&event) {
                tracing::warn!("Input injection error: {}", e);
            }
        }
        tracing::info!("Input dispatch thread stopped");
    });

    let mut session = match capture.start_capture(
        Some(primary.id),
        primary.native_resolution,
        target_fps,
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to start capture session: {}", e);
            return;
        }
    };

    // ── Encoder is initialized lazily from the first captured frame ──
    // The capture server fixates the real resolution at negotiation time,
    // which can differ from the display's reported native size (e.g. rotated
    // monitors). Build the encoder from the frame the capture path actually
    // delivers, and rebuild it if the resolution changes mid-stream.
    let mut encode_session: Option<Box<dyn flux_encode::traits::EncodeSession>> = None;
    let mut encode_resolution = flux_core::types::Resolution::new(0, 0);

    // Verification file (first ~5 seconds)
    let h264_path = std::path::PathBuf::from("flux_capture_test.h264");
    let mut h264_file = std::fs::File::create(&h264_path).ok();
    let max_verify_frames = target_fps as u64 * 5;
    let mut total_encoded_bytes: u64 = 0;

    tracing::info!(
        "Capture+encode loop starting @{}fps → H.264 (encoder built from first frame; verify: {})",
        target_fps,
        h264_path.display()
    );

    let mut frame_count: u64 = 0;
    let loop_start = std::time::Instant::now();

    loop {
        // Check for IDR requests
        if let Ok(_) = idr_rx.try_recv() {
            tracing::info!("Handling IDR request from client");
            if let Some(ref mut enc) = encode_session {
                enc.request_idr();
            }
        }

        let t0 = std::time::Instant::now();

        let frame = match session.next_frame() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("Capture error: {}", e);
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };

        let t_capture = t0.elapsed();
        frame_count += 1;

        // (Re)build the encoder once the negotiated capture resolution is
        // known or whenever it changes (e.g. display rotation / mode switch),
        // so the encoder dimensions always match the frames it receives.
        if encode_resolution != frame.resolution {
            let (sess, backend) = build_encode_session_for(target_fps, frame.resolution);
            encode_session = sess;
            encode_resolution = frame.resolution;
            tracing::info!(
                "Capture+encode loop: {}x{}@{}fps → {:?} H.264",
                encode_resolution.width, encode_resolution.height, target_fps, backend
            );
            if let Some(ref mut enc) = encode_session {
                enc.request_idr();
            }
        }

        // ── Hardware H.264 encode ───────────────────────────────────
        let t1 = std::time::Instant::now();
        let ts_micros = frame
            .timestamp
            .saturating_duration_since(loop_start)
            .as_micros() as u64;
        if let Some(ref mut enc) = encode_session {
            match enc.encode(&frame) {
                Ok(packets) => {
                    for pkt in &packets {
                        total_encoded_bytes += pkt.data.len() as u64;

                        if frame_count <= max_verify_frames {
                            if let Some(ref mut f) = h264_file {
                                use std::io::Write;
                                let _ = f.write_all(&pkt.data);
                            }
                        } else if h264_file.is_some() {
                            h264_file.take();
                            tracing::info!(
                                "H.264 verification file closed ({} frames, {} bytes)",
                                max_verify_frames,
                                total_encoded_bytes
                            );
                        }

                        let _ = h264_tx.send(Arc::new((ts_micros, pkt.data.clone())));
                    }
                }
                Err(e) => {
                    tracing::warn!("Encode error on frame {}: {}", frame_count, e);
                }
            }
        }
        let t_encode = t1.elapsed();

        // Periodic performance stats (every 5 seconds)
        if frame_count % (target_fps as u64 * 5) == 0 {
            let wall = loop_start.elapsed().as_secs_f64();
            let actual_fps = frame_count as f64 / wall;
            let avg_kbps = if frame_count > 0 {
                total_encoded_bytes * 8 / frame_count * target_fps as u64 / 1000
            } else { 0 };
            tracing::info!(
                "Perf: {:.1} fps | capture={:.1}ms encode={:.1}ms | {} frames, ~{} kbps",
                actual_fps,
                t_capture.as_secs_f64() * 1000.0,
                t_encode.as_secs_f64() * 1000.0,
                frame_count,
                avg_kbps,
            );
        }
    }
}

/// The top-level Flux server orchestrating all subsystems.
struct FluxServer {
    _config: FluxConfig,
    _platform: PlatformInfo,
    _cert_manager: CertificateManager,
    session_manager: session::SessionManager,
}

impl FluxServer {
    async fn new(
        config: FluxConfig,
        platform: PlatformInfo,
        cert_manager: CertificateManager,
        _authenticator: flux_crypto::PinAuthenticator,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let session_manager = session::SessionManager::new(
            config.clone(),
            platform.clone(),
        );

        // TODO: Start the QUIC server for signaling.
        // TODO: Start mDNS/zeroconf advertisement.
        // TODO: Start the HTTP(S) control API.

        Ok(Self {
            _config: config,
            _platform: platform,
            _cert_manager: cert_manager,
            session_manager,
        })
    }

    async fn shutdown(self) {
        tracing::info!("Shutting down all sessions...");
        self.session_manager.shutdown_all().await;
    }
}
