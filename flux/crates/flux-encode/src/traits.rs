use flux_core::error::Result;
use flux_core::frame::{CapturedFrame, EncodedPacket, GpuDeviceHandle};
use flux_core::types::{ChromaSampling, DynamicRange, RateControlMode, Resolution, VideoCodec};

/// Configuration for creating an encoding session.
#[derive(Debug, Clone)]
pub struct EncodeConfig {
    pub codec: VideoCodec,
    pub resolution: Resolution,
    pub framerate: u32,
    pub bitrate_kbps: u32,
    pub rate_control: RateControlMode,
    pub dynamic_range: DynamicRange,
    pub chroma_sampling: ChromaSampling,
    pub gop_size: u32,
    pub b_frames: u32,
    pub max_ref_frames: u32,
}

impl Default for EncodeConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::H265,
            resolution: Resolution::new(1920, 1080),
            framerate: 60,
            bitrate_kbps: 20_000,
            rate_control: RateControlMode::Cbr,
            dynamic_range: DynamicRange::Sdr,
            chroma_sampling: ChromaSampling::Yuv420,
            gop_size: 0, // Infinite GOP — IDR frames requested explicitly
            b_frames: 0, // No B-frames for low latency
            max_ref_frames: 1,
        }
    }
}

/// Capabilities reported by an encoder backend.
#[derive(Debug, Clone)]
pub struct EncoderCapabilities {
    pub name: &'static str,
    pub supported_codecs: Vec<VideoCodec>,
    pub supports_hdr: bool,
    pub supports_yuv444: bool,
    pub max_resolution: Resolution,
    pub max_framerate: u32,
}

/// A video encoder backend (NVENC, AMF, VA-API, Vulkan Video, software).
pub trait VideoEncoder: Send + Sync {
    /// Human-readable backend name.
    fn name(&self) -> &'static str;

    /// Query the capabilities of this encoder.
    fn capabilities(&self) -> Result<EncoderCapabilities>;

    /// Validate whether the given configuration is supported.
    fn validate_config(&self, config: &EncodeConfig) -> Result<()>;

    /// Create an encoding session with the given configuration.
    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>>;

    /// Create an encoding session using the capture device when available.
    ///
    /// Backends that support device sharing may override this method. Other
    /// backends retain their existing device-selection behavior.
    fn create_session_with_device(
        &self,
        config: EncodeConfig,
        _capture_device: Option<GpuDeviceHandle>,
    ) -> Result<Box<dyn EncodeSession>> {
        self.create_session(config)
    }
}

/// An active encoding session that converts captured frames into compressed packets.
pub trait EncodeSession: Send {
    /// Submit a captured frame for encoding.
    ///
    /// Returns one or more encoded packets (there may be zero if the encoder
    /// is buffering, or multiple in case of B-frame reordering).
    fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>>;

    /// Request that the next encoded frame be an IDR / keyframe.
    fn request_idr(&mut self);

    /// Request an IDR because an encoder session was rebuilt.
    ///
    /// Backends that do not distinguish this diagnostic cause can use the
    /// ordinary request path.
    fn request_idr_for_rebuild(&mut self) {
        self.request_idr();
    }

    /// Flush the encoder — returns any remaining buffered packets.
    fn flush(&mut self) -> Result<Vec<EncodedPacket>>;

    /// Dynamically update the target bitrate (kbps).
    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()>;

    /// Dynamically update the configured frame rate without rebuilding the
    /// session. Backends that do not expose runtime frame-rate control may
    /// leave this as a no-op.
    fn set_framerate(&mut self, _framerate: u32) -> Result<()> {
        Ok(())
    }
}
