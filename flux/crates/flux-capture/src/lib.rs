pub mod backend;
pub mod bridge;
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
#[cfg(all(target_os = "linux", feature = "capture-pipewire"))]
pub use pipewire_source::PipewireStreamSource;
#[cfg(all(target_os = "linux", feature = "capture-pipewire"))]
pub use portal::XdgPortalSession;
#[cfg(unix)]
pub use session::{
    BufferKind, CursorMode, FormatPrefs, NegotiatedFormat, PipewireFrameSource, PortalGrant, PortalOptions,
    PortalSession, PortalStream, SourceKind,
};
pub use traits::{CaptureSession, ScreenCapture};

use flux_core::error::Result;
use flux_core::types::CaptureBackend;

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
