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
mod quic_frames;
mod registration;
mod session;
#[cfg(target_os = "windows")]
mod ccd_display;
#[cfg(target_os = "windows")]
mod virtual_display;
#[cfg(feature = "tray")]
mod tray;

use flux_core::config::FluxConfig;
use flux_core::error::FluxError;
use flux_core::platform::PlatformInfo;
use flux_crypto::CertificateManager;
#[cfg(target_os = "windows")]
type PrivacyController = ccd_display::PrivacyController;
#[cfg(not(target_os = "windows"))]
type PrivacyController = ();
#[cfg(target_os = "windows")]
type VirtualDisplayHandle = virtual_display::VirtualDisplay;
#[cfg(not(target_os = "windows"))]
type VirtualDisplayHandle = ();
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct ModeRequest {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct ResolutionStatus {
    pub state: &'static str,
    pub width: u32,
    pub height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

type ResolutionStatusMessage = Arc<(u64, Vec<u8>)>;

fn publish_resolution_status(
    tx: &tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
    status: ResolutionStatus,
) {
    if let Ok(payload) = serde_json::to_vec(&status) {
        let _ = tx.send(Arc::new((0, payload)));
    }
}

pub(crate) fn validate_mode_request(request: ModeRequest) -> Result<ModeRequest, &'static str> {
    if request.width < 640 || request.height < 480 {
        return Err("resolution is implausibly small");
    }
    if request.width > 2560 || request.height > 1440 {
        return Err("resolution exceeds 2560x1440");
    }
    if request.width % 2 != 0 || request.height % 2 != 0 {
        return Err("resolution dimensions must be even");
    }
    Ok(request)
}

pub(crate) fn mode_change_needed(current: ModeRequest, requested: ModeRequest) -> bool {
    current.width != requested.width || current.height != requested.height
}

fn effective_default_fps_cap(configured: u32, requested_default: u32) -> u32 {
    requested_default.min(configured)
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

    #[cfg(target_os = "windows")]
    {
        let privacy_snapshot_path = config
            .security
            .cert_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("privacy_topology.bin");
        ccd_display::recover_snapshot(&privacy_snapshot_path);
    }
    if config.video.privacy.enabled && config.video.virtual_display.is_none() {
        return Err(
            "privacy mode requires [video.virtual_display]; refusing to disable the only display"
                .into(),
        );
    }
    #[cfg(not(target_os = "windows"))]
    if config.video.privacy.enabled {
        return Err("privacy mode is only supported on Windows".into());
    }

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

    // Plug in a virtual monitor for headless hosts before capture starts.
    // Kept alive for the whole server lifetime; dropped (plugged out) on exit.
    #[cfg(target_os = "windows")]
    let (virtual_display, virtual_display_target) = if let Some(vd) = config.video.virtual_display {
        let capture_probe = flux_capture::create_capture(None)
            .map_err(|e| format!("initialize capture while probing virtual display: {e}"))?;
        let before = match capture_probe.enumerate_displays() {
            Ok(displays) => displays,
            Err(e) => {
                tracing::info!("No displays before virtual plug-in: {e}");
                Vec::new()
            }
        };
        let mut before_names: std::collections::HashSet<String> =
            before.into_iter().map(|display| display.name).collect();
        let mut display =
            virtual_display::VirtualDisplay::plug_in(vd.width, vd.height, vd.refresh_hz)
            .map_err(|e| format!("plug in virtual display: {e}"))?;
        if display.was_adopted() {
            // The 60-byte status IOCTL reports whether a monitor is present
            // but not its mode. Recreate an adopted monitor at the requested
            // mode so the existing new-output identity check remains strict.
            display
                .unplug()
                .map_err(|e| format!("unplug existing virtual display: {e}"))?;
            let display_count_before_unplug = before_names.len();
            let unplug_deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(2);
            let after_unplug = loop {
                match capture_probe.enumerate_displays() {
                    Ok(displays) if displays.len() < display_count_before_unplug => {
                        break displays;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        tracing::debug!("waiting for existing virtual DXGI output removal: {e}")
                    }
                }
                if std::time::Instant::now() >= unplug_deadline {
                    drop(display);
                    return Err(
                        "existing virtual display could not be removed within 2 seconds"
                            .into(),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            };
            before_names = after_unplug
                .into_iter()
                .map(|display| display.name)
                .collect();
            display
                .plug_in_mode(vd.width, vd.height, vd.refresh_hz)
                .map_err(|e| format!("re-plug virtual display: {e}"))?;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let target = loop {
            if std::time::Instant::now() >= deadline {
                drop(display);
                return Err(
                    "virtual display plugged in but no new DXGI output appeared within 10 seconds"
                        .into(),
                );
            }
            match capture_probe.enumerate_displays() {
                Ok(displays) => {
                    if let Some(display) = displays
                        .into_iter()
                        .find(|display| !before_names.contains(&display.name))
                    {
                        break display;
                    }
                }
                Err(e) => tracing::debug!("waiting for virtual DXGI output: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        tracing::info!(
            "Virtual display is available as DXGI output {} on adapter LUID {:?}",
            target.name,
            target.adapter_luid
        );
        ccd_display::configure_virtual_display(
            &target,
            vd.width,
            vd.height,
            vd.refresh_hz,
        )
        .map_err(|e| format!("configure virtual display topology: {e}"))?;

        let configured_deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(5);
        let target = loop {
            if std::time::Instant::now() >= configured_deadline {
                drop(display);
                return Err(format!(
                    "virtual display {} did not appear at configured mode {}x{}@{}Hz after CCD apply",
                    target.name, vd.width, vd.height, vd.refresh_hz
                )
                .into());
            }
            match capture_probe.enumerate_displays() {
                Ok(displays) => {
                    if let Some(display) = displays.into_iter().find(|display| {
                        display.name == target.name
                            && display.adapter_luid == target.adapter_luid
                            && display.native_resolution.width == vd.width
                            && display.native_resolution.height == vd.height
                    }) {
                        break display;
                    }
                }
                Err(e) => tracing::debug!("waiting for configured virtual DXGI output: {e}"),
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        };
        tracing::info!(
            "Virtual display configured as DXGI output {} at {}x{} on adapter LUID {:?}",
            target.name,
            target.native_resolution.width,
            target.native_resolution.height,
            target.adapter_luid
        );
        (Some(display), Some(target))
    } else {
        (None, None)
    };
    #[cfg(not(target_os = "windows"))]
    let virtual_display: Option<VirtualDisplayHandle> = None;
    #[cfg(not(target_os = "windows"))]
    let virtual_display_target = None;

    #[cfg(target_os = "windows")]
    let privacy_controller = if config.video.privacy.enabled {
        let privacy_snapshot_path = config
            .security
            .cert_path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("privacy_topology.bin");
        Some(
            ccd_display::PrivacyController::new(
                virtual_display_target
                    .clone()
                    .ok_or("privacy mode requires a confirmed virtual display output")?,
                privacy_snapshot_path,
                config.video.privacy.lock_on_disconnect,
            )
            .map_err(|error| format!("initialize privacy mode: {error}"))?,
        )
    } else {
        None
    };
    #[cfg(not(target_os = "windows"))]
    let privacy_controller: Option<PrivacyController> = None;

    // ── Start capture → hardware H.264 encode ──────────────────────
    // Broadcast channel: capture thread sends, TCP frame server(s) receive.
    // Each message carries the frame's capture timestamp (microseconds since
    // capture start) so downstream playout can pace by true capture spacing
    // rather than bursty network arrival time.
    let (h264_tx, _) = tokio::sync::broadcast::channel::<Arc<(u64, Vec<u8>)>>(8);
    let (cursor_tx, _) = tokio::sync::watch::channel(Arc::new((
        0u64,
        flux_core::cursor::CursorMetadata::hidden(),
    )));
    let h264_tx2 = h264_tx.clone();
    let cursor_tx2 = cursor_tx.clone();

    // IDR request channel: Frame server (TCP) -> Capture thread
    let (idr_tx, idr_rx) = std::sync::mpsc::channel::<()>();

    // Bitrate update channel: relay congestion feedback -> capture thread
    let (bitrate_tx, bitrate_rx) = std::sync::mpsc::channel::<u32>();
    let (quality_tx, quality_rx) = std::sync::mpsc::channel::<(u8, u8)>();
    let (resolution_tx, resolution_rx) = std::sync::mpsc::channel::<ModeRequest>();
    let (resolution_status_tx, _) =
        tokio::sync::broadcast::channel::<ResolutionStatusMessage>(8);

    // Input event channel: Frame server (TCP) -> Capture thread (Input Sink)
    let (input_tx, input_rx) =
        std::sync::mpsc::channel::<(std::time::Instant, flux_input::InputEvent)>();

    let capture_fps = config.video.max_fps.min(144);
    let forced_encoder = config.video.encoder;
    let default_quality_level = config.video.quality_level;
    let runtime_status = Arc::new(std::sync::RwLock::new(registration::RuntimeStatus::default()));
    let runtime_status_capture = runtime_status.clone();
    let resolution_status_tx_capture = resolution_status_tx.clone();
    std::thread::Builder::new()
        .name("flux-capture".into())
        .spawn(move || {
            capture_loop(
                h264_tx2,
                cursor_tx2,
                idr_rx,
                bitrate_rx,
                quality_rx,
                resolution_rx,
                input_rx,
                capture_fps,
                config.video.default_fps_cap,
                forced_encoder,
                default_quality_level,
                virtual_display_target,
                virtual_display,
                resolution_status_tx_capture,
                runtime_status_capture,
            );
        })?;

    // ── Start TCP frame server (for Go WebRTC relay) ─────────────
    let frame_port = config.server.signaling_port + 2; // e.g. 8555
    let frame_addr = format!("{}:{}", config.bind_address, frame_port);
    let frame_listener = tokio::net::TcpListener::bind(&frame_addr).await?;
    let frame_host = config
        .relay
        .advertise_host
        .as_deref()
        .unwrap_or_else(|| {
            if config.bind_address == "0.0.0.0" {
                "127.0.0.1"
            } else {
                config.bind_address.as_str()
            }
        });
    let advertised_frame_addr = match frame_host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V6(_)) if !frame_host.starts_with('[') => {
            format!("[{frame_host}]:{frame_port}")
        }
        _ => format!("{frame_host}:{frame_port}"),
    };
    let registration_stop = registration::start(
        &config,
        &platform,
        advertised_frame_addr,
        runtime_status,
    );
    tracing::info!("H.264 frame server listening on tcp://{}", frame_addr);

    // QUIC frame server on the same port number over UDP. Preferred by the
    // relay on lossy links (no head-of-line blocking); TCP remains as fallback.
    let quic_handle = match frame_addr.parse::<std::net::SocketAddr>() {
        Ok(bind_addr) => match quic_frames::make_endpoint(bind_addr, &cert_manager) {
            Ok(endpoint) => {
                tracing::info!("H.264 frame server listening on quic://{}", frame_addr);
                let h264_tx = h264_tx.clone();
                let cursor_rx = cursor_tx.subscribe();
                let idr_tx = idr_tx.clone();
                let bitrate_tx = bitrate_tx.clone();
                let quality_tx = quality_tx.clone();
                let resolution_tx = resolution_tx.clone();
                let resolution_status_tx = resolution_status_tx.clone();
                let input_tx = input_tx.clone();
                let privacy_controller = privacy_controller.clone();
                Some(tokio::spawn(async move {
                    quic_frames::serve(
                        endpoint,
                        h264_tx,
                        cursor_rx,
                        idr_tx,
                        bitrate_tx,
                        quality_tx,
                        resolution_tx,
                        resolution_status_tx,
                        input_tx,
                        privacy_controller,
                    )
                    .await;
                }))
            }
            Err(e) => {
                tracing::warn!("QUIC frame server unavailable: {} (TCP only)", e);
                None
            }
        },
        Err(e) => {
            tracing::warn!("QUIC frame server bind address invalid: {} (TCP only)", e);
            None
        }
    };

    let frame_privacy_controller = privacy_controller.clone();
    let cursor_rx = cursor_tx.subscribe();
    let frame_handle = tokio::spawn(async move {
        frame_server(
            frame_listener,
            h264_tx,
            cursor_rx,
            idr_tx,
            bitrate_tx,
            quality_tx,
            resolution_tx,
            resolution_status_tx,
            input_tx,
            frame_privacy_controller,
        )
        .await;
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
    if let Some(h) = quic_handle {
        h.abort();
    }
    #[cfg(target_os = "windows")]
    if let Some(privacy) = privacy_controller {
        if let Err(error) = privacy.restore() {
            tracing::error!("Failed to restore privacy display topology: {error}");
        }
    }
    server.shutdown().await;
    if let Some(handle) = registration_stop {
        let _ = handle.stop.send(());
    }
    tracing::info!("Flux Server stopped.");
    Ok(())
}

/// TCP frame server: accepts connections and streams timestamped H.264 NALUs.
/// Protocol: [1-byte type][8-byte BE capture-timestamp µs][4-byte BE length][payload].
/// Type 0x01 is an H.264 access unit; type 0x02 is cursor JSON metadata.
/// Control commands: 0x01 IDR, 0x02 input, 0x03 bitrate, 0x04 viewer count,
/// and 0x05 quality/FPS ([quality level, FPS cap], each 0 means automatic),
/// and 0x06 resolution ([2-byte BE width, 2-byte BE height], refresh fixed at 60).
async fn frame_server(
    listener: tokio::net::TcpListener,
    h264_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    cursor_rx: tokio::sync::watch::Receiver<Arc<(u64, flux_core::cursor::CursorMetadata)>>,
    idr_tx: std::sync::mpsc::Sender<()>,
    bitrate_tx: std::sync::mpsc::Sender<u32>,
    quality_tx: std::sync::mpsc::Sender<(u8, u8)>,
    resolution_tx: std::sync::mpsc::Sender<ModeRequest>,
    resolution_status_tx: tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
    input_tx: std::sync::mpsc::Sender<(std::time::Instant, flux_input::InputEvent)>,
    privacy_controller: Option<PrivacyController>,
) {
    #[cfg(not(target_os = "windows"))]
    let _ = &privacy_controller;
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Frame server accept error: {}", e);
                continue;
            }
        };
        tracing::info!("Frame client connected: {}", addr);

        // Low-latency socket setup: disable Nagle so each frame is sent
        // immediately, and mark packets DSCP AF41 (interactive video) so
        // QoS-aware networks prioritize them when the relay runs on
        // another machine. Both are best-effort.
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!("set_nodelay failed: {}", e);
        }
        let sock = socket2::SockRef::from(&stream);
        if let Err(e) = sock.set_tos_v4(34 << 2) {
            tracing::debug!("DSCP not set (normal on Windows): {}", e);
        }
        let mut rx = h264_tx.subscribe();
        let mut cursor_rx = cursor_rx.clone();
        let idr_tx = idr_tx.clone();
        let bitrate_tx = bitrate_tx.clone();
        let quality_tx = quality_tx.clone();
        let resolution_tx = resolution_tx.clone();
        let mut resolution_status_rx = resolution_status_tx.subscribe();
        let input_tx = input_tx.clone();
        #[cfg(target_os = "windows")]
        let privacy_connection = privacy_controller
            .as_ref()
            .map(|privacy| privacy.connection());
        #[cfg(not(target_os = "windows"))]
        let privacy_connection: Option<()> = None;
        #[cfg(not(target_os = "windows"))]
        let _ = &privacy_connection;

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut reader, mut writer) = stream.into_split();
            let mut frames_sent: u64 = 0;

            // Spawn reader task to handle upstream commands
            let mut reader_handle = tokio::spawn(async move {
                #[cfg(target_os = "windows")]
                let privacy_connection = privacy_connection;
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
                                    let _ = input_tx.send((std::time::Instant::now(), event));
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to deserialize input event: {}", e);
                                }
                            }
                        }
                        0x03 => {
                            // Bitrate update (congestion feedback from relay)
                            let mut kbps_buf = [0u8; 4];
                            if reader.read_exact(&mut kbps_buf).await.is_err() {
                                break;
                            }
                            let kbps = u32::from_be_bytes(kbps_buf);
                            tracing::info!("Client {} requested bitrate {} kbps", addr, kbps);
                            let _ = bitrate_tx.send(kbps);
                        }
                        0x04 => {
                            let mut count_buf = [0u8; 4];
                            if reader.read_exact(&mut count_buf).await.is_err() {
                                break;
                            }
                            let count = u32::from_be_bytes(count_buf);
                            tracing::info!("Client {} reported {} viewer(s)", addr, count);
                            #[cfg(target_os = "windows")]
                            if let Some(privacy) = privacy_connection.as_ref() {
                                if let Err(error) = privacy.update(count) {
                                    tracing::error!(
                                        "Privacy mode viewer-count update failed: {error}"
                                    );
                                }
                            }
                        }
                        0x05 => {
                            let mut level = [0u8; 2];
                            if reader.read_exact(&mut level).await.is_err() {
                                break;
                            }
                            let _ = quality_tx.send((level[0], level[1]));
                        }
                        0x06 => {
                            let mut dimensions = [0u8; 4];
                            if reader.read_exact(&mut dimensions).await.is_err() {
                                break;
                            }
                            let request = ModeRequest {
                                width: u16::from_be_bytes([dimensions[0], dimensions[1]]) as u32,
                                height: u16::from_be_bytes([dimensions[2], dimensions[3]]) as u32,
                            };
                            if validate_mode_request(request).is_err() {
                                tracing::warn!(
                                    "Ignoring invalid resolution request: {}x{}",
                                    request.width,
                                    request.height
                                );
                            } else {
                                let _ = resolution_tx.send(request);
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

            let initial_cursor = cursor_rx.borrow_and_update().clone();
            let initial_payload = match serde_json::to_vec(&initial_cursor.1) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!("Failed to serialize initial cursor: {}", error);
                    Vec::new()
                }
            };
            if !initial_payload.is_empty() {
                let mut header = [0u8; 13];
                header[0] = 0x02;
                header[1..9].copy_from_slice(&initial_cursor.0.to_be_bytes());
                header[9..13].copy_from_slice(&(initial_payload.len() as u32).to_be_bytes());
                if writer.write_all(&header).await.is_err()
                    || writer.write_all(&initial_payload).await.is_err()
                {
                    reader_handle.abort();
                    return;
                }
            }

            loop {
                let (message_type, msg) = tokio::select! {
                    result = &mut reader_handle => {
                        let _ = result;
                        break;
                    }
                    result = cursor_rx.changed() => {
                        if result.is_err() {
                            break;
                        }
                        let cursor = cursor_rx.borrow_and_update().clone();
                        let Ok(payload) = serde_json::to_vec(&cursor.1) else {
                            continue;
                        };
                        let mut header = [0u8; 13];
                        header[0] = 0x02;
                        header[1..9].copy_from_slice(&cursor.0.to_be_bytes());
                        header[9..13].copy_from_slice(&(payload.len() as u32).to_be_bytes());
                        if writer.write_all(&header).await.is_err()
                            || writer.write_all(&payload).await.is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    result = rx.recv() => match result {
                        Ok(d) => (0x01, d),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Frame client {} lagged by {} frames", addr, n);
                            continue;
                        }
                        Err(_) => break,
                    },
                    result = resolution_status_rx.recv() => match result {
                        Ok(status) => (0x03, status),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                };
                let mut header = [0u8; 13];
                header[0] = message_type;
                header[1..9].copy_from_slice(&msg.0.to_be_bytes());
                header[9..13].copy_from_slice(&(msg.1.len() as u32).to_be_bytes());
                if writer.write_all(&header).await.is_err()
                    || writer.write_all(&msg.1).await.is_err()
                {
                    break;
                }
                frames_sent += 1;
            }

            reader_handle.abort();
            tracing::info!("Frame client disconnected: {} ({} frames sent)", addr, frames_sent);
        });
    }
}

