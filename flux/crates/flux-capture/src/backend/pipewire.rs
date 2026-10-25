//! Linux PipeWire screen-cast capture backend.
//!
//! Uses the XDG Desktop Portal screen-cast API to negotiate capture, then
//! receives frames via a PipeWire stream. DMA-BUF frames are preferred for
//! zero-copy GPU encode.

use flux_core::error::{FluxError, Result};
#[cfg(feature = "capture-pipewire")]
use flux_core::frame::CapturedFrame;
use flux_core::types::Resolution;

use crate::traits::{CaptureSession, CursorUpdateSink, DisplayInfo, ScreenCapture};

/// Placeholder display id returned by [`PipeWireCapture::enumerate_displays`]
/// until it can report real per-output PipeWire node ids. It is *not* a real
/// node id (PipeWire node 0 is the core object), so the portal capture path
/// treats it as "no specific monitor" (i.e. primary).
const STUB_DISPLAY_ID: u32 = 0;

/// Name of the display reported when capture owns a Mutter virtual monitor.
pub const VIRTUAL_MONITOR_NAME: &str = "Rustcast virtual monitor";

#[cfg(not(feature = "capture-pipewire"))]
const NOT_BUILT: &str = "PipeWire capture not available: built without feature 'capture-pipewire'";

/// PipeWire screen-cast capture backend.
pub struct PipeWireCapture {
    // Will hold PipeWire main-loop, core proxy, etc.
    _private: (),
}

impl PipeWireCapture {
    /// Fails without the `capture-pipewire` feature: there is no stream to
    /// read frames from, so a "working" backend would only stream black.
    pub fn new() -> Result<Self> {
        #[cfg(not(feature = "capture-pipewire"))]
        {
            Err(FluxError::Capture(NOT_BUILT.into()))
        }

        #[cfg(feature = "capture-pipewire")]
        {
            tracing::info!("Initializing PipeWire screen-cast capture");
            Ok(Self { _private: () })
        }
    }
}

impl ScreenCapture for PipeWireCapture {
    fn name(&self) -> &'static str {
        "PipeWire Screen-Cast"
    }

    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>> {
        #[cfg(feature = "capture-mutter")]
        if real::use_mutter() {
            if crate::mutter::MutterSource::from_env() == crate::mutter::MutterSource::Virtual {
                // No monitor exists until the virtual stream is created; the
                // caller supplies the mode it wants captured.
                return Ok(vec![DisplayInfo {
                    id: STUB_DISPLAY_ID,
                    adapter_luid: None,
                    name: VIRTUAL_MONITOR_NAME.into(),
                    native_resolution: Resolution::new(1920, 1080),
                    desktop_rect: flux_core::types::DesktopRect {
                        left: 0,
                        top: 0,
                        width: 1920,
                        height: 1080,
                    },
                    primary: true,
                    capture_supported: true,
                }]);
            }
            match crate::mutter::query_primary_monitor() {
                Ok(monitor) => {
                    return Ok(vec![DisplayInfo {
                        id: STUB_DISPLAY_ID,
                        adapter_luid: None,
                        name: monitor.connector,
                        native_resolution: monitor.resolution,
                        desktop_rect: monitor.rect,
                        primary: true,
                        capture_supported: true,
                    }]);
                }
                Err(e) => tracing::warn!("Mutter display query failed ({e}); using stub display"),
            }
        }

        // TODO: Query available outputs via wl_output or org.freedesktop.portal.ScreenCast.
        tracing::debug!("Enumerating PipeWire displays (stub)");
        Ok(vec![DisplayInfo {
            id: STUB_DISPLAY_ID,
            adapter_luid: None,
            name: "Primary Display".into(),
            native_resolution: Resolution::new(1920, 1080),
            desktop_rect: flux_core::types::DesktopRect {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
            primary: true,
            capture_supported: true,
        }])
    }

    fn start_capture(
        &self,
        display_id: Option<u32>,
        resolution: Resolution,
        framerate: u32,
        cursor_sink: Option<CursorUpdateSink>,
    ) -> Result<Box<dyn CaptureSession>> {
        #[cfg(not(feature = "capture-mutter"))]
        let _ = &cursor_sink;
        tracing::info!(
            "Starting PipeWire capture on display {:?} at {}@{}fps",
            display_id,
            resolution,
            framerate
        );

        #[cfg(feature = "capture-pipewire")]
        {
            // `enumerate_displays` currently reports a single placeholder id
            // (not a real PipeWire node id), so callers that forward it — e.g.
            // the host picking `primary.id` — would otherwise ask
            // `select_stream` to match a node that doesn't exist. Treat the
            // placeholder as "primary" until real per-output node ids land.
            let requested = display_id.filter(|&id| id != STUB_DISPLAY_ID);
            #[cfg(feature = "capture-mutter")]
            if real::use_mutter() {
                return real::start_mutter_capture(resolution, framerate, cursor_sink);
            }
            real::start_portal_capture(requested, resolution, framerate)
        }

        #[cfg(not(feature = "capture-pipewire"))]
        {
            Err(FluxError::Capture(NOT_BUILT.into()))
        }
    }
}

