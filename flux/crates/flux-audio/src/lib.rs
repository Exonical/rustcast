pub mod capture;
pub mod encoder;
pub mod traits;

pub use traits::{AudioCaptureSession, AudioEncoder};

use flux_core::error::Result;

/// Create an audio capture session for the current platform.
pub fn create_audio_capture() -> Result<Box<dyn AudioCaptureSession>> {
    #[cfg(target_os = "windows")]
    {
        tracing::info!("Creating WASAPI audio capture");
        Ok(Box::new(capture::wasapi::WasapiCapture::new()?))
    }
    #[cfg(target_os = "linux")]
    {
        tracing::info!("Creating PipeWire audio capture");
        Ok(Box::new(capture::pipewire::PipeWireAudioCapture::new()?))
    }
}

/// Create an Opus audio encoder.
pub fn create_audio_encoder(config: encoder::OpusEncoderConfig) -> Result<encoder::OpusEncoder> {
    encoder::OpusEncoder::new(config)
}
