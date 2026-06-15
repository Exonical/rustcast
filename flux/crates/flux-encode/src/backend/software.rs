//! Software encoding fallback.
//!
//! Uses CPU-based encoders (e.g. x264, x265, or pure-Rust alternatives) when
//! no suitable GPU encoder is available. This is the last-resort fallback and
//! will consume significant CPU resources.

use flux_core::error::Result;
use flux_core::frame::{CapturedFrame, EncodedPacket};
use flux_core::types::{Resolution, VideoCodec};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

/// Software video encoder (CPU fallback).
pub struct SoftwareEncoder {
    _private: (),
}

impl SoftwareEncoder {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing software encoder fallback");
        // TODO: Check for system-installed libx264 / libx265 / rav1e
        Ok(Self { _private: () })
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn name(&self) -> &'static str {
        "Software (CPU)"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Ok(EncoderCapabilities {
            name: "Software (CPU)",
            supported_codecs: vec![VideoCodec::H264, VideoCodec::H265],
            supports_hdr: false,
            supports_yuv444: true,
            max_resolution: Resolution::new(7680, 4320),
            max_framerate: 60,
        })
    }

    fn validate_config(&self, _config: &EncodeConfig) -> Result<()> {
        Ok(())
    }

    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        Ok(Box::new(SoftwareSession::new(config)?))
    }
}

struct SoftwareSession {
    config: EncodeConfig,
    frame_index: u64,
    idr_requested: bool,
    // TODO: FFI handle to x264_t* or x265_encoder*
}

impl SoftwareSession {
    fn new(config: EncodeConfig) -> Result<Self> {
        tracing::info!(
            "Creating software encode session: {} {}@{}fps {}kbps",
            config.codec,
            config.resolution,
            config.framerate,
            config.bitrate_kbps,
        );

        // TODO: Initialize x264/x265 via FFI:
        //
        //   For x264:
        //     1. x264_param_default_preset(&param, "ultrafast", "zerolatency")
        //     2. Set width, height, fps, bitrate, keyint_max, bframes=0
        //     3. x264_encoder_open(&param)
        //
        //   For x265:
        //     1. x265_param_default_preset(&param, "ultrafast", "zerolatency")
        //     2. Similar configuration
        //     3. x265_encoder_open(&param)

        Ok(Self {
            config,
            frame_index: 0,
            idr_requested: true,
        })
    }
}

impl EncodeSession for SoftwareSession {
    fn encode(&mut self, _frame: &CapturedFrame) -> Result<Vec<EncodedPacket>> {
        self.frame_index += 1;
        let is_idr = self.idr_requested;
        self.idr_requested = false;

        // TODO: Software encode:
        //   1. Convert frame data to YUV420 (if RGB input)
        //   2. Fill x264_picture_t / x265_picture with plane pointers
        //   3. x264_encoder_encode / x265_encoder_encode
        //   4. Collect NAL units into output buffer

        tracing::trace!(
            "Software encode frame {} (IDR={})",
            self.frame_index,
            is_idr
        );

        Ok(vec![EncodedPacket {
            frame_index: self.frame_index,
            pts: self.frame_index,
            is_keyframe: is_idr,
            data: Vec::new(),
        }])
    }

    fn request_idr(&mut self) {
        self.idr_requested = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        // TODO: x264_encoder_encode with NULL pic to flush
        Ok(vec![])
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        // TODO: x264_encoder_reconfig / x265_encoder_reconfig
        self.config.bitrate_kbps = bitrate_kbps;
        Ok(())
    }
}
