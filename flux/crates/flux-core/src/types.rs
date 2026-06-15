use serde::{Deserialize, Serialize};

/// Unique identifier for a streaming session.
pub type SessionId = uuid::Uuid;

/// Video codec selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

impl std::fmt::Display for VideoCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::H264 => write!(f, "H.264"),
            Self::H265 => write!(f, "H.265/HEVC"),
            Self::Av1 => write!(f, "AV1"),
        }
    }
}

/// Color depth / dynamic range mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DynamicRange {
    Sdr,
    Hdr10,
}

/// Chroma sub-sampling format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChromaSampling {
    Yuv420,
    Yuv444,
}

/// Pixel format of a captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
    Nv12,
    P010,
    I420,
}

impl PixelFormat {
    /// Bytes per pixel for packed formats; returns `None` for planar.
    pub fn bytes_per_pixel(&self) -> Option<usize> {
        match self {
            Self::Bgra8 | Self::Rgba8 => Some(4),
            Self::Nv12 | Self::P010 | Self::I420 => None,
        }
    }

    /// Whether this format requires color-space conversion before encoding.
    pub fn needs_csc(&self) -> bool {
        matches!(self, Self::Bgra8 | Self::Rgba8)
    }
}

/// Resolution expressed as (width, height).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    pub fn pixel_count(&self) -> u64 {
        self.width as u64 * self.height as u64
    }
}

impl std::fmt::Display for Resolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

/// Encoder backend / GPU vendor hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
    Unknown,
}

/// Hardware encoder backend used on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EncoderBackend {
    /// NVIDIA NVENC (Windows & Linux).
    Nvenc,
    /// AMD Advanced Media Framework (Windows).
    Amf,
    /// VA-API (Linux — AMD & Intel).
    Vaapi,
    /// Vulkan Video extensions (cross-platform, cross-vendor).
    VulkanVideo,
    /// Software fallback (libx264 / rav1e).
    Software,
}

/// Capture backend used on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptureBackend {
    /// Windows Desktop Duplication API (DXGI).
    Dxgi,
    /// Linux PipeWire screen-cast portal.
    PipeWire,
    /// Linux KMS/DRM direct capture.
    Drm,
}

/// Rate-control mode for encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RateControlMode {
    Cbr,
    Vbr,
    Cqp,
}

/// Audio codec selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AudioCodec {
    Opus,
    Pcm,
}
