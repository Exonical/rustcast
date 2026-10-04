//! Placeholder VA-API encoder used when `encoder-vaapi` is disabled.
//!
//! Keeps the default build free of the libva headers. Construction fails so
//! callers fall back to another backend; selecting `Vaapi` without the feature
//! is a configuration error rather than a silent no-op producing empty packets.

use flux_core::error::{FluxError, Result};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

const NOT_COMPILED_IN: &str = "VA-API encoder not compiled in (enable the `encoder-vaapi` feature)";

/// VA-API hardware encoder (Linux) — disabled build.
pub struct VaapiEncoder {
    _private: (),
}

impl VaapiEncoder {
    pub fn new() -> Result<Self> {
        Err(FluxError::EncoderInit(NOT_COMPILED_IN.into()))
    }
}

impl VideoEncoder for VaapiEncoder {
    fn name(&self) -> &'static str {
        "VA-API"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Err(FluxError::EncoderInit(NOT_COMPILED_IN.into()))
    }

    fn validate_config(&self, _config: &EncodeConfig) -> Result<()> {
        Err(FluxError::EncoderInit(NOT_COMPILED_IN.into()))
    }

    fn create_session(&self, _config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        Err(FluxError::EncoderInit(NOT_COMPILED_IN.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_fails_instead_of_faking_success() {
        assert!(matches!(VaapiEncoder::new(), Err(FluxError::EncoderInit(_))));
    }
}
