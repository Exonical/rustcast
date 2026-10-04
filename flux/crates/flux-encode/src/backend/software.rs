//! Placeholder software (CPU) encoder.
//!
//! A real CPU encoder (libx264 / libx265 / rav1e) is not implemented here; on
//! Linux the `encoder-ffmpeg` feature provides one instead. Construction fails
//! so backend selection reports "no functional encoder" rather than selecting
//! a no-op that would silently produce empty packets.

use flux_core::error::{FluxError, Result};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

const NOT_IMPLEMENTED: &str = "Software (CPU) encoder not implemented \
     (on Linux, enable the `encoder-ffmpeg` feature for a libx264/libx265 fallback)";

/// Software video encoder (CPU fallback) — not implemented.
pub struct SoftwareEncoder {
    _private: (),
}

impl SoftwareEncoder {
    pub fn new() -> Result<Self> {
        // TODO: Check for system-installed libx264 / libx265 / rav1e
        Err(FluxError::EncoderInit(NOT_IMPLEMENTED.into()))
    }
}

impl VideoEncoder for SoftwareEncoder {
    fn name(&self) -> &'static str {
        "Software (CPU)"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Err(FluxError::EncoderInit(NOT_IMPLEMENTED.into()))
    }

    fn validate_config(&self, _config: &EncodeConfig) -> Result<()> {
        Err(FluxError::EncoderInit(NOT_IMPLEMENTED.into()))
    }

    fn create_session(&self, _config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        Err(FluxError::EncoderInit(NOT_IMPLEMENTED.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_fails_instead_of_faking_success() {
        assert!(matches!(SoftwareEncoder::new(), Err(FluxError::EncoderInit(_))));
    }
}
