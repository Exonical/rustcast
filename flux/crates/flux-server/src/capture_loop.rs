//! The capture thread: capture → encode → broadcast, plus the capture
//! session restart, resolution transition and recovery paths it drives.

use std::sync::Arc;
use std::time::Instant;

use flux_core::error::FluxError;

use crate::VirtualDisplayHandle;
#[cfg(target_os = "windows")]
use crate::ccd_display;
use crate::clipboard::{ClipboardCtx, attach_clipboard};
use crate::encoder::{EncoderRetry, bitrate_kbps_for, build_encode_session_for};
#[cfg(not(target_os = "windows"))]
use crate::resolution::{MODE_REQUEST_TIMEOUT, PendingMode, PendingModeState};
use crate::resolution::{
    ModeRequest, ResolutionStatus, ResolutionStatusMessage, ResolutionStatusTracker, mode_change_needed,
    publish_resolution_status, validate_mode_request,
};

fn effective_default_fps_cap(configured: u32, requested_default: u32) -> u32 {
    requested_default.min(configured)
}

#[derive(Debug, Default)]
struct FramePacer {
    last_input_at: Option<std::time::Instant>,
    credit: f64,
}

const FRAME_PACING_ACCEPT_THRESHOLD: f64 = 0.90;

fn should_encode_frame(
    pacer: &mut FramePacer,
    now: std::time::Instant,
    fps_cap: u32,
) -> bool {
    let fps = fps_cap.max(1) as f64;
    let Some(last) = pacer.last_input_at.replace(now) else {
        pacer.credit = 0.0;
        return true;
    };

    let elapsed = now.duration_since(last).as_secs_f64();
    if elapsed >= 1.0 / fps {
        pacer.credit = 0.0;
        return true;
    }

    pacer.credit += elapsed * fps;
    if pacer.credit >= FRAME_PACING_ACCEPT_THRESHOLD {
        pacer.credit -= 1.0;
        true
    } else {
        false
    }
}

/// Per-frame capture and encode durations accumulated over a reporting window.
/// A single instantaneous sample hides exactly the frames that matter, since the
/// stalls being chased are occasional: only the maxima show them, and keyframes
/// are tracked separately because their cost is what the rest of the pipeline
/// has to absorb.
#[derive(Debug, Default)]
struct StageTimings {
    frames: u64,
    capture_sum: std::time::Duration,
    capture_max: std::time::Duration,
    encode_sum: std::time::Duration,
    encode_max: std::time::Duration,
    keyframes: u64,
    keyframe_encode_max: std::time::Duration,
    keyframe_bytes_max: usize,
}

impl StageTimings {
    fn observe(
        &mut self,
        capture: std::time::Duration,
        encode: std::time::Duration,
        keyframe_bytes: Option<usize>,
    ) {
        crate::metrics::encode_seconds(encode.as_secs_f64());
        self.frames += 1;
        self.capture_sum += capture;
        self.capture_max = self.capture_max.max(capture);
        self.encode_sum += encode;
        self.encode_max = self.encode_max.max(encode);
        if let Some(bytes) = keyframe_bytes {
            self.keyframes += 1;
            self.keyframe_encode_max = self.keyframe_encode_max.max(encode);
            self.keyframe_bytes_max = self.keyframe_bytes_max.max(bytes);
        }
    }

    /// Formats the window's timings, or `None` if no frames were observed.
    fn summary(&self) -> Option<String> {
        if self.frames == 0 {
            return None;
        }
        let frames = self.frames as f64;
        let ms = |duration: std::time::Duration| duration.as_secs_f64() * 1000.0;
        let mut summary = format!(
            "capture avg={:.1}ms max={:.1}ms | encode avg={:.1}ms max={:.1}ms",
            ms(self.capture_sum) / frames,
            ms(self.capture_max),
            ms(self.encode_sum) / frames,
            ms(self.encode_max),
        );
        if self.keyframes > 0 {
            summary += &format!(
                " | keyframes n={} max encode={:.1}ms max={} bytes",
                self.keyframes,
                ms(self.keyframe_encode_max),
                self.keyframe_bytes_max,
            );
        }
        Some(summary)
    }
}

#[cfg(target_os = "windows")]
fn replug_capture(
    starter: &CaptureStarter<'_>,
    display: &mut VirtualDisplayHandle,
    mode: ModeRequest,
) -> Result<
    (
        flux_capture::traits::DisplayInfo,
        Box<dyn flux_capture::traits::CaptureSession>,
    ),
    String,
> {
    let capture = starter.capture;
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
    let session = starter
        .restart_on(&target)
        .map_err(|error| format!("restart capture after resolution transition: {error}"))?;
    tracing::info!(
        "Resolution transition: capture restarted at {}x{}",
        target.native_resolution.width,
        target.native_resolution.height
    );
    Ok((target, session))
}

