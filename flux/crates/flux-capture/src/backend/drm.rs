//! Linux KMS/DRM direct capture backend.
//!
//! Captures frames directly from the DRM framebuffer. This is a lower-level
//! fallback for environments without PipeWire (e.g. headless servers, bare
//! Wayland compositors).

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, DmaBufHandle, GpuFrameHandle};
use flux_core::types::{PixelFormat, Resolution};

use crate::traits::{CaptureSession, DisplayInfo, ScreenCapture};

/// DRM/KMS direct framebuffer capture backend.
pub struct DrmCapture {
    // Will hold DRM file descriptor, connector info, etc.
    _private: (),
}

impl DrmCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing DRM/KMS capture");

        // TODO: Initialization:
        //   1. Open /dev/dri/card0 (or enumerate render nodes)
        //   2. drmModeGetResources → enumerate connectors, CRTCs, encoders
        //   3. For each active connector, record resolution and CRTC mapping

        Ok(Self { _private: () })
    }
}

impl ScreenCapture for DrmCapture {
    fn name(&self) -> &'static str {
        "DRM/KMS"
    }

    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>> {
        tracing::debug!("Enumerating DRM displays (stub)");
        Ok(vec![DisplayInfo {
            id: 0,
            name: "DRM Output 0".into(),
            native_resolution: Resolution::new(1920, 1080),
            primary: true,
        }])
    }

    fn start_capture(
        &self,
        display_id: Option<u32>,
        resolution: Resolution,
        framerate: u32,
    ) -> Result<Box<dyn CaptureSession>> {
        let display_id = display_id.unwrap_or(0);
        tracing::info!(
            "Starting DRM capture on output {} at {}@{}fps",
            display_id,
            resolution,
            framerate
        );

        Ok(Box::new(DrmCaptureSession::new(
            display_id, resolution, framerate,
        )?))
    }
}

struct DrmCaptureSession {
    display_id: u32,
    resolution: Resolution,
    framerate: u32,
    frame_sequence: u64,
    running: bool,
}

impl DrmCaptureSession {
    fn new(display_id: u32, resolution: Resolution, framerate: u32) -> Result<Self> {
        // TODO: Set up DRM plane capture:
        //   1. Find the primary plane for the target CRTC
        //   2. Use drmModeGetFB2 to get the framebuffer DMA-BUF handle
        //   3. Set up vblank-synchronized capture timing

        Ok(Self {
            display_id,
            resolution,
            framerate,
            frame_sequence: 0,
            running: true,
        })
    }
}

impl CaptureSession for DrmCaptureSession {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        if !self.running {
            return Err(FluxError::Capture("session stopped".into()));
        }
        self.frame_sequence += 1;

        // TODO: drmModeGetFB2 → export DMA-BUF fd via drmPrimeHandleToFD
        Ok(CapturedFrame {
            sequence: self.frame_sequence,
            timestamp: std::time::Instant::now(),
            format: PixelFormat::Bgra8,
            resolution: self.resolution,
            stride: self.resolution.width * 4,
            data: Vec::new(),
            gpu_handle: Some(GpuFrameHandle::DmaBuf(DmaBufHandle {
                fd: -1,
                offset: 0,
                stride: self.resolution.width * 4,
                modifier: 0,
                width: self.resolution.width,
                height: self.resolution.height,
            })),
        })
    }

    fn try_next_frame(&mut self) -> Result<Option<CapturedFrame>> {
        if !self.running {
            return Ok(None);
        }
        Ok(Some(self.next_frame()?))
    }

    fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping DRM capture session");
        self.running = false;
        Ok(())
    }
}
