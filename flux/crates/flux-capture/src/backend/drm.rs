//! Linux KMS/DRM direct capture backend (not implemented).
//!
//! Intended as a lower-level fallback for environments without PipeWire (e.g.
//! headless servers, bare Wayland compositors) that captures straight from the
//! DRM framebuffer. Until it exists, construction fails so callers get an
//! explicit error instead of a "working" session that streams black frames.

use flux_core::error::{FluxError, Result};
use flux_core::types::Resolution;

use crate::traits::{CaptureSession, CursorUpdateSink, DisplayInfo, ScreenCapture};

const NOT_IMPLEMENTED: &str = "DRM/KMS capture not implemented";

/// DRM/KMS direct framebuffer capture backend — not implemented.
pub struct DrmCapture {
    // Will hold DRM file descriptor, connector info, etc.
    _private: (),
}

impl DrmCapture {
    pub fn new() -> Result<Self> {
        // TODO: Initialization:
        //   1. Open /dev/dri/card0 (or enumerate render nodes)
        //   2. drmModeGetResources → enumerate connectors, CRTCs, encoders
        //   3. For each active connector, record resolution and CRTC mapping
        //
        // Capture: find the primary plane for the target CRTC, drmModeGetFB2
        // → export DMA-BUF fd via drmPrimeHandleToFD into
        // `GpuFrameHandle::DmaBuf`, with vblank-synchronized timing.
        Err(FluxError::Capture(NOT_IMPLEMENTED.into()))
    }
}

impl ScreenCapture for DrmCapture {
    fn name(&self) -> &'static str {
        "DRM/KMS"
    }

    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>> {
        Err(FluxError::Capture(NOT_IMPLEMENTED.into()))
    }

    fn start_capture(
        &self,
        _display_id: Option<u32>,
        _resolution: Resolution,
        _framerate: u32,
        _cursor_sink: Option<CursorUpdateSink>,
    ) -> Result<Box<dyn CaptureSession>> {
        Err(FluxError::Capture(NOT_IMPLEMENTED.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_fails_instead_of_faking_success() {
        assert!(matches!(DrmCapture::new(), Err(FluxError::Capture(_))));
    }

    #[test]
    fn create_capture_rejects_drm() {
        let result = crate::create_capture(Some(flux_core::types::CaptureBackend::Drm));
        assert!(matches!(result, Err(FluxError::Capture(_))));
    }
}