/// Route input through the capture session's own injection backend, when it has one.
fn attach_input_backend(
    input_sink: &flux_input::InputSink,
    session: &dyn flux_capture::traits::CaptureSession,
) {
    #[cfg(not(target_os = "windows"))]
    input_sink.set_backend(session.input_backend());
    #[cfg(target_os = "windows")]
    let _ = (input_sink, session);
}

/// Starts capture sessions for `capture_loop`, wiring each new session to the
/// shared cursor and input sinks.
struct CaptureStarter<'a> {
    capture: &'a dyn flux_capture::traits::ScreenCapture,
    cursor_sink: &'a flux_capture::traits::CursorUpdateSink,
    input_sink: &'a flux_input::InputSink,
    clipboard: &'a ClipboardCtx,
    target_fps: u32,
}

impl CaptureStarter<'_> {
    /// Start capturing `display` at `resolution` and route input through the new session.
    fn start(
        &self,
        display: &flux_capture::traits::DisplayInfo,
        resolution: flux_core::types::Resolution,
    ) -> flux_core::Result<Box<dyn flux_capture::traits::CaptureSession>> {
        let session = self.capture.start_capture(
            Some(display.id),
            resolution,
            self.target_fps,
            Some(self.cursor_sink.clone()),
        )?;
        attach_input_backend(self.input_sink, session.as_ref());
        attach_clipboard(self.clipboard, session.as_ref());
        Ok(session)
    }

    /// Start capturing a (possibly re-enumerated) `display` at its native
    /// resolution and point absolute input at its desktop rectangle.
    fn restart_on(
        &self,
        display: &flux_capture::traits::DisplayInfo,
    ) -> flux_core::Result<Box<dyn flux_capture::traits::CaptureSession>> {
        let session = self.start(display, display.native_resolution)?;
        if let Err(error) = self.input_sink.set_target_rect(display.desktop_rect) {
            tracing::warn!("Failed to update input target rectangle: {}", error);
        }
        Ok(session)
    }
}

/// Stop and drop a capture session. DXGI allows only one active
/// IDXGIOutputDuplication per output per process, so the old session must be
/// gone before its replacement can start.
fn release_capture_session(mut session: Box<dyn flux_capture::traits::CaptureSession>) {
    let _ = session.stop();
}

/// Record the capture and encode sizes the cursor sink scales positions and bitmaps between.
fn set_cursor_dimensions(
    dimensions: &std::sync::RwLock<(u32, u32, u32, u32)>,
    capture: flux_core::types::Resolution,
    encode: flux_core::types::Resolution,
) {
    if let Ok(mut dimensions) = dimensions.write() {
        *dimensions = (capture.width, capture.height, encode.width, encode.height);
    }
}

/// Resolve a pending compositor-side mode change against the latest frame:
/// on success adopt the new mode as the primary display's and retarget
/// absolute input; on timeout report the failure.
#[cfg(not(target_os = "windows"))]
fn settle_pending_mode(
    pending: &mut Option<PendingMode>,
    frame: Option<flux_core::types::Resolution>,
    primary: &mut flux_capture::traits::DisplayInfo,
    input_sink: &flux_input::InputSink,
    tx: &tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
    status_tracker: &mut ResolutionStatusTracker,
) {
    let Some(request) = *pending else {
        return;
    };
    match request.state(frame, std::time::Instant::now()) {
        PendingModeState::Waiting => {}
        PendingModeState::Applied => {
            primary.native_resolution = request.requested;
            primary.desktop_rect.width = request.requested.width;
            primary.desktop_rect.height = request.requested.height;
            if let Err(error) = input_sink.set_target_rect(primary.desktop_rect) {
                tracing::warn!("Failed to update input target rectangle: {}", error);
            }
            tracing::info!("Resolution request applied: now {}", request.requested);
            status_tracker.on_transition_applied(ResolutionStatus {
                state: "succeeded",
                width: request.requested.width,
                height: request.requested.height,
                previous_width: Some(request.previous.width),
                previous_height: Some(request.previous.height),
                error: None,
                encode_width: None,
                encode_height: None,
            });
            *pending = None;
        }
        PendingModeState::TimedOut => {
            tracing::warn!(
                "Resolution request {} was not applied within {:?}",
                request.requested,
                MODE_REQUEST_TIMEOUT
            );
            status_tracker
                .on_transition_failed(request.requested.width, request.requested.height);
            publish_resolution_status(
                tx,
                ResolutionStatus {
                    state: "failed",
                    width: request.requested.width,
                    height: request.requested.height,
                    previous_width: Some(request.previous.width),
                    previous_height: Some(request.previous.height),
                    error: Some("compositor did not apply the mode".into()),
                    encode_width: None,
                    encode_height: None,
                },
            );
            *pending = None;
        }
    }
}