/// The ordered H.264 encoder backends to try for this build/platform: the
/// vendor hardware encoder first (AMF on Windows, FFmpeg-VA-API or
/// cros-codecs VA-API on Linux depending on the compiled features), then the
/// vendor-neutral Vulkan Video encoder, then the software fallback.
// Vec::new + cfg-gated pushes: `vec![]` can't hold conditional elements.
#[allow(clippy::vec_init_then_push)]
fn encoder_backend_candidates(
    forced: Option<flux_core::types::EncoderBackend>,
) -> Vec<flux_core::types::EncoderBackend> {
    use flux_core::types::EncoderBackend;
    if let Some(backend) = forced {
        tracing::info!("Encoder backend forced by config: {:?}", backend);
        // Keep the software fallback so a failed forced backend still streams.
        if backend == EncoderBackend::Software {
            return vec![backend];
        }
        return vec![backend, EncoderBackend::Software];
    }
    let mut candidates = Vec::new();
    #[cfg(target_os = "windows")]
    candidates.push(EncoderBackend::Amf);
    #[cfg(all(target_os = "linux", feature = "encoder-ffmpeg"))]
    candidates.push(EncoderBackend::FfmpegVaapi);
    #[cfg(all(target_os = "linux", feature = "encoder-vaapi", not(feature = "encoder-ffmpeg")))]
    candidates.push(EncoderBackend::Vaapi);
    #[cfg(feature = "encoder-vulkan")]
    candidates.push(EncoderBackend::VulkanVideo);
    candidates.push(EncoderBackend::Software);
    candidates
}