/// Real portal-negotiated PipeWire capture, behind the `capture-pipewire`
/// feature so default/CI builds (no portal, no PipeWire daemon) keep working.
#[cfg(feature = "capture-pipewire")]
mod real {
    use super::*;
    use crate::pipewire_source::PipewireStreamSource;
    use crate::portal::XdgPortalSession;
    use crate::session::{
        select_stream, FormatPrefs, FrameSourceSession, PipewireFrameSource, PortalOptions, PortalSession,
    };
    use std::time::Duration;

    /// Negotiate an `xdg-desktop-portal` ScreenCast+RemoteDesktop session and
    /// connect a live PipeWire stream to the selected granted node.
    ///
    /// `display_id`, when set, is the PipeWire `node_id` of the granted stream
    /// to capture (multi-monitor); `None` selects the primary monitor (the
    /// stream at the virtual-desktop origin) via [`select_stream`]. When the
    /// caller wants more than one monitor offered in the dialog, the portal is
    /// asked to allow multiple sources.
    ///
    /// The portal negotiation is async (zbus) and prompts the user for
    /// consent, so it is driven on a dedicated thread that owns a Tokio
    /// runtime; that runtime is then kept alive inside the returned session so
    /// the portal's D-Bus connection (and therefore the PipeWire stream) stays
    /// open for the capture's lifetime.
    pub(super) fn start_portal_capture(
        display_id: Option<u32>,
        resolution: Resolution,
        framerate: u32,
    ) -> Result<Box<dyn CaptureSession>> {
        let prefs = FormatPrefs {
            resolution,
            framerate,
            ..Default::default()
        };

        let negotiate = std::thread::spawn(
            move || -> Result<(tokio::runtime::Runtime, XdgPortalSession, u32, std::os::fd::RawFd)> {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .worker_threads(1)
                    .build()
                    .map_err(|e| FluxError::Capture(format!("failed to build portal runtime: {e}")))?;
                let (portal, node_id, fd) = runtime.block_on(async {
                    let opts = PortalOptions {
                        // Offer multiple sources when a specific monitor was
                        // requested, so the grant can contain it.
                        multiple: display_id.is_some(),
                        ..Default::default()
                    };
                    let mut portal = XdgPortalSession::new();
                    let grant = portal.negotiate(opts).await?;
                    let node_id = select_stream(&grant.streams, display_id)?.node_id;
                    Ok::<_, FluxError>((portal, node_id, grant.pipewire_fd))
                })?;
                Ok((runtime, portal, node_id, fd))
            },
        );

        let (runtime, portal, node_id, fd) = negotiate
            .join()
            .map_err(|_| FluxError::Capture("portal negotiation thread panicked".into()))??;

        // `connect` dups the fd, so the stream owns its descriptor; we still
        // keep `portal` (and its runtime) alive to hold the D-Bus session.
        let mut source = PipewireStreamSource::new();
        source.connect(fd, node_id, prefs)?;

        Ok(Box::new(PortalPipewireSession {
            inner: FrameSourceSession::new(source, Duration::from_millis(100)),
            _portal: portal,
            _runtime: runtime,
        }))
    }

    /// Whether capture should go through Mutter's direct D-Bus API:
    /// `FLUX_CAPTURE_API=mutter` forces it, `portal` disables it, and when unset
    /// it is used if Mutter's RemoteDesktop service is on the session bus.
    #[cfg(feature = "capture-mutter")]
    pub(super) fn use_mutter() -> bool {
        match std::env::var("FLUX_CAPTURE_API").ok().as_deref() {
            Some("mutter") => true,
            Some("portal") => false,
            _ => crate::mutter::is_available(),
        }
    }

    /// Capture through a direct Mutter ScreenCast + RemoteDesktop session
    /// (a virtual monitor sized `resolution`, or the primary monitor when
    /// `FLUX_MUTTER_SOURCE=monitor`), reading the stream from the local
    /// PipeWire daemon. The pointer is delivered as cursor metadata to
    /// `cursor_sink`.
    #[cfg(feature = "capture-mutter")]
    pub(super) fn start_mutter_capture(
        resolution: Resolution,
        framerate: u32,
        cursor_sink: Option<CursorUpdateSink>,
    ) -> Result<Box<dyn CaptureSession>> {
        use crate::mutter::{MutterSession, MutterSource};
        use crate::session::CursorMode;

        let mutter_source = MutterSource::from_env();
        let prefs = FormatPrefs {
            resolution,
            framerate,
            exact_size: mutter_source == MutterSource::Virtual,
            ..Default::default()
        };
        let mutter = MutterSession::start(mutter_source, CursorMode::Metadata, resolution)?;
        let mut source = PipewireStreamSource::new();
        source.set_cursor_sink(cursor_sink);
        source.connect_local(mutter.node_id(), prefs)?;
        Ok(Box::new(MutterPipewireSession {
            inner: FrameSourceSession::new(source, Duration::from_millis(100)),
            mutter,
        }))
    }