/// Apply a requested virtual-display mode. If it does not apply, roll back to
/// `old_mode`, then keep retrying the rollback with exponential backoff until
/// capture is restored. The caller must already have released its session.
#[cfg(target_os = "windows")]
fn transition_resolution(
    starter: &CaptureStarter<'_>,
    display: &mut VirtualDisplayHandle,
    request: ModeRequest,
    old_mode: ModeRequest,
    resolution_status_tx: &tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
    status_tracker: &mut ResolutionStatusTracker,
) -> (
    flux_capture::traits::DisplayInfo,
    Box<dyn flux_capture::traits::CaptureSession>,
) {
    let publish = |state: &'static str, error: Option<String>| {
        publish_resolution_status(
            resolution_status_tx,
            ResolutionStatus {
                state,
                width: request.width,
                height: request.height,
                previous_width: Some(old_mode.width),
                previous_height: Some(old_mode.height),
                error,
                encode_width: None,
                encode_height: None,
            },
        );
    };
    let error = match replug_capture(starter, display, request) {
        Ok(restored) => {
            tracing::info!("Resolution transition succeeded: {}x{}", request.width, request.height);
            status_tracker.on_transition_applied(ResolutionStatus {
                state: "succeeded",
                width: request.width,
                height: request.height,
                previous_width: Some(old_mode.width),
                previous_height: Some(old_mode.height),
                error: None,
                encode_width: None,
                encode_height: None,
            });
            return restored;
        }
        Err(error) => error,
    };
    tracing::error!(
        "Resolution transition failed: {}; starting rollback to {}x{}",
        error,
        old_mode.width,
        old_mode.height
    );
    let rollback_error = match replug_capture(starter, display, old_mode) {
        Ok(restored) => {
            tracing::warn!(
                "Resolution transition rollback succeeded; continuing at {}x{}",
                old_mode.width,
                old_mode.height
            );
            status_tracker.on_transition_failed(request.width, request.height);
            publish(
                "failed",
                Some(format!(
                    "requested mode did not apply; still streaming at {}x{}",
                    old_mode.width, old_mode.height
                )),
            );
            return restored;
        }
        Err(rollback_error) => rollback_error,
    };
    tracing::error!("Resolution transition rollback failed: {}", rollback_error);
    status_tracker.on_transition_failed(request.width, request.height);
    publish(
        "failed",
        Some(format!("transition and rollback failed: {rollback_error}")),
    );
    let mut retry_delay = std::time::Duration::from_secs(1);
    loop {
        std::thread::sleep(retry_delay);
        match replug_capture(starter, display, old_mode) {
            Ok(restored) => {
                tracing::warn!(
                    "Resolution transition recovery succeeded; continuing at {}x{}",
                    old_mode.width,
                    old_mode.height
                );
                return restored;
            }
            Err(recovery_error) => {
                tracing::error!(
                    "Resolution transition recovery retry failed: {}; retrying in {:?}",
                    recovery_error,
                    retry_delay
                );
                retry_delay = std::cmp::min(retry_delay.saturating_mul(2), std::time::Duration::from_secs(30));
            }
        }
    }
}

