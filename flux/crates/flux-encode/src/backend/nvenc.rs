//! Placeholder NVIDIA NVENC hardware encoder.
//!
//! A real NVENC backend (NVIDIA Video Codec SDK via `nvEncodeAPI64.dll` /
//! `libnvidia-encode.so`) is not implemented yet. Construction fails so
//! backend selection falls through to another candidate rather than locking in
//! a no-op that would silently produce empty packets.

use flux_core::error::{FluxError, Result};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

const NOT_IMPLEMENTED: &str = "NVENC encoder not implemented / not available";

/// NVENC hardware encoder — not implemented.
pub struct NvencEncoder {
    _private: (),
}

impl NvencEncoder {
    pub fn new() -> Result<Self> {
        // TODO: Initialization sequence:
        //   1. Load nvEncodeAPI64.dll (Windows) or libnvidia-encode.so (Linux)
        //   2. NvEncodeAPIGetMaxSupportedVersion — verify driver compatibility
        //   3. NvEncodeAPICreateInstance — get function pointers
        //   4. Create CUDA context (cuCtxCreate) or D3D11 device
        //   5. NvEncOpenEncodeSessionEx — open a session to probe capabilities
        Err(FluxError::EncoderInit(NOT_IMPLEMENTED.into()))
    }
}

impl VideoEncoder for NvencEncoder {
    fn name(&self) -> &'static str {
        "NVENC"
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
        assert!(matches!(NvencEncoder::new(), Err(FluxError::EncoderInit(_))));
    }
}
