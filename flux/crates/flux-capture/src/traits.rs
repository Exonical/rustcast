use flux_core::error::Result;
use flux_core::frame::CapturedFrame;
use flux_core::types::Resolution;

/// A screen capture backend that can enumerate displays and start capture sessions.
pub trait ScreenCapture: Send + Sync {
    /// Return the name of this backend (e.g. "DXGI", "PipeWire").
    fn name(&self) -> &'static str;

    /// List available displays / outputs.
    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>>;

    /// Begin capturing a specific display (or the primary display if `display_id` is `None`).
    fn start_capture(
        &self,
        display_id: Option<u32>,
        resolution: Resolution,
        framerate: u32,
    ) -> Result<Box<dyn CaptureSession>>;
}

/// Metadata about an available display output.
#[derive(Debug, Clone)]
pub struct DisplayInfo {
    /// Opaque identifier for this display.
    pub id: u32,

    /// Human-readable name (e.g. "DELL U2723QE").
    pub name: String,

    /// Native resolution.
    pub native_resolution: Resolution,

    /// Whether this is the primary display.
    pub primary: bool,
}

/// An active capture session that produces frames.
pub trait CaptureSession: Send {
    /// Block until the next frame is available, then return it.
    fn next_frame(&mut self) -> Result<CapturedFrame>;

    /// Non-blocking try: returns `Ok(None)` if no frame is ready yet.
    fn try_next_frame(&mut self) -> Result<Option<CapturedFrame>>;

    /// Signal the capture backend to stop. The session becomes invalid after this call.
    fn stop(&mut self) -> Result<()>;
}