/// Wait for the `lost` display (same name and adapter LUID) to reappear and
/// restart capture on it, retrying every second. Never falls back to a
/// different display. The caller must already have released its session.
fn recover_lost_capture(
    starter: &CaptureStarter<'_>,
    lost: &flux_capture::traits::DisplayInfo,
) -> (
    flux_capture::traits::DisplayInfo,
    Box<dyn flux_capture::traits::CaptureSession>,
) {
    loop {
        let displays = match starter.capture.enumerate_displays() {
            Ok(displays) => displays,
            Err(error) => {
                tracing::warn!(
                    "Waiting for target display {} on adapter LUID {:?} after capture loss; display enumeration failed: {}",
                    lost.name,
                    lost.adapter_luid,
                    error
                );
                std::thread::sleep(std::time::Duration::from_secs(1));
                continue;
            }
        };
        let refreshed = displays
            .into_iter()
            .find(|display| display.name == lost.name && display.adapter_luid == lost.adapter_luid);
        let Some(refreshed) = refreshed else {
            tracing::warn!(
                "Waiting for target display {} on adapter LUID {:?} after capture loss; refusing to capture another display",
                lost.name,
                lost.adapter_luid
            );
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        };
        // A compositor virtual monitor has no real geometry to re-read: keep
        // the mode it was last running at.
        #[cfg(target_os = "linux")]
        let refreshed = if refreshed.name == flux_capture::backend::pipewire::VIRTUAL_MONITOR_NAME {
            flux_capture::traits::DisplayInfo {
                native_resolution: lost.native_resolution,
                desktop_rect: lost.desktop_rect,
                ..refreshed
            }
        } else {
            refreshed
        };
        match starter.restart_on(&refreshed) {
            Ok(session) => {
                tracing::info!("Capture session recreated after capture loss");
                return (refreshed, session);
            }
            Err(restart_error) => {
                tracing::warn!("Capture session recreation failed: {}; retrying", restart_error);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
    }
}

/// Capture session obtained by [`restart_for_downscale`].
enum DownscaleRestart {
    /// Capture delivers frames already scaled to the encode resolution on the GPU.
    Gpu(Box<dyn flux_capture::traits::CaptureSession>),
    /// GPU scaling was unavailable; capture resumed at the display's native
    /// resolution and frames must be downscaled on the CPU.
    Cpu(Box<dyn flux_capture::traits::CaptureSession>),
}

/// Restart capture on `display` at `encode_resolution` so the backend scales
/// on the GPU, falling back to native-resolution capture. The caller must
/// already have released its session.
fn restart_for_downscale(
    starter: &CaptureStarter<'_>,
    display: &flux_capture::traits::DisplayInfo,
    encode_resolution: flux_core::types::Resolution,
) -> flux_core::Result<DownscaleRestart> {
    match starter.start(display, encode_resolution) {
        Ok(session) => Ok(DownscaleRestart::Gpu(session)),
        Err(error) => {
            tracing::warn!(
                "Failed to restart capture at {}: {} — falling back to CPU downscale",
                encode_resolution,
                error
            );
            starter
                .start(display, display.native_resolution)
                .map(DownscaleRestart::Cpu)
        }
    }
}

/// The capture thread's inputs: channels to and from the frame servers,
/// encoder and FPS settings, and the display to capture.
pub(crate) struct CaptureLoop {
    pub(crate) h264_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    pub(crate) stream_epoch: Instant,
    pub(crate) cursor_tx: tokio::sync::watch::Sender<Arc<(u64, flux_core::cursor::CursorMetadata)>>,
    pub(crate) idr_rx: std::sync::mpsc::Receiver<()>,
    pub(crate) bitrate_rx: std::sync::mpsc::Receiver<u32>,
    pub(crate) quality_rx: std::sync::mpsc::Receiver<(u8, u8)>,
    pub(crate) resolution_rx: std::sync::mpsc::Receiver<ModeRequest>,
    pub(crate) input_rx: std::sync::mpsc::Receiver<(std::time::Instant, flux_input::InputEvent)>,
    pub(crate) clipboard: ClipboardCtx,
    pub(crate) target_fps: u32,
    pub(crate) default_fps_cap: u32,
    pub(crate) forced_backend: Option<flux_core::types::EncoderBackend>,
    pub(crate) default_quality_level: u8,
    pub(crate) gpu_vendor: String,
    pub(crate) target_display: Option<flux_capture::traits::DisplayInfo>,
    pub(crate) virtual_display: Option<VirtualDisplayHandle>,
    pub(crate) initial_mode: Option<flux_core::config::VirtualDisplayConfig>,
    pub(crate) resolution_status_tx: tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
    pub(crate) runtime_status: Arc<std::sync::RwLock<flux_core::registration::RuntimeStatus>>,
}

impl CaptureLoop {
    /// Background thread: capture → hardware H.264 encode → broadcast channel.
    /// Writes first ~5s of H.264 NALUs to a verification file.
    pub(crate) fn run(self) {
        let Self {
            h264_tx,
            stream_epoch,
            cursor_tx,
            idr_rx,
            bitrate_rx,
            quality_rx,
            resolution_rx,
            input_rx,
            clipboard,
            target_fps,
            default_fps_cap,
            forced_backend,
            default_quality_level,
            gpu_vendor,
            target_display,
            virtual_display,
            initial_mode,
            resolution_status_tx,
            runtime_status,
        } = self;
        #[cfg(target_os = "windows")]
        let mut virtual_display = virtual_display;
        crate::metrics::set_host_info("unknown", &gpu_vendor, std::env::consts::OS);
        #[cfg(not(target_os = "windows"))]
        let _ = &virtual_display;
        #[cfg(target_os = "windows")]
        let _ = initial_mode;
        #[cfg(not(target_os = "windows"))]
        let mut target_fps = target_fps;
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
        // On Linux the capture backend may own a compositor virtual monitor whose
        // mode is whatever we ask for; use the configured one from the start.
        #[cfg(target_os = "linux")]
        let owns_virtual_monitor = primary.name == flux_capture::backend::pipewire::VIRTUAL_MONITOR_NAME;
        #[cfg(not(target_os = "linux"))]
        let owns_virtual_monitor = false;
        #[cfg(not(target_os = "windows"))]
        if owns_virtual_monitor && let Some(mode) = initial_mode {
            primary.native_resolution = flux_core::types::Resolution::new(mode.width, mode.height);
            primary.desktop_rect = flux_core::types::DesktopRect {
                left: 0,
                top: 0,
                width: mode.width,
                height: mode.height,
            };
            target_fps = mode.refresh_hz.clamp(1, 144);
            tracing::info!(
                "Virtual monitor mode {}x{}@{}Hz",
                mode.width,
                mode.height,
                target_fps
            );
        }
        if let Ok(mut status) = runtime_status.write() {
            status.display_name = Some(primary.name.clone());
            status.captured_virtual_display = Some(target_display.is_some() || owns_virtual_monitor);
        }
    
        // Initialize input using the selected output's virtual-desktop rectangle.
        let input_sink = match flux_input::InputSink::new(primary.desktop_rect) {
            Ok(sink) => Arc::new(sink),
            Err(e) => {
                tracing::error!("Failed to initialize input sink: {}", e);
                return;
            }
        };
        let loop_start = stream_epoch;
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

        let starter = CaptureStarter {
            capture: capture.as_ref(),
            cursor_sink: &cursor_sink,
            input_sink: &input_sink,
            clipboard: &clipboard,
            target_fps,
        };
        let mut session = match starter.start(&primary, primary.native_resolution) {
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
        let mut encoder_retry = EncoderRetry::default();
        let mut quality_level = default_quality_level.clamp(1, 10);
        let configured_fps = target_fps;
        let default_fps_cap = effective_default_fps_cap(configured_fps, default_fps_cap);
        let mut fps_cap = default_fps_cap;
        let mut frame_pacer = FramePacer::default();
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
        let mut stage_timings = StageTimings::default();
        let mut resolution_status_tracker = ResolutionStatusTracker::default();
        #[cfg(not(target_os = "windows"))]
        let mut pending_mode: Option<PendingMode> = None;
        loop {
            if let Ok(request) = resolution_rx.try_recv() {
                resolution_status_tracker.on_transition_started(request.width, request.height);
                #[cfg(target_os = "windows")]
                {
                    let current_mode = ModeRequest {
                        width: primary.native_resolution.width,
                        height: primary.native_resolution.height,
                    };
                    match (validate_mode_request(request), virtual_display.as_mut()) {
                        (Err(error), _) => tracing::warn!(
                            "Ignoring invalid resolution request {}x{}: {}",
                            request.width,
                            request.height,
                            error
                        ),
                        (Ok(request), None) => {
                            tracing::warn!(
                                "Ignoring resolution request {}x{} because no virtual display is active",
                                request.width,
                                request.height
                            );
                        }
                        (Ok(request), Some(_)) if !mode_change_needed(current_mode, request) => {
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
                                    encode_width: None,
                                    encode_height: None,
                                },
                            );
                        }
                        (Ok(request), Some(display)) => {
                            tracing::info!(
                                "Resolution transition: requested {}x{}, current {}x{}",
                                request.width,
                                request.height,
                                current_mode.width,
                                current_mode.height
                            );
                            encode_session = None;
                            capture_resolution = flux_core::types::Resolution::new(0, 0);
                            encode_resolution = flux_core::types::Resolution::new(0, 0);
                            release_capture_session(session);
                            crate::metrics::capture_restart();
                            (primary, session) = transition_resolution(
                                &starter,
                                display,
                                request,
                                current_mode,
                                &resolution_status_tx,
                                &mut resolution_status_tracker,
                            );
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let publish = |state: &'static str, request: ModeRequest, error: Option<String>| {
                        publish_resolution_status(
                            &resolution_status_tx,
                            ResolutionStatus {
                                state,
                                width: request.width,
                                height: request.height,
                                previous_width: None,
                                previous_height: None,
                                error,
                                encode_width: None,
                                encode_height: None,
                            },
                        );
                    };
                    match validate_mode_request(request) {
                        Err(error) => {
                            tracing::warn!(
                                "Ignoring invalid resolution request {}x{}: {}",
                                request.width,
                                request.height,
                                error
                            );
                            resolution_status_tracker
                                .on_transition_failed(request.width, request.height);
                            publish("failed", request, Some(error.to_string()));
                        }
                        Ok(request) => {
                            let current_mode = ModeRequest {
                                width: primary.native_resolution.width,
                                height: primary.native_resolution.height,
                            };
                            if !mode_change_needed(current_mode, request) {
                                tracing::info!(
                                    "Resolution request {}x{} already active; no-op",
                                    request.width,
                                    request.height
                                );
                                pending_mode = None;
                                publish("succeeded", request, None);
                            } else {
                                let requested = flux_core::types::Resolution::new(request.width, request.height);
                                match session.request_mode(requested, target_fps) {
                                    Ok(()) => {
                                        tracing::info!(
                                            "Resolution request: {}x{} -> {}x{}",
                                            current_mode.width,
                                            current_mode.height,
                                            request.width,
                                            request.height
                                        );
                                        pending_mode = Some(PendingMode::new(
                                            requested,
                                            primary.native_resolution,
                                            std::time::Instant::now(),
                                        ));
                                    }
                                    Err(error) => {
                                        tracing::warn!(
                                            "Resolution request {}x{} failed: {}",
                                            request.width,
                                            request.height,
                                            error
                                        );
                                        resolution_status_tracker
                                            .on_transition_failed(request.width, request.height);
                                        publish("failed", request, Some(error.to_string()));
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Check for IDR requests
            if let Ok(_) = idr_rx.try_recv() {
                crate::metrics::idr_request();
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
                    if let Err(error) = enc.set_framerate(fps_cap) {
                        tracing::warn!("runtime framerate update to {} failed: {}", fps_cap, error);
                    }
                    if let Err(error) = enc.set_bitrate(kbps) {
                        tracing::warn!("quality level {} bitrate update failed: {}", quality_level, error);
                    } else {
                        crate::metrics::encoder_target(kbps);
                        if let Ok(mut status) = runtime_status.write() {
                            status.target_bitrate_kbps = kbps;
                            if let Some(notify) = status.registration_notify.as_ref() {
                                let _ = notify.send(());
                            }
                        }
                    }
                }
            }
            if let (Some(kbps), Some(ref mut enc)) = (new_bitrate, encode_session.as_mut()) {
                match enc.set_bitrate(kbps) {
                    Ok(()) => {
                        crate::metrics::encoder_target(kbps);
                        tracing::info!("Encoder bitrate set to {} kbps", kbps);
                    }
                    Err(e) => tracing::warn!("set_bitrate({} kbps) failed: {}", kbps, e),
                }
            }

            let t0 = std::time::Instant::now();

            let frame = match session.next_frame_timeout(std::time::Duration::from_millis(100)) {
                Ok(Some(f)) => f,
                Ok(None) => {
                    // No frame yet (idle desktop): service requests and deadlines.
                    #[cfg(not(target_os = "windows"))]
                    settle_pending_mode(
                        &mut pending_mode,
                        None,
                        &mut primary,
                        &input_sink,
                        &resolution_status_tx,
                        &mut resolution_status_tracker,
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!("Capture error: {}", e);
                    if !matches!(e, FluxError::CaptureSessionLost(_)) {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                        continue;
                    }
                    encode_session = None;
                    capture_resolution = flux_core::types::Resolution::new(0, 0);
                    encode_resolution = flux_core::types::Resolution::new(0, 0);
                    release_capture_session(session);
                    crate::metrics::capture_restart();
                    (primary, session) = recover_lost_capture(&starter, &primary);
                    #[cfg(not(target_os = "windows"))]
                    {
                        pending_mode = None;
                    }
                    continue;
                }
            };
            crate::metrics::capture_frame();
            #[cfg(not(target_os = "windows"))]
            settle_pending_mode(
                &mut pending_mode,
                Some(frame.resolution),
                &mut primary,
                &input_sink,
                &resolution_status_tx,
                &mut resolution_status_tracker,
            );

            let t_capture = t0.elapsed();
            crate::metrics::capture_seconds(t_capture.as_secs_f64());
            if !should_encode_frame(&mut frame_pacer, t0, fps_cap) {
                session.recycle_frame(frame);
                continue;
            }
            frame_count += 1;

            // (Re)build the encoder once the negotiated capture resolution is
            // known or whenever it changes (e.g. display rotation / mode switch),
            // so the encoder dimensions always match the frames it receives.
            let rebuild_now = std::time::Instant::now();
            if capture_resolution != frame.resolution || (encode_session.is_none() && encoder_retry.due(rebuild_now)) {
                encode_session = None;
                let (sess, backend) = build_encode_session_for(
                    fps_cap,
                    frame.resolution,
                    forced_backend,
                    quality_level,
                    session.gpu_device(),
                );
                crate::metrics::set_host_info(&format!("{backend:?}"), &gpu_vendor, std::env::consts::OS);
                capture_resolution = frame.resolution;
                match sess {
                    Some((s, res)) => {
                        encode_session = Some(s);
                        encode_resolution = res;
                        encoder_retry.on_success();
                    }
                    None => {
                        encode_resolution = frame.resolution;
                        let retry_delay = encoder_retry.on_failure(std::time::Instant::now());
                        tracing::warn!("Encoder rebuild failed; retrying in {}s", retry_delay.as_secs());
                        let status = resolution_status_tracker.on_encoder_failed(
                            capture_resolution.width,
                            capture_resolution.height,
                            retry_delay,
                        );
                        publish_resolution_status(&resolution_status_tx, status);
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
                set_cursor_dimensions(&cursor_dimensions, capture_resolution, encode_resolution);
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
                    encode_session = None;
                    capture_resolution = flux_core::types::Resolution::new(0, 0);
                    release_capture_session(session);
                    match restart_for_downscale(&starter, &primary, encode_resolution) {
                        Ok(DownscaleRestart::Gpu(restarted)) => {
                            crate::metrics::capture_restart();
                            session = restarted;
                            set_cursor_dimensions(&cursor_dimensions, encode_resolution, encode_resolution);
                            // The encoder was released with the old capture
                            // session; rebuild it from the restarted session's
                            // device on the next frame.
                            tracing::info!("Capture restarted with GPU downscale to {}", encode_resolution);
                            continue;
                        }
                        Ok(DownscaleRestart::Cpu(restored)) => {
                            crate::metrics::capture_restart();
                            session = restored;
                            set_cursor_dimensions(&cursor_dimensions, primary.native_resolution, encode_resolution);
                        }
                        Err(e) => {
                            tracing::error!("Failed to restore capture session: {}", e);
                            return;
                        }
                    }
                }
            }

            // CPU fallback when GPU-side scaling isn't available: downscale the
            // frame before encoding when the encoder session is smaller.
            let frame = if frame.resolution != encode_resolution {
                let scaled = flux_encode::scale::downscale_frame(&frame, encode_resolution);
                session.recycle_frame(frame);
                match scaled {
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
            let mut keyframe_bytes = None;
            let frame = Arc::new(frame);
            if let Some(ref mut enc) = encode_session {
                match enc.encode(Arc::clone(&frame)) {
                    Ok(packets) => {
                        for pkt in packets {
                            total_encoded_bytes += pkt.data.len() as u64;
                            crate::metrics::encoded_frame(pkt.is_keyframe, pkt.data.len());
                            if pkt.is_keyframe {
                                keyframe_bytes = Some(pkt.data.len());
                            }

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

                            let _ = h264_tx.send(Arc::new((ts_micros, pkt.data)));
                            if let Some(status) = resolution_status_tracker
                                .on_packet_emitted(encode_resolution.width, encode_resolution.height)
                            {
                                publish_resolution_status(&resolution_status_tx, status);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Encode error on frame {}: {}", frame_count, e);
                    }
                }
            }
            let t_encode = t1.elapsed();
            // Encoders release their clone before returning, so the buffer is
            // normally free for reuse; otherwise it is simply dropped.
            if let Ok(frame) = Arc::try_unwrap(frame) {
                session.recycle_frame(frame);
            }
            stage_timings.observe(t_capture, t_encode, keyframe_bytes);

            // Periodic performance stats (every 5 seconds)
            if frame_count % (target_fps as u64 * 5) == 0 {
                let wall = loop_start.elapsed().as_secs_f64();
                let actual_fps = frame_count as f64 / wall;
                let avg_kbps = if frame_count > 0 {
                    (total_encoded_bytes as f64 * 8.0 / wall / 1000.0) as u64
                } else { 0 };
                let stages = stage_timings.summary().unwrap_or_default();
                stage_timings = StageTimings::default();
                tracing::info!(
                    "Perf: {:.1} fps | {} | {} frames, ~{} kbps",
                    actual_fps,
                    stages,
                    frame_count,
                    avg_kbps,
                );
            }
        }
    }
}


#[cfg(test)]
mod pacing_tests {
    use super::*;

    #[test]
    fn pacing_accepts_native_interval_and_small_jitter() {
        let start = std::time::Instant::now();
        let interval = std::time::Duration::from_secs_f64(1.0 / 48.0);
        let mut pacer = FramePacer::default();
        assert!(should_encode_frame(&mut pacer, start, 48));
        assert!(should_encode_frame(&mut pacer, start + interval, 48));
        assert!(should_encode_frame(
            &mut pacer,
            start + interval + interval.mul_f64(0.95),
            48
        ));
    }

    fn accepted_count(input_fps: u32, output_fps: u32, frames: usize) -> usize {
        let start = std::time::Instant::now();
        let interval = std::time::Duration::from_secs_f64(1.0 / input_fps as f64);
        let mut pacer = FramePacer::default();
        (0..frames)
            .filter(|index| {
                should_encode_frame(
                    &mut pacer,
                    start + interval.mul_f64(*index as f64),
                    output_fps,
                )
            })
            .count()
    }

    #[test]
    fn pacing_targets_48_fps_from_60_fps_capture() {
        assert_eq!(accepted_count(60, 48, 600), 480);
    }

    #[test]
    fn pacing_targets_30_fps_from_60_fps_capture() {
        assert_eq!(accepted_count(60, 30, 600), 300);
    }

    #[test]
    fn pacing_does_not_burst_after_capture_stall() {
        let start = std::time::Instant::now();
        let mut pacer = FramePacer::default();
        assert!(should_encode_frame(&mut pacer, start, 48));
        assert!(should_encode_frame(
            &mut pacer,
            start + std::time::Duration::from_secs(1),
            48
        ));
        assert!(!should_encode_frame(
            &mut pacer,
            start + std::time::Duration::from_secs(1) + std::time::Duration::from_millis(1),
            48
        ));
    }
}

#[cfg(test)]
mod stage_timing_tests {
    use super::*;

    fn ms(millis: u64) -> std::time::Duration {
        std::time::Duration::from_millis(millis)
    }

    #[test]
    fn reports_worst_case_alongside_average() {
        let mut timings = StageTimings::default();
        timings.observe(ms(2), ms(4), None);
        timings.observe(ms(8), ms(40), Some(700_000));

        assert_eq!(
            timings.summary().unwrap(),
            "capture avg=5.0ms max=8.0ms | encode avg=22.0ms max=40.0ms \
             | keyframes n=1 max encode=40.0ms max=700000 bytes"
        );
    }

    #[test]
    fn idle_window_has_no_summary() {
        assert!(StageTimings::default().summary().is_none());
    }
}

#[cfg(test)]
mod capture_restart_tests {
    use super::*;
    use flux_capture::traits::{CaptureSession, CursorUpdateSink, DisplayInfo, ScreenCapture};
    use flux_core::frame::CapturedFrame;
    use flux_core::types::{DesktopRect, Resolution};
    use std::sync::Mutex;

    struct FakeSession;

    impl CaptureSession for FakeSession {
        fn next_frame(&mut self) -> flux_core::Result<CapturedFrame> {
            Err(FluxError::Capture("fake".into()))
        }

        fn try_next_frame(&mut self) -> flux_core::Result<Option<CapturedFrame>> {
            Ok(None)
        }

        fn stop(&mut self) -> flux_core::Result<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct FakeCapture {
        displays: Vec<DisplayInfo>,
        failing_resolutions: Vec<Resolution>,
        starts: Mutex<Vec<(Option<u32>, Resolution)>>,
    }

    impl ScreenCapture for FakeCapture {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn enumerate_displays(&self) -> flux_core::Result<Vec<DisplayInfo>> {
            Ok(self.displays.clone())
        }

        fn start_capture(
            &self,
            display_id: Option<u32>,
            resolution: Resolution,
            _framerate: u32,
            _cursor_sink: Option<CursorUpdateSink>,
        ) -> flux_core::Result<Box<dyn CaptureSession>> {
            self.starts.lock().unwrap().push((display_id, resolution));
            if self.failing_resolutions.contains(&resolution) {
                return Err(FluxError::Capture("unsupported resolution".into()));
            }
            Ok(Box::new(FakeSession))
        }
    }

    fn display(id: u32, name: &str, width: u32, height: u32) -> DisplayInfo {
        DisplayInfo {
            id,
            adapter_luid: Some(7),
            name: name.into(),
            native_resolution: Resolution::new(width, height),
            desktop_rect: DesktopRect {
                left: 0,
                top: 0,
                width,
                height,
            },
            primary: false,
            capture_supported: true,
        }
    }

    fn with_starter(capture: &FakeCapture, f: impl FnOnce(&CaptureStarter<'_>)) {
        let cursor_sink: CursorUpdateSink = Arc::new(|_| {});
        let input_sink = flux_input::InputSink::new(DesktopRect {
            left: 0,
            top: 0,
            width: 1,
            height: 1,
        })
        .expect("input sink");
        let clipboard = ClipboardCtx {
            sink: flux_capture::traits::ClipboardSink::default(),
            events: tokio::sync::broadcast::channel::<Arc<(u64, Vec<u8>)>>(4).0,
            epoch: Instant::now(),
            enabled: true,
            max_bytes: 1024 * 1024,
        };
        f(&CaptureStarter {
            capture,
            cursor_sink: &cursor_sink,
            input_sink: &input_sink,
            clipboard: &clipboard,
            target_fps: 60,
        });
    }

    #[test]
    fn downscale_restart_prefers_gpu_scaled_capture() {
        let capture = FakeCapture::default();
        let target = display(3, "out", 5120, 2160);
        let encode = Resolution::new(4096, 1728);
        with_starter(&capture, |starter| {
            let restart = restart_for_downscale(starter, &target, encode).expect("restart");
            assert!(matches!(restart, DownscaleRestart::Gpu(_)));
        });
        assert_eq!(*capture.starts.lock().unwrap(), vec![(Some(3), encode)]);
    }

    #[test]
    fn downscale_restart_falls_back_to_native_capture() {
        let encode = Resolution::new(4096, 1728);
        let capture = FakeCapture {
            failing_resolutions: vec![encode],
            ..Default::default()
        };
        let target = display(3, "out", 5120, 2160);
        with_starter(&capture, |starter| {
            let restart = restart_for_downscale(starter, &target, encode).expect("restart");
            assert!(matches!(restart, DownscaleRestart::Cpu(_)));
        });
        assert_eq!(
            *capture.starts.lock().unwrap(),
            vec![(Some(3), encode), (Some(3), target.native_resolution)]
        );
    }

    #[test]
    fn downscale_restart_reports_failed_native_restore() {
        let encode = Resolution::new(4096, 1728);
        let target = display(3, "out", 5120, 2160);
        let capture = FakeCapture {
            failing_resolutions: vec![encode, target.native_resolution],
            ..Default::default()
        };
        with_starter(&capture, |starter| {
            assert!(restart_for_downscale(starter, &target, encode).is_err());
        });
    }

    #[test]
    fn lost_capture_recovers_on_the_same_display_only() {
        let lost = display(1, "virtual", 1920, 1080);
        let capture = FakeCapture {
            displays: vec![display(5, "other", 2560, 1440), display(9, "virtual", 1280, 720)],
            ..Default::default()
        };
        with_starter(&capture, |starter| {
            let (refreshed, _session) = recover_lost_capture(starter, &lost);
            assert_eq!(refreshed.id, 9);
            assert_eq!(refreshed.native_resolution, Resolution::new(1280, 720));
        });
        assert_eq!(
            *capture.starts.lock().unwrap(),
            vec![(Some(9), Resolution::new(1280, 720))]
        );
    }
}


#[cfg(test)]
mod fps_cap_tests {
    use super::*;

    #[test]
    fn default_fps_cap_is_used_for_startup_and_auto() {
        assert_eq!(effective_default_fps_cap(60, 48), 48);
        assert_eq!(effective_default_fps_cap(30, 48), 30);
    }
}
