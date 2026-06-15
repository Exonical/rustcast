pub mod backend;
pub mod bridge;
pub mod cursor;
// The Wayland portal / PipeWire interfaces are fd-based and only compile on
// unix; Windows uses the DXGI backend instead.
#[cfg(all(unix, any(test, feature = "mock")))]
pub mod mock;
#[cfg(unix)]
pub mod session;
pub mod traits;

// Real Linux backends: xdg-desktop-portal negotiation (ashpd) and the live
// PipeWire stream. Gated on Linux + the `capture-pipewire` feature so default
// and Windows builds don't pull in the portal/PipeWire system dependencies.
#[cfg(all(target_os = "linux", feature = "capture-pipewire"))]
pub mod pipewire_source;
#[cfg(all(target_os = "linux", feature = "capture-pipewire"))]
pub mod portal;

pub use bridge::{FrameBridge, FrameSink, FrameSource};
pub use cursor::parse_spa_meta_cursor;
#[cfg(all(target_os = "linux", feature = "capture-pipewire"))]
pub use pipewire_source::PipewireStreamSource;
#[cfg(all(target_os = "linux", feature = "capture-pipewire"))]
pub use portal::XdgPortalSession;
#[cfg(unix)]
pub use session::{
    select_stream, BufferKind, CursorMode, FormatPrefs, NegotiatedFormat, PipewireFrameSource, PortalGrant,
    PortalOptions, PortalSession, PortalStream, SourceKind,
};
pub use traits::{CaptureSession, ScreenCapture};

use flux_core::error::Result;
use flux_core::types::CaptureBackend;

/// Probe which Wayland portal interfaces are reachable on the session bus.
///
/// Builds the `ScreenCast` / `RemoteDesktop` proxies, each of which reads the
/// interface `version` property on construction and fails with
/// `PortalNotFound` when the portal is absent — so successful construction
/// means the interface is available. Runs on a throwaway current-thread
/// runtime; returns an all-false [`PortalCapabilities`] if no session bus /
/// portal is present.
#[cfg(all(target_os = "linux", feature = "capture-pipewire"))]
pub fn probe_portal_capabilities() -> flux_core::capability::PortalCapabilities {
    use ashpd::desktop::remote_desktop::RemoteDesktop;
    use ashpd::desktop::screencast::Screencast;
    use flux_core::capability::PortalCapabilities;

    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            tracing::warn!("portal probe: could not build runtime: {e}");
            return PortalCapabilities::default();
        }
    };

    runtime.block_on(async {
        PortalCapabilities {
            screencast: Screencast::new().await.is_ok(),
            remote_desktop: RemoteDesktop::new().await.is_ok(),
            // ashpd does not expose the negotiated interface version publicly.
            screencast_version: None,
            remote_desktop_version: None,
        }
    })
}

/// Create the best available capture backend for this platform.
pub fn create_capture(backend: Option<CaptureBackend>) -> Result<Box<dyn ScreenCapture>> {
    let backend = backend.unwrap_or_else(default_backend);

    tracing::info!("Initializing capture backend: {:?}", backend);

    match backend {
        #[cfg(target_os = "windows")]
        CaptureBackend::Dxgi => Ok(Box::new(backend::dxgi::DxgiCapture::new()?)),

        #[cfg(target_os = "linux")]
        CaptureBackend::PipeWire => Ok(Box::new(backend::pipewire::PipeWireCapture::new()?)),

        #[cfg(target_os = "linux")]
        CaptureBackend::Drm => Ok(Box::new(backend::drm::DrmCapture::new()?)),

        #[allow(unreachable_patterns)]
        _other => Err(flux_core::FluxError::NoCaptureBackend),
    }
}

fn default_backend() -> CaptureBackend {
    #[cfg(target_os = "windows")]
    {
        CaptureBackend::Dxgi
    }
    #[cfg(target_os = "linux")]
    {
        CaptureBackend::PipeWire
    }
}
