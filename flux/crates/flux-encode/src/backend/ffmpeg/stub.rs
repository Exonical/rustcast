//! Placeholder FFmpeg VA-API encoder used when `encoder-ffmpeg` is disabled.
//!
//! Keeps the default build free of the libav* system dependencies. Construction
//! fails so callers fall back to another backend; selecting `FfmpegVaapi`
//! without the feature is a configuration error rather than a silent no-op.

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, EncodedPacket};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

/// FFmpeg VA-API encoder (Linux) — disabled build.
pub struct FfmpegVaapiEncoder {
    _private: (),
}

impl FfmpegVaapiEncoder {
    pub fn new() -> Result<Self> {
        Err(FluxError::EncoderInit(
            "FFmpeg VA-API backend not compiled in (enable the `encoder-ffmpeg` feature)".into(),
        ))
    }
}

impl VideoEncoder for FfmpegVaapiEncoder {
    fn name(&self) -> &'static str {
        "ffmpeg-vaapi"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Err(FluxError::EncoderInit("FFmpeg VA-API backend not compiled in".into()))
    }

    fn validate_config(&self, _config: &EncodeConfig) -> Result<()> {
        Err(FluxError::EncoderInit("FFmpeg VA-API backend not compiled in".into()))
    }

    fn create_session(&self, _config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        Err(FluxError::EncoderInit("FFmpeg VA-API backend not compiled in".into()))
    }
}

/// FFmpeg Vulkan-video encoder (Linux) — disabled build.
pub struct FfmpegVulkanEncoder {
    _private: (),
}

impl FfmpegVulkanEncoder {
    pub fn new() -> Result<Self> {
        Err(FluxError::EncoderInit(
            "FFmpeg Vulkan backend not compiled in (enable the `encoder-ffmpeg` feature)".into(),
        ))
    }
}

impl VideoEncoder for FfmpegVulkanEncoder {
    fn name(&self) -> &'static str {
        "ffmpeg-vulkan"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Err(FluxError::EncoderInit("FFmpeg Vulkan backend not compiled in".into()))
    }

    fn validate_config(&self, _config: &EncodeConfig) -> Result<()> {
        Err(FluxError::EncoderInit("FFmpeg Vulkan backend not compiled in".into()))
    }

    fn create_session(&self, _config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        Err(FluxError::EncoderInit("FFmpeg Vulkan backend not compiled in".into()))
    }
}

/// FFmpeg software encoder (Linux) — disabled build.
pub struct FfmpegSoftwareEncoder {
    _private: (),
}

impl FfmpegSoftwareEncoder {
    pub fn new() -> Result<Self> {
        Err(FluxError::EncoderInit(
            "FFmpeg software backend not compiled in (enable the `encoder-ffmpeg` feature)".into(),
        ))
    }
}

impl VideoEncoder for FfmpegSoftwareEncoder {
    fn name(&self) -> &'static str {
        "ffmpeg-software"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Err(FluxError::EncoderInit("FFmpeg software backend not compiled in".into()))
    }

    fn validate_config(&self, _config: &EncodeConfig) -> Result<()> {
        Err(FluxError::EncoderInit("FFmpeg software backend not compiled in".into()))
    }

    fn create_session(&self, _config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        Err(FluxError::EncoderInit("FFmpeg software backend not compiled in".into()))
    }
}

// Silence unused warnings for the trait's frame type in the disabled build.
#[allow(dead_code)]
fn _unused(_: &CapturedFrame) -> Option<EncodedPacket> {
    None
}