/// Bits-per-pixel-per-frame coefficients for quality levels 1 through 10.
/// Level 3 is the former 0.05 setting; the default level 6 is 0.10.
/// Keep the UI estimate in `flux-web/ui/app/page.tsx` synchronized with this table.
fn quality_bpp(level: u8) -> f64 {
    match level.clamp(1, 10) {
        1 => 0.025,
        2 => 0.035,
        3 => 0.050,
        4 => 0.065,
        5 => 0.080,
        6 => 0.100,
        7 => 0.120,
        8 => 0.140,
        9 => 0.160,
        _ => 0.200,
    }
}

fn bitrate_kbps_for(
    resolution: flux_core::types::Resolution,
    fps: u32,
    level: u8,
) -> u32 {
    let bps = resolution.width as f64
        * resolution.height as f64
        * fps as f64
        * quality_bpp(level);
    (bps / 1000.0).round().clamp(3_000.0, 50_000.0) as u32
}

const FRAME_PACING_TOLERANCE: f64 = 0.10;

fn should_encode_frame(
    last_encoded_at: &mut Option<std::time::Instant>,
    now: std::time::Instant,
    fps_cap: u32,
) -> bool {
    let interval = std::time::Duration::from_secs_f64(1.0 / fps_cap.max(1) as f64);
    if let Some(last) = *last_encoded_at {
        if now.duration_since(last) + interval.mul_f64(FRAME_PACING_TOLERANCE) < interval {
            return false;
        }
    }
    *last_encoded_at = Some(now);
    true
}

