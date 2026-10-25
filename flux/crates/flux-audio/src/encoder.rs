//! Opus audio encoder.
//!
//! Opus is the standard codec for low-latency audio streaming. It provides
//! excellent quality at low bitrates and supports both voice and music content.

use flux_core::error::{FluxError, Result};
use flux_core::frame::EncodedAudioPacket;

use crate::traits::{AudioEncoder, AudioSamples};

/// Maximum bytes a single Opus packet can take (spec limit 1275; headroom).
const MAX_OPUS_PACKET: usize = 4000;

/// Configuration for the Opus encoder.
#[derive(Debug, Clone)]
pub struct OpusEncoderConfig {
    /// Sample rate (must be 8000, 12000, 16000, 24000, or 48000).
    pub sample_rate: u32,

    /// Number of channels (1 = mono, 2 = stereo).
    pub channels: u16,

    /// Target bitrate in bits per second (e.g. 128000 for 128 kbps).
    pub bitrate_bps: u32,

    /// Frame duration in milliseconds (2.5, 5, 10, 20, 40, 60).
    /// Lower = less latency, higher = better compression.
    pub frame_duration_ms: f32,

    /// Application hint for the encoder.
    pub application: OpusApplication,
}

impl Default for OpusEncoderConfig {
    fn default() -> Self {
        Self {
            sample_rate: 48000,
            channels: 2,
            bitrate_bps: 128_000,
            frame_duration_ms: 20.0,
            application: OpusApplication::Audio,
        }
    }
}

/// Opus application mode hint.
#[derive(Debug, Clone, Copy)]
pub enum OpusApplication {
    /// Optimize for voice (spoken content).
    Voip,
    /// Optimize for general audio / music.
    Audio,
    /// Optimize for lowest latency.
    LowDelay,
}

#[cfg(feature = "opus")]
/// Opus audio encoder wrapping libopus.
pub struct OpusEncoder {
    config: OpusEncoderConfig,
    encoder: opus::Encoder,
    sequence: u64,
    samples_per_frame: usize,
}

#[cfg(feature = "opus")]
impl OpusEncoder {
    pub fn new(config: OpusEncoderConfig) -> Result<Self> {
        tracing::info!(
            "Creating Opus encoder: {}Hz {}ch {}bps {:.1}ms",
            config.sample_rate,
            config.channels,
            config.bitrate_bps,
            config.frame_duration_ms,
        );

        if !matches!(config.sample_rate, 8_000 | 12_000 | 16_000 | 24_000 | 48_000) {
            return Err(FluxError::AudioEncode(format!(
                "unsupported Opus sample rate: {}",
                config.sample_rate
            )));
        }
        let channels = match config.channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            other => {
                return Err(FluxError::AudioEncode(format!(
                    "unsupported Opus channel count: {other}"
                )));
            }
        };
        let application = match config.application {
            OpusApplication::Voip => opus::Application::Voip,
            OpusApplication::Audio => opus::Application::Audio,
            OpusApplication::LowDelay => opus::Application::LowDelay,
        };

        let mut encoder = opus::Encoder::new(config.sample_rate, channels, application)
            .map_err(|e| FluxError::AudioEncode(format!("opus_encoder_create: {e}")))?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(config.bitrate_bps as i32))
            .map_err(|e| FluxError::AudioEncode(format!("OPUS_SET_BITRATE: {e}")))?;
        encoder
            .set_inband_fec(false)
            .map_err(|e| FluxError::AudioEncode(format!("OPUS_SET_INBAND_FEC: {e}")))?;

        let samples_per_frame = (config.sample_rate as f32 * config.frame_duration_ms / 1000.0) as usize;

        Ok(Self {
            config,
            encoder,
            sequence: 0,
            samples_per_frame,
        })
    }

    /// Number of samples per channel in each Opus frame.
    pub fn samples_per_frame(&self) -> usize {
        self.samples_per_frame
    }
}

#[cfg(feature = "opus")]
impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, samples: &AudioSamples) -> Result<EncodedAudioPacket> {
        let expected = self.samples_per_frame * self.config.channels as usize;
        if samples.data.len() != expected {
            return Err(FluxError::AudioEncode(format!(
                "expected {expected} interleaved samples, got {}",
                samples.data.len()
            )));
        }

        let mut output = vec![0u8; MAX_OPUS_PACKET];
        let written = self
            .encoder
            .encode_float(&samples.data, &mut output)
            .map_err(|e| FluxError::AudioEncode(format!("opus_encode_float: {e}")))?;
        output.truncate(written);

        self.sequence += 1;
        tracing::trace!("Opus encode frame {}", self.sequence);

        Ok(EncodedAudioPacket {
            sequence: self.sequence,
            timestamp: self.sequence * self.samples_per_frame as u64,
            data: output,
        })
    }

    fn flush(&mut self) -> Result<Vec<EncodedAudioPacket>> {
        // Opus doesn't buffer — each call to encode is self-contained.
        Ok(vec![])
    }
}

#[cfg(not(feature = "opus"))]
/// Stub used when the opus codec is not compiled in (e.g. Windows builds
/// without libopus). Construction always fails at runtime.
pub struct OpusEncoder;

#[cfg(not(feature = "opus"))]
impl OpusEncoder {
    pub fn new(_config: OpusEncoderConfig) -> Result<Self> {
        Err(FluxError::AudioEncode("opus support not compiled in".into()))
    }

    pub fn samples_per_frame(&self) -> usize {
        0
    }
}

#[cfg(not(feature = "opus"))]
impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, _samples: &AudioSamples) -> Result<EncodedAudioPacket> {
        Err(FluxError::AudioEncode("opus support not compiled in".into()))
    }

    fn flush(&mut self) -> Result<Vec<EncodedAudioPacket>> {
        Ok(vec![])
    }
}
