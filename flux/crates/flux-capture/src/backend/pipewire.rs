//! Linux PipeWire screen-cast capture backend.
//!
//! Uses the XDG Desktop Portal screen-cast API to negotiate capture, then
//! receives frames via a PipeWire stream. DMA-BUF frames are preferred for
//! zero-copy GPU encode.

use flux_core::error::{FluxError, Result};
use flux_core::frame::CapturedFrame;
use flux_core::types::{PixelFormat, Resolution};

use crate::traits::{CaptureSession, DisplayInfo, ScreenCapture};

/// PipeWire screen-cast capture backend.
pub struct PipeWireCapture {
    // Will hold PipeWire main-loop, core proxy, etc.
    _private: (),
}

impl PipeWireCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing PipeWire screen-cast capture");

        // TODO: Initialization sequence:
        //
        //   1. Connect to D-Bus session bus
        //   2. Call org.freedesktop.portal.ScreenCast.CreateSession
        //   3. SelectSources (monitor / window / virtual)
        //   4. Start → receive PipeWire node_id
        //   5. Initialize PipeWire context and connect to the node

        Ok(Self { _private: () })
    }
}

impl ScreenCapture for PipeWireCapture {
    fn name(&self) -> &'static str {
        "PipeWire Screen-Cast"
    }

    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>> {
        // TODO: Query available outputs via wl_output or org.freedesktop.portal.ScreenCast.
        tracing::debug!("Enumerating PipeWire displays (stub)");
        Ok(vec![DisplayInfo {
            id: 0,
            name: "Primary Display".into(),
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
            "Starting PipeWire capture on display {} at {}@{}fps",
            display_id,
            resolution,
            framerate
        );

        Ok(Box::new(PipeWireCaptureSession::new(
            display_id, resolution, framerate,
        )?))
    }
}

/// An active PipeWire capture session.
struct PipeWireCaptureSession {
    display_id: u32,
    resolution: Resolution,
    framerate: u32,
    frame_sequence: u64,
    running: bool,
    // TODO: Store pw_stream, spa buffers, etc.
}

impl PipeWireCaptureSession {
    fn new(display_id: u32, resolution: Resolution, framerate: u32) -> Result<Self> {
        // TODO: Full initialization:
        //
        //   1. pw_stream_new(core, "flux-capture", props)
        //   2. Set stream params requesting DMA-BUF + SPA_VIDEO_FORMAT_BGRx
        //   3. pw_stream_connect(PW_DIRECTION_INPUT, node_id, PW_STREAM_FLAG_AUTOCONNECT)
        //   4. Register on_process callback to receive buffers
        //   5. Start the PipeWire main loop on a dedicated thread

        Ok(Self {
            display_id,
            resolution,
            framerate,
            frame_sequence: 0,
            running: true,
        })
    }

    fn acquire_frame(&mut self) -> Result<CapturedFrame> {
        // TODO: Real implementation:
        //
        //   1. Dequeue buffer from pw_stream
        //   2. If buffer has SPA_DATA_DmaBuf:
        //      - Extract fd, offset, stride, modifier from spa_data
        //      - Return CapturedFrame with GpuFrameHandle::DmaBuf
        //   3. If buffer has SPA_DATA_MemPtr (fallback):
        //      - memcpy the pixel data
        //      - Return CapturedFrame with data vec
        //   4. pw_stream_queue_buffer to return the buffer

        self.frame_sequence += 1;

        // Until the real PipeWire stream is wired up (see `bridge` module and
        // the `PipewireFrameSource` trait), return an empty CPU frame rather
        // than fabricating an invalid DMA-BUF fd.
        Ok(CapturedFrame {
            sequence: self.frame_sequence,
            timestamp: std::time::Instant::now(),
            format: PixelFormat::Bgra8,
            resolution: self.resolution,
            stride: self.resolution.width * 4,
            data: Vec::new(),
            gpu_handle: None,
        })
    }
}

impl CaptureSession for PipeWireCaptureSession {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        if !self.running {
            return Err(FluxError::Capture("session stopped".into()));
        }
        self.acquire_frame()
    }

    fn try_next_frame(&mut self) -> Result<Option<CapturedFrame>> {
        if !self.running {
            return Ok(None);
        }
        Ok(Some(self.acquire_frame()?))
    }

    fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping PipeWire capture session");
        self.running = false;
        // TODO: pw_stream_disconnect, destroy main loop, close DMA-BUF fds.
        Ok(())
    }
}