/// Create an encoder of the given backend and open a session, returning `None`
/// (with a warning logged) if either the encoder or the session can't be built.
/// If the requested resolution exceeds the encoder's maximum, the session is
/// opened at an aspect-preserving downscaled resolution instead; the actually
/// used resolution is returned alongside the session.
fn create_encode_session(
    backend: flux_core::types::EncoderBackend,
    mut config: flux_encode::traits::EncodeConfig,
    quality_level: u8,
) -> Option<(Box<dyn flux_encode::traits::EncodeSession>, flux_core::types::Resolution)> {
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
    if let Ok(caps) = encoder.capabilities() {
        let fitted = flux_encode::scale::fit_within(config.resolution, caps.max_resolution);
        if fitted != config.resolution {
            tracing::info!(
                "{:?}: downscaling {} → {} to fit encoder maximum {}",
                backend,
                config.resolution,
                fitted,
                caps.max_resolution
            );
            config.resolution = fitted;
        }
    }
    let resolution = config.resolution;
    config.bitrate_kbps = bitrate_kbps_for(resolution, config.framerate, quality_level);
    match encoder.create_session(config) {
        Ok(s) => {
            tracing::info!("{:?} H.264 encode session started at {}", backend, resolution);
            Some((s, resolution))
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
#[allow(clippy::type_complexity)]
fn build_encode_session_for(
    target_fps: u32,
    resolution: flux_core::types::Resolution,
    forced_backend: Option<flux_core::types::EncoderBackend>,
    quality_level: u8,
) -> (
    Option<(Box<dyn flux_encode::traits::EncodeSession>, flux_core::types::Resolution)>,
    flux_core::types::EncoderBackend,
) {
    let encoder_config = flux_encode::traits::EncodeConfig {
        codec: flux_core::types::VideoCodec::H264,
        resolution,
        framerate: target_fps,
        // Placeholder; recomputed per-backend from the fitted resolution.
        bitrate_kbps: 10_000,
        rate_control: flux_core::types::RateControlMode::Vbr,
        dynamic_range: flux_core::types::DynamicRange::Sdr,
        chroma_sampling: flux_core::types::ChromaSampling::Yuv420,
        // Emit a keyframe every ~2s so a late joiner or any decode desync
        // recovers on its own without depending solely on a PLI/IDR request.
        gop_size: target_fps.saturating_mul(2).max(1),
        b_frames: 0,
        max_ref_frames: 1,
    };

    let mut backend = flux_core::types::EncoderBackend::Software;
    let mut session = None;
    for candidate in encoder_backend_candidates(forced_backend) {
        session = create_encode_session(candidate, encoder_config.clone(), quality_level);
        backend = candidate;
        if session.is_some() {
            break;
        }
        tracing::warn!("{:?} encoder unavailable; trying next backend", candidate);
    }
    (session, backend)
}

#[cfg(target_os = "windows")]
fn replug_capture(
    capture: &dyn flux_capture::traits::ScreenCapture,
    display: &mut VirtualDisplayHandle,
    mode: ModeRequest,
    cursor_sink: flux_capture::traits::CursorUpdateSink,
    target_fps: u32,
) -> Result<
    (
        flux_capture::traits::DisplayInfo,
        Box<dyn flux_capture::traits::CaptureSession>,
    ),
    String,
> {
    let before = capture
        .enumerate_displays()
        .map_err(|error| format!("enumerate displays before replug: {error}"))?;
    tracing::info!(
        "Resolution transition: unplugging virtual display for requested mode {}x{}@60Hz",
        mode.width,
        mode.height
    );
    display.unplug()?;
    let removal_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let after_unplug = loop {
        let displays = capture
            .enumerate_displays()
            .map_err(|error| format!("enumerate displays after unplug: {error}"))?;
        if displays.len() < before.len() {
            tracing::info!("Resolution transition: virtual output disappeared");
            break displays;
        }
        if std::time::Instant::now() >= removal_deadline {
            return Err("virtual output did not disappear after unplug".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let replug_baseline: std::collections::HashSet<String> =
        after_unplug.iter().map(|display| display.name.clone()).collect();
    display.plug_in_mode(mode.width, mode.height, 60)?;
    let arrival_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let target = loop {
        let displays = capture
            .enumerate_displays()
            .map_err(|error| format!("enumerate displays after plug-in: {error}"))?;
        if let Some(target) = displays
            .into_iter()
            .find(|display| !replug_baseline.contains(&display.name))
        {
            tracing::info!(
                "Resolution transition: virtual output arrived as {} on adapter LUID {:?}",
                target.name,
                target.adapter_luid
            );
            break target;
        }
        if std::time::Instant::now() >= arrival_deadline {
            return Err("virtual output did not reappear after plug-in".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    ccd_display::configure_virtual_display(&target, mode.width, mode.height, 60)
        .map_err(|error| format!("apply CCD mode {}x{}: {error}", mode.width, mode.height))?;
    let configured_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let target = loop {
        let displays = capture
            .enumerate_displays()
            .map_err(|error| format!("enumerate configured output: {error}"))?;
        if let Some(target) = displays.into_iter().find(|display| {
            !replug_baseline.contains(&display.name)
                && display.native_resolution.width == mode.width
                && display.native_resolution.height == mode.height
        }) {
            tracing::info!(
                "Resolution transition: CCD apply succeeded for {}x{}",
                mode.width,
                mode.height
            );
            break target;
        }
        if std::time::Instant::now() >= configured_deadline {
            return Err("virtual output did not settle at requested dimensions".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    let session = capture
        .start_capture(Some(target.id), target.native_resolution, target_fps, Some(cursor_sink))
        .map_err(|error| format!("restart capture after resolution transition: {error}"))?;
    tracing::info!(
        "Resolution transition: capture restarted at {}x{}",
        target.native_resolution.width,
        target.native_resolution.height
    );
    Ok((target, session))
}

/// Background thread: capture → hardware H.264 encode → broadcast channel.
/// Writes first ~5s of H.264 NALUs to a verification file.
#[allow(clippy::too_many_arguments)]
fn capture_loop(
    h264_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    cursor_tx: tokio::sync::watch::Sender<Arc<(u64, flux_core::cursor::CursorMetadata)>>,
    idr_rx: std::sync::mpsc::Receiver<()>,
    bitrate_rx: std::sync::mpsc::Receiver<u32>,
    quality_rx: std::sync::mpsc::Receiver<(u8, u8)>,
    resolution_rx: std::sync::mpsc::Receiver<ModeRequest>,
    input_rx: std::sync::mpsc::Receiver<(std::time::Instant, flux_input::InputEvent)>,
    target_fps: u32,
    default_fps_cap: u32,
    forced_backend: Option<flux_core::types::EncoderBackend>,
    default_quality_level: u8,
    target_display: Option<flux_capture::traits::DisplayInfo>,
    mut virtual_display: Option<VirtualDisplayHandle>,
    resolution_status_tx: tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
    runtime_status: Arc<std::sync::RwLock<registration::RuntimeStatus>>,
) {
    #[cfg(not(target_os = "windows"))]
    let _ = &virtual_display;
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

    let mut primary = if let Some(target) = target_display.as_ref() {
        match displays.iter().find(|display| {
            display.name == target.name && display.adapter_luid == target.adapter_luid
        }) {
            Some(display) => display.clone(),
            None => {
                tracing::error!(
                    "Virtual display output {} on adapter LUID {:?} is no longer present; refusing to capture another display",
                    target.name,
                    target.adapter_luid,
                );
                return;
            }
        }
    } else {
        displays
            .iter()
            .find(|d| d.primary)
            .unwrap_or(&displays[0])
            .clone()
    };
    if let Ok(mut status) = runtime_status.write() {
        status.display_name = Some(primary.name.clone());
        status.captured_virtual_display = Some(target_display.is_some());
    }
    
    // Initialize input using the selected output's virtual-desktop rectangle.
    let input_sink = match flux_input::InputSink::new(primary.desktop_rect) {
        Ok(sink) => Arc::new(sink),
        Err(e) => {
            tracing::error!("Failed to initialize input sink: {}", e);
            return;
        }
    };
    let loop_start = std::time::Instant::now();
    let cursor_dimensions = Arc::new(std::sync::RwLock::new((
        primary.native_resolution.width,
        primary.native_resolution.height,
        primary.native_resolution.width,
        primary.native_resolution.height,
    )));
    let cursor_sink: flux_capture::traits::CursorUpdateSink = {
        let cursor_tx = cursor_tx.clone();
        let cursor_dimensions = cursor_dimensions.clone();
        Arc::new(move |metadata| {
            let metadata = if let Ok(dimensions) = cursor_dimensions.read() {
                let (capture_width, capture_height, encode_width, encode_height) = *dimensions;
                let scale_x = encode_width as f32 / capture_width.max(1) as f32;
                let scale_y = encode_height as f32 / capture_height.max(1) as f32;
                let mut metadata = metadata;
                if let Some((x, y)) = metadata.position {
                    metadata.position = Some((
                        (x as f32 * scale_x).round() as i32,
                        (y as f32 * scale_y).round() as i32,
                    ));
                }
                if let Some(bitmap) = metadata.bitmap.as_ref() {
                    metadata.bitmap = Some(flux_capture::cursor::scale_cursor_bitmap(
                        bitmap, scale_x, scale_y,
                    ));
                }
                metadata
            } else {
                metadata
            };
            let ts_micros = loop_start.elapsed().as_micros() as u64;
            let _ = cursor_tx.send(Arc::new((ts_micros, metadata)));
        })
    };

    // Spawn a dedicated thread for input handling to ensure low latency
    // and avoid blocking the capture loop.
    let input_sink_thread = input_sink.clone();
    std::thread::spawn(move || {
        tracing::info!("Input dispatch thread started");
        let mut max_queue_delay = std::time::Duration::ZERO;
        let mut dispatched: u64 = 0;
        let mut last_report = std::time::Instant::now();
        let mut last_stall_warning: Option<std::time::Instant> = None;
        while let Ok((enqueued_at, event)) = input_rx.recv() {
            let queue_delay = enqueued_at.elapsed();
            max_queue_delay = max_queue_delay.max(queue_delay);
            dispatched += 1;
            // Rate-limited: a stalled dispatch thread releases a whole backlog
            // of late events at once, and one line per event drowns the log.
            if queue_delay > std::time::Duration::from_millis(20)
                && last_stall_warning
                    .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(5))
            {
                last_stall_warning = Some(std::time::Instant::now());
                tracing::warn!(
                    "Input event sat {:.1}ms in the dispatch queue (scheduler stall?)",
                    queue_delay.as_secs_f64() * 1000.0
                );
            }
            if let Err(e) = input_sink_thread.handle_event(&event) {
                tracing::warn!("Input injection error: {}", e);
            }
            if last_report.elapsed() >= std::time::Duration::from_secs(30) && dispatched > 0 {
                tracing::info!(
                    "Input dispatch: {} events, max queue delay {:.2}ms over last 30s",
                    dispatched,
                    max_queue_delay.as_secs_f64() * 1000.0
                );
                dispatched = 0;
                max_queue_delay = std::time::Duration::ZERO;
                last_report = std::time::Instant::now();
            }
        }
        tracing::info!("Input dispatch thread stopped");
    });

    let mut session = match capture.start_capture(
        Some(primary.id),
        primary.native_resolution,
        target_fps,
        Some(cursor_sink.clone()),
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
    let mut quality_level = default_quality_level.clamp(1, 10);
    let configured_fps = target_fps;
    let default_fps_cap = effective_default_fps_cap(configured_fps, default_fps_cap);
    let mut fps_cap = default_fps_cap;
    let mut last_encoded_at: Option<std::time::Instant> = None;
    let mut capture_resolution = flux_core::types::Resolution::new(0, 0);
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
    loop {
        if let Ok(request) = resolution_rx.try_recv() {
            #[cfg(target_os = "windows")]
            {
                match validate_mode_request(request) {
                    Err(error) => tracing::warn!(
                        "Ignoring invalid resolution request {}x{}: {}",
                        request.width,
                        request.height,
                        error
                    ),
                    Ok(request) if virtual_display.is_none() => {
                        tracing::warn!(
                            "Ignoring resolution request {}x{} because no virtual display is active",
                            request.width,
                            request.height
                        );
                    }
                    Ok(request)
                        if !mode_change_needed(
                            ModeRequest {
                                width: primary.native_resolution.width,
                                height: primary.native_resolution.height,
                            },
                            request,
                        ) =>
                    {
                        tracing::info!(
                            "Resolution transition: requested mode {}x{} already active; no-op",
                            request.width,
                            request.height
                        );
                        publish_resolution_status(
                            &resolution_status_tx,
                            ResolutionStatus {
                                state: "succeeded",
                                width: request.width,
                                height: request.height,
                                previous_width: None,
                                previous_height: None,
                                error: None,
                            },
                        );
                    }
                    Ok(request) => {
                        let old_mode = ModeRequest {
                            width: primary.native_resolution.width,
                            height: primary.native_resolution.height,
                        };
                        tracing::info!(
                            "Resolution transition: requested {}x{}, current {}x{}",
                            request.width,
                            request.height,
                            old_mode.width,
                            old_mode.height
                        );
                        let _ = session.stop();
                        drop(session);
                        encode_session = None;
                        capture_resolution = flux_core::types::Resolution::new(0, 0);
                        encode_resolution = flux_core::types::Resolution::new(0, 0);
                        let transition = replug_capture(
                            capture.as_ref(),
                            virtual_display.as_mut().expect("checked above"),
                            request,
                            cursor_sink.clone(),
                            target_fps,
                        );
                        match transition {
                            Ok((target, new_session)) => {
                                primary = target;
                                session = new_session;
                                let _ = input_sink.set_target_rect(primary.desktop_rect);
                                tracing::info!(
                                    "Resolution transition succeeded: {}x{}",
                                    request.width,
                                    request.height
                                );
                                publish_resolution_status(
                                    &resolution_status_tx,
                                    ResolutionStatus {
                                        state: "succeeded",
                                        width: request.width,
                                        height: request.height,
                                        previous_width: Some(old_mode.width),
                                        previous_height: Some(old_mode.height),
                                        error: None,
                                    },
                                );
                            }
                            Err(error) => {
                                tracing::error!(
                                    "Resolution transition failed: {}; starting rollback to {}x{}",
                                    error,
                                    old_mode.width,
                                    old_mode.height
                                );
                                match replug_capture(
                                    capture.as_ref(),
                                    virtual_display.as_mut().expect("checked above"),
                                    old_mode,
                                    cursor_sink.clone(),
                                    target_fps,
                                ) {
                                    Ok((target, restored_session)) => {
                                        primary = target;
                                        session = restored_session;
                                        let _ = input_sink.set_target_rect(primary.desktop_rect);
                                        tracing::warn!(
                                            "Resolution transition rollback succeeded; continuing at {}x{}",
                                            old_mode.width,
                                            old_mode.height
                                        );
                                        publish_resolution_status(
                                            &resolution_status_tx,
                                            ResolutionStatus {
                                                state: "failed",
                                                width: request.width,
                                                height: request.height,
                                                previous_width: Some(old_mode.width),
                                                previous_height: Some(old_mode.height),
                                                error: Some(format!(
                                                    "requested mode did not apply; still streaming at {}x{}",
                                                    old_mode.width, old_mode.height
                                                )),
                                            },
                                        );
                                    }
                                    Err(rollback_error) => {
                                        tracing::error!(
                                            "Resolution transition rollback failed: {}",
                                            rollback_error
                                        );
                                        publish_resolution_status(
                                            &resolution_status_tx,
                                            ResolutionStatus {
                                                state: "failed",
                                                width: request.width,
                                                height: request.height,
                                                previous_width: Some(old_mode.width),
                                                previous_height: Some(old_mode.height),
                                                error: Some(format!(
                                                    "transition and rollback failed: {rollback_error}"
                                                )),
                                            },
                                        );
                                        let mut retry_delay = std::time::Duration::from_secs(1);
                                        loop {
                                            std::thread::sleep(retry_delay);
                                            match replug_capture(
                                                capture.as_ref(),
                                                virtual_display.as_mut().expect("checked above"),
                                                old_mode,
                                                cursor_sink.clone(),
                                                target_fps,
                                            ) {
                                                Ok((target, restored_session)) => {
                                                    primary = target;
                                                    session = restored_session;
                                                    let _ = input_sink.set_target_rect(primary.desktop_rect);
                                                    tracing::warn!(
                                                        "Resolution transition recovery succeeded; continuing at {}x{}",
                                                        old_mode.width,
                                                        old_mode.height
                                                    );
                                                    break;
                                                }
                                                Err(recovery_error) => {
                                                    tracing::error!(
                                                        "Resolution transition recovery retry failed: {}; retrying in {:?}",
                                                        recovery_error,
                                                        retry_delay
                                                    );
                                                    retry_delay = std::cmp::min(
                                                        retry_delay.saturating_mul(2),
                                                        std::time::Duration::from_secs(30),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                tracing::warn!(
                    "Ignoring resolution request {}x{} on a non-Windows capture backend",
                    request.width,
                    request.height
                );
            }
        }
        // Check for IDR requests
        if let Ok(_) = idr_rx.try_recv() {
            tracing::info!("Handling IDR request from client");
            if let Some(ref mut enc) = encode_session {
                enc.request_idr();
            }
        }

        // Apply bitrate updates from relay congestion feedback (latest wins)
        let mut new_bitrate: Option<u32> = None;
        while let Ok(kbps) = bitrate_rx.try_recv() {
            new_bitrate = Some(kbps);
        }
        while let Ok((level, requested_fps)) = quality_rx.try_recv() {
            let valid_level = level <= 10;
            if (1..=10).contains(&level) {
                quality_level = level;
            } else if level == 0 {
                quality_level = default_quality_level.clamp(1, 10);
            } else {
                tracing::warn!("Ignoring out-of-range quality level {}", level);
            }
            let valid_fps = requested_fps <= 144;
            if requested_fps == 0 {
                fps_cap = default_fps_cap;
            } else if valid_fps {
                fps_cap = u32::from(requested_fps).min(configured_fps);
            } else {
                tracing::warn!("Ignoring out-of-range FPS cap {}", requested_fps);
            }
            if !valid_level && !valid_fps {
                continue;
            }
            if let Some(ref mut enc) = encode_session {
                let kbps = bitrate_kbps_for(encode_resolution, fps_cap, quality_level);
                if let Err(error) = enc.set_bitrate(kbps) {
                    tracing::warn!("quality level {} bitrate update failed: {}", quality_level, error);
                } else if let Ok(mut status) = runtime_status.write() {
                    status.target_bitrate_kbps = kbps;
                    if let Some(notify) = status.registration_notify.as_ref() {
                        let _ = notify.send(());
                    }
                }
            }
        }
        if let (Some(kbps), Some(ref mut enc)) = (new_bitrate, encode_session.as_mut()) {
            match enc.set_bitrate(kbps) {
                Ok(()) => tracing::info!("Encoder bitrate set to {} kbps", kbps),
                Err(e) => tracing::warn!("set_bitrate({} kbps) failed: {}", kbps, e),
            }
        }

        let t0 = std::time::Instant::now();

        let frame = match session.next_frame() {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("Capture error: {}", e);
                if !matches!(e, FluxError::CaptureSessionLost(_)) {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    continue;
                }
                let _ = session.stop();
                drop(session);
                loop {
                    let displays = match capture.enumerate_displays() {
                        Ok(displays) => displays,
                        Err(error) => {
                            tracing::warn!(
                                "Waiting for target display {} on adapter LUID {:?} after capture loss; display enumeration failed: {}",
                                primary.name,
                                primary.adapter_luid,
                                error
                            );
                            std::thread::sleep(std::time::Duration::from_secs(1));
                            continue;
                        }
                    };
                    let refreshed = displays.into_iter().find(|display| {
                        display.name == primary.name
                            && display.adapter_luid == primary.adapter_luid
                    });
                    let Some(refreshed) = refreshed else {
                        tracing::warn!(
                            "Waiting for target display {} on adapter LUID {:?} after capture loss; refusing to capture another display",
                            primary.name,
                            primary.adapter_luid
                        );
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        continue;
                    };
                    match capture.start_capture(
                        Some(refreshed.id),
                        refreshed.native_resolution,
                        target_fps,
                        Some(cursor_sink.clone()),
                    ) {
                        Ok(new_session) => {
                            session = new_session;
                            if let Err(error) = input_sink.set_target_rect(refreshed.desktop_rect) {
                                tracing::warn!("Failed to update input target rectangle: {}", error);
                            }
                            primary = refreshed;
                            tracing::info!("Capture session recreated after capture loss");
                            break;
                        }
                        Err(restart_error) => {
                            tracing::warn!(
                                "Capture session recreation failed: {}; retrying",
                                restart_error
                            );
                            std::thread::sleep(std::time::Duration::from_secs(1));
                        }
                    }
                }
                continue;
            }
        };

        let t_capture = t0.elapsed();
        if !should_encode_frame(&mut last_encoded_at, t0, fps_cap) {
            continue;
        }
        frame_count += 1;

        // (Re)build the encoder once the negotiated capture resolution is
        // known or whenever it changes (e.g. display rotation / mode switch),
        // so the encoder dimensions always match the frames it receives.
        if capture_resolution != frame.resolution {
            let (sess, backend) = build_encode_session_for(
                target_fps,
                frame.resolution,
                forced_backend,
                quality_level,
            );
            capture_resolution = frame.resolution;
            match sess {
                Some((s, res)) => {
                    encode_session = Some(s);
                    encode_resolution = res;
                }
                None => {
                    encode_session = None;
                    encode_resolution = frame.resolution;
                }
            }
            if let Ok(mut status) = runtime_status.write() {
                status.capture_width = capture_resolution.width;
                status.capture_height = capture_resolution.height;
                status.encode_width = encode_resolution.width;
                status.encode_height = encode_resolution.height;
                status.encoder_backend = Some(format!("{backend:?}"));
                status.target_bitrate_kbps =
                    bitrate_kbps_for(encode_resolution, fps_cap, quality_level);
                if let Some(notify) = status.registration_notify.as_ref() {
                    let _ = notify.send(());
                }
            }
            if let Ok(mut dimensions) = cursor_dimensions.write() {
                dimensions.0 = capture_resolution.width;
                dimensions.1 = capture_resolution.height;
                dimensions.2 = encode_resolution.width;
                dimensions.3 = encode_resolution.height;
            }
            tracing::info!(
                "Capture+encode loop: {}x{} captured → {}x{}@{}fps {:?} H.264",
                capture_resolution.width, capture_resolution.height,
                encode_resolution.width, encode_resolution.height, target_fps, backend
            );
            if let Some(ref mut enc) = encode_session {
                enc.request_idr_for_rebuild();
            }

            // If the encoder had to be opened below the capture size (e.g.
            // 5120x2160 exceeds AMD's 4096x4096 H.264 limit), restart the
            // capture session at the encode resolution so the backend scales
            // on the GPU (DXGI video-processor blit) and frames arrive
            // already sized for the encoder.
            if encode_session.is_some() && encode_resolution != capture_resolution {
                // Release the current session first: DXGI allows only one
                // active IDXGIOutputDuplication per output per process, so
                // DuplicateOutput fails while the old one is alive.
                let _ = session.stop();
                drop(session);
                match capture.start_capture(
                    Some(primary.id),
                    encode_resolution,
                    target_fps,
                    Some(cursor_sink.clone()),
                ) {
                    Ok(s) => {
                        session = s;
                        if let Ok(mut dimensions) = cursor_dimensions.write() {
                            dimensions.0 = encode_resolution.width;
                            dimensions.1 = encode_resolution.height;
                            dimensions.2 = encode_resolution.width;
                            dimensions.3 = encode_resolution.height;
                        }
                        capture_resolution = encode_resolution;
                        tracing::info!("Capture restarted with GPU downscale to {}", encode_resolution);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to restart capture at {}: {} — falling back to CPU downscale",
                            encode_resolution, e
                        );
                        session = match capture.start_capture(
                            Some(primary.id),
                            primary.native_resolution,
                            target_fps,
                            Some(cursor_sink.clone()),
                        ) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::error!("Failed to restore capture session: {}", e);
                                return;
                            }
                        };
                        if let Ok(mut dimensions) = cursor_dimensions.write() {
                            dimensions.0 = primary.native_resolution.width;
                            dimensions.1 = primary.native_resolution.height;
                            dimensions.2 = encode_resolution.width;
                            dimensions.3 = encode_resolution.height;
                        }
                    }
                }
            }
        }

        // CPU fallback when GPU-side scaling isn't available: downscale the
        // frame before encoding when the encoder session is smaller.
        let frame = if frame.resolution != encode_resolution {
            match flux_encode::scale::downscale_frame(&frame, encode_resolution) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!("Frame downscale failed: {}", e);
                    continue;
                }
            }
        } else {
            frame
        };

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
                (total_encoded_bytes as f64 * 8.0 / wall / 1000.0) as u64
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

#[cfg(test)]
mod quality_tests {
    use super::*;

    #[test]
    fn quality_levels_map_to_expected_1080p60_targets() {
        let resolution = flux_core::types::Resolution::new(1920, 1080);
        assert_eq!(bitrate_kbps_for(resolution, 60, 3), 6_221);
        assert_eq!(bitrate_kbps_for(resolution, 60, 6), 12_442);
        assert_eq!(bitrate_kbps_for(resolution, 60, 10), 24_883);
    }

    #[test]
    fn fps_cap_reduces_target_linearly() {
        let resolution = flux_core::types::Resolution::new(1920, 1080);
        assert_eq!(bitrate_kbps_for(resolution, 48, 6), 9_953);
    }

    #[test]
    fn pacing_accepts_native_interval_and_small_jitter() {
        let start = std::time::Instant::now();
        let interval = std::time::Duration::from_secs_f64(1.0 / 48.0);
        let mut last = Some(start);
        assert!(should_encode_frame(&mut last, start + interval, 48));
        assert!(should_encode_frame(
            &mut last,
            start + interval + interval.mul_f64(0.95),
            48
        ));
    }
}

#[cfg(test)]
mod mode_request_tests {
    use super::*;

    #[test]
    fn rejects_invalid_mode_requests() {
        assert!(validate_mode_request(ModeRequest { width: 2561, height: 1440 }).is_err());
        assert!(validate_mode_request(ModeRequest { width: 1921, height: 1080 }).is_err());
        assert!(validate_mode_request(ModeRequest { width: 320, height: 240 }).is_err());
    }

    #[test]
    fn accepts_and_preserves_valid_mode_requests() {
        let request = ModeRequest { width: 2560, height: 1080 };
        assert_eq!(validate_mode_request(request).unwrap().width, 2560);
        assert!(!mode_change_needed(request, request));
        assert!(mode_change_needed(request, ModeRequest { width: 1920, height: 1080 }));
    }

    #[test]
    fn default_fps_cap_is_used_for_startup_and_auto() {
        assert_eq!(effective_default_fps_cap(60, 48), 48);
        assert_eq!(effective_default_fps_cap(30, 48), 30);
    }
}