    /// Map "Mutter closed the session" to the error the host uses to recreate capture.
    #[cfg(feature = "capture-mutter")]
    pub(super) fn session_lost_if_closed(closed: bool) -> Result<()> {
        if closed {
            Err(FluxError::CaptureSessionLost(
                "Mutter closed the ScreenCast/RemoteDesktop session".into(),
            ))
        } else {
            Ok(())
        }
    }

    #[cfg(all(test, feature = "capture-mutter"))]
    mod mutter_tests {
        use super::*;

        #[test]
        fn closed_session_maps_to_capture_session_lost() {
            assert!(session_lost_if_closed(false).is_ok());
            assert!(matches!(
                session_lost_if_closed(true),
                Err(FluxError::CaptureSessionLost(_))
            ));
        }
    }

    /// A [`CaptureSession`] backed by a direct Mutter session; also exposes the
    /// session's RemoteDesktop input.
    #[cfg(feature = "capture-mutter")]
    pub(super) struct MutterPipewireSession {
        inner: FrameSourceSession<PipewireStreamSource>,
        mutter: crate::mutter::MutterSession,
    }

    #[cfg(feature = "capture-mutter")]
    impl MutterPipewireSession {
        fn sync_stream_size(&self, frame: &CapturedFrame) {
            self.mutter.set_stream_size(frame.resolution);
        }
    }

    #[cfg(feature = "capture-mutter")]
    impl CaptureSession for MutterPipewireSession {
        fn next_frame(&mut self) -> Result<CapturedFrame> {
            loop {
                session_lost_if_closed(self.mutter.is_closed())?;
                if let Some(frame) = self.inner.recv_timeout(Duration::from_millis(100))? {
                    self.sync_stream_size(&frame);
                    return Ok(frame);
                }
            }
        }

        fn try_next_frame(&mut self) -> Result<Option<CapturedFrame>> {
            session_lost_if_closed(self.mutter.is_closed())?;
            let frame = self.inner.try_next_frame()?;
            if let Some(frame) = &frame {
                self.sync_stream_size(frame);
            }
            Ok(frame)
        }

        fn next_frame_timeout(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
            session_lost_if_closed(self.mutter.is_closed())?;
            let frame = self.inner.recv_timeout(timeout)?;
            if let Some(frame) = &frame {
                self.sync_stream_size(frame);
            }
            Ok(frame)
        }

        fn request_mode(&mut self, resolution: Resolution, refresh_hz: u32) -> Result<()> {
            if self.mutter.source() != crate::mutter::MutterSource::Virtual {
                return Err(FluxError::Capture(
                    "the captured monitor's mode is controlled by the compositor, not by capture".into(),
                ));
            }
            session_lost_if_closed(self.mutter.is_closed())?;
            self.inner.source().request_size(resolution, refresh_hz)
        }

        fn input_backend(&self) -> Option<std::sync::Arc<dyn flux_input::InputBackend>> {
            Some(self.mutter.input_backend())
        }

        fn clipboard_backend(&self) -> Option<std::sync::Arc<dyn crate::traits::ClipboardBackend>> {
            Some(self.mutter.clipboard_backend())
        }

        fn recycle_frame(&mut self, frame: CapturedFrame) {
            self.inner.recycle_frame(frame);
        }

        fn stop(&mut self) -> Result<()> {
            self.inner.stop()
        }
    }

    /// A [`CaptureSession`] that keeps the portal session (and its Tokio
    /// runtime) alive alongside the PipeWire frame source it feeds.
    pub(super) struct PortalPipewireSession {
        inner: FrameSourceSession<PipewireStreamSource>,
        _portal: XdgPortalSession,
        _runtime: tokio::runtime::Runtime,
    }

    impl CaptureSession for PortalPipewireSession {
        fn next_frame(&mut self) -> Result<CapturedFrame> {
            self.inner.next_frame()
        }

        fn try_next_frame(&mut self) -> Result<Option<CapturedFrame>> {
            self.inner.try_next_frame()
        }

        fn recycle_frame(&mut self, frame: CapturedFrame) {
            self.inner.recycle_frame(frame);
        }

        fn stop(&mut self) -> Result<()> {
            self.inner.stop()
        }
    }
}

#[cfg(all(test, not(feature = "capture-pipewire")))]
mod stub_tests {
    use super::*;

    #[test]
    fn construction_fails_instead_of_faking_success() {
        match PipeWireCapture::new() {
            Err(FluxError::Capture(msg)) => assert!(msg.contains("capture-pipewire"), "{msg}"),
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("stub PipeWire capture must not construct"),
        }
    }

    #[test]
    fn default_create_capture_fails_without_feature() {
        assert!(matches!(crate::create_capture(None), Err(FluxError::Capture(_))));
    }
}
