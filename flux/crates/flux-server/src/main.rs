use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
#[cfg(feature = "tray")]
use parking_lot::RwLock;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[cfg(all(target_os = "linux", feature = "audio-pipewire"))]
mod audio;
mod capture_loop;
mod clipboard;
mod display_platform;
mod encoder;
mod frame_protocol;
mod frame_server;
mod http;
mod metrics;
mod quic_frames;
mod resolution;

#[cfg(target_os = "windows")]
mod ccd_display;
#[cfg(target_os = "windows")]
mod virtual_display;
#[cfg(feature = "tray")]
mod tray;

use clipboard::ClipboardCtx;
use display_platform::PrivacyController;
use flux_core::config::FluxConfig;
use flux_core::platform::PlatformInfo;
use flux_crypto::CertificateManager;
use resolution::{ModeRequest, ResolutionStatusMessage};
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
    flux_crypto::install_crypto_provider();

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
    let mut config = if args.config.exists() {
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
    if let Ok(addr) = std::env::var("FLUX_METRICS_ADDR")
        && !addr.trim().is_empty()
    {
        config.server.metrics_addr = addr.trim().to_owned();
    }
    metrics::init();

    let privacy_snapshot_path = config
        .security
        .cert_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("privacy_topology.bin");
    display_platform::recover_snapshot(&privacy_snapshot_path);
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
    let gpu_vendor = format!("{:?}", platform.gpu_vendor);
    metrics::set_host_info("unknown", &gpu_vendor, std::env::consts::OS);
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
                                        // Not implemented: there is no pairing listener that would accept a PIN.
                                    }
                                    TrayAction::OpenConfig => {
                                        tracing::info!("Tray: Open config requested");
                                        // Not implemented: opening the config file in an editor.
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
            let Some(after_unplug) = capture_loop::wait_for_displays(
                capture_probe.as_ref(),
                std::time::Duration::from_secs(2),
                "existing virtual DXGI output removal",
                |displays| (displays.len() < display_count_before_unplug).then_some(displays),
            ) else {
                drop(display);
                return Err("existing virtual display could not be removed within 2 seconds".into());
            };
            before_names = after_unplug
                .into_iter()
                .map(|display| display.name)
                .collect();
            display
                .plug_in_mode(vd.width, vd.height, vd.refresh_hz)
                .map_err(|e| format!("re-plug virtual display: {e}"))?;
        }
        let Some(target) = capture_loop::wait_for_displays(
            capture_probe.as_ref(),
            std::time::Duration::from_secs(10),
            "virtual DXGI output",
            |displays| displays.into_iter().find(|output| !before_names.contains(&output.name)),
        ) else {
            drop(display);
            return Err("virtual display plugged in but no new DXGI output appeared within 10 seconds".into());
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

        let Some(target) = capture_loop::wait_for_displays(
            capture_probe.as_ref(),
            std::time::Duration::from_secs(5),
            "configured virtual DXGI output",
            |displays| {
                displays.into_iter().find(|output| {
                    output.name == target.name
                        && output.adapter_luid == target.adapter_luid
                        && output.native_resolution.width == vd.width
                        && output.native_resolution.height == vd.height
                })
            },
        ) else {
            drop(display);
            return Err(format!(
                "virtual display {} did not appear at configured mode {}x{}@{}Hz after CCD apply",
                target.name, vd.width, vd.height, vd.refresh_hz
            )
            .into());
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
    let (virtual_display, virtual_display_target): (Option<display_platform::VirtualDisplayHandle>, _) = (None, None);

    let privacy_controller = if config.video.privacy.enabled {
        Some(
            PrivacyController::new(
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

    // ── Start capture → hardware H.264 encode ──────────────────────
    // Broadcast channel: capture thread sends, TCP frame server(s) receive.
    // Each message carries the frame's capture timestamp (microseconds since
    // capture start) so downstream playout can pace by true capture spacing
    // rather than bursty network arrival time.
    // Shared epoch for capture/audio timestamps so video and audio play out
    // against the same clock.
    let stream_epoch = Instant::now();
    let (h264_tx, _) = tokio::sync::broadcast::channel::<Arc<(u64, Vec<u8>)>>(8);
    let (audio_tx, _) = tokio::sync::broadcast::channel::<Arc<(u64, Vec<u8>)>>(64);
    let clipboard = ClipboardCtx {
        sink: flux_capture::traits::ClipboardSink::default(),
        events: tokio::sync::broadcast::channel::<Arc<(u64, Vec<u8>)>>(16).0,
        epoch: stream_epoch,
        enabled: config.clipboard.enabled,
        max_bytes: config.clipboard.max_bytes,
    };
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
    let runtime_status = Arc::new(std::sync::RwLock::new(flux_core::registration::RuntimeStatus::default()));
    let runtime_status_capture = runtime_status.clone();
    let resolution_status_tx_capture = resolution_status_tx.clone();
    let initial_mode = config.video.virtual_display;
    let gpu_vendor_capture = gpu_vendor.clone();
    let clipboard_capture = clipboard.clone();
    let capture_loop = capture_loop::CaptureLoop {
        h264_tx: h264_tx2,
        stream_epoch,
        cursor_tx: cursor_tx2,
        idr_rx,
        bitrate_rx,
        quality_rx,
        resolution_rx,
        input_rx,
        clipboard: clipboard_capture,
        target_fps: capture_fps,
        default_fps_cap: config.video.default_fps_cap,
        forced_backend: forced_encoder,
        default_quality_level,
        gpu_vendor: gpu_vendor_capture,
        target_display: virtual_display_target,
        virtual_display,
        initial_mode,
        resolution_status_tx: resolution_status_tx_capture,
        runtime_status: runtime_status_capture,
    };
    std::thread::Builder::new()
        .name("flux-capture".into())
        .spawn(move || capture_loop.run())?;

    // ── Desktop audio capture → Opus encode → broadcast ──
    #[cfg(all(target_os = "linux", feature = "audio-pipewire"))]
    if config.audio.enabled {
        let audio_tx_thread = audio_tx.clone();
        let audio_config = config.audio.clone();
        std::thread::Builder::new()
            .name("flux-audio".into())
            .spawn(move || audio::audio_loop(audio_tx_thread, stream_epoch, audio_config))
            .map_err(|e| format!("spawn audio thread: {e}"))?;
    }

    // ── Start TCP frame server (for Go relay) ─────────────
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
    let machine_id_path = std::path::PathBuf::from(&config.security.cert_path)
        .with_file_name("flux-machine-id");
    let registration_stop = flux_core::registration::start(
        &config,
        &platform,
        advertised_frame_addr,
        runtime_status.clone(),
        &machine_id_path,
    );
    tracing::info!("H.264 frame server listening on tcp://{}", frame_addr);

    let metrics_handle = if config.server.metrics_addr.eq_ignore_ascii_case("off") {
        None
    } else {
        match tokio::net::TcpListener::bind(&config.server.metrics_addr).await {
            Ok(listener) => {
                tracing::info!("Metrics listener on http://{}/metrics", config.server.metrics_addr);
                Some(tokio::spawn(async move {
                    if let Err(error) = axum::serve(listener, http::build_router()).await {
                        tracing::warn!("Metrics HTTP server stopped: {}", error);
                    }
                }))
            }
            Err(error) => {
                tracing::warn!(
                    "Metrics listener {} unavailable: {}; streaming continues",
                    config.server.metrics_addr,
                    error
                );
                None
            }
        }
    };

    // Report runtime status (encoder backend etc.) to flux-hostd when running
    // as the per-user agent: written once the encoder backend is known.
    if let Ok(agent_status_path) = std::env::var("FLUX_AGENT_STATUS_FILE") {
        let status = runtime_status.clone();
        tokio::spawn(async move {
            let mut last = String::new();
            loop {
                let snapshot = status.read().ok().map(|s| {
                    serde_json::json!({
                        "encoder_backend": s.encoder_backend,
                        "width": s.encode_width,
                        "height": s.encode_height,
                        "virtual_display": s.captured_virtual_display,
                        "display_name": s.display_name,
                    })
                });
                if let Some(json) = snapshot {
                    let text = json.to_string();
                    if text != last {
                        let tmp = format!("{agent_status_path}.tmp");
                        if std::fs::write(&tmp, &text).is_ok() {
                            let _ = std::fs::rename(&tmp, &agent_status_path);
                        }
                        last = text;
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }

    let commands = frame_protocol::CommandChannels {
        idr_tx,
        bitrate_tx,
        quality_tx,
        resolution_tx,
        input_tx,
        clipboard: clipboard.clone(),
    };

    // QUIC frame server on the same port number over UDP. Preferred by the
    // relay on lossy links (no head-of-line blocking); TCP remains as fallback.
    let quic_handle = if !config.server.frame_quic {
        None
    } else {
        match frame_addr.parse::<std::net::SocketAddr>() {
        Ok(bind_addr) => match quic_frames::make_endpoint(bind_addr, &cert_manager) {
            Ok(endpoint) => {
                tracing::info!("H.264 frame server listening on quic://{}", frame_addr);
                let h264_tx = h264_tx.clone();
                let audio_tx = audio_tx.clone();
                let cursor_rx = cursor_tx.subscribe();
                let commands = commands.clone();
                let resolution_status_tx = resolution_status_tx.clone();
                let privacy_controller = privacy_controller.clone();
                Some(tokio::spawn(async move {
                    quic_frames::serve(
                        endpoint,
                        h264_tx,
                        audio_tx,
                        cursor_rx,
                        commands,
                        resolution_status_tx,
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
        }
    };

    let frame_privacy_controller = privacy_controller.clone();
    let cursor_rx = cursor_tx.subscribe();
    let frame_handle = tokio::spawn(async move {
        frame_server::serve(
            frame_listener,
            h264_tx,
            audio_tx.clone(),
            cursor_rx,
            commands,
            resolution_status_tx,
            frame_privacy_controller,
        )
        .await;
    });

    // Build the server.
    let _server = FluxServer::new(config, platform, cert_manager, authenticator).await?;

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
    if let Some(handle) = metrics_handle {
        handle.abort();
    }
    if let Some(h) = quic_handle {
        h.abort();
    }
    if let Some(privacy) = privacy_controller
        && let Err(error) = privacy.restore()
    {
        tracing::error!("Failed to restore privacy display topology: {error}");
    }
    if let Some(handle) = registration_stop {
        let _ = handle.stop.send(());
    }
    tracing::info!("Flux Server stopped.");
    Ok(())
}

/// The top-level Flux server orchestrating all subsystems.
struct FluxServer {
    _config: FluxConfig,
    _platform: PlatformInfo,
    _cert_manager: CertificateManager,
}

impl FluxServer {
    async fn new(
        config: FluxConfig,
        platform: PlatformInfo,
        cert_manager: CertificateManager,
        _authenticator: flux_crypto::PinAuthenticator,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // main() already runs the relay frame servers (TCP, optional QUIC) and
        // the plain-HTTP `/api/status` + `/metrics` listener. Still missing:
        // - a client signaling/pairing listener on `server.signaling_port`
        //   (nothing drives flux-protocol `SessionNegotiation` or `_authenticator`);
        // - mDNS/zeroconf advertisement;
        // - the HTTPS control API on `server.https_port`.

        Ok(Self {
            _config: config,
            _platform: platform,
            _cert_manager: cert_manager,
        })
    }
}
