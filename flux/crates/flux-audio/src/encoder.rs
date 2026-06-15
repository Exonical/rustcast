//! Opus audio encoder.
//!
//! Opus is the standard codec for low-latency audio streaming. It provides
//! excellent quality at low bitrates and supports both voice and music content.

use flux_core::error::Result;
use flux_core::frame::EncodedAudioPacket;

use crate::traits::{AudioEncoder, AudioSamples};

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
            bitrate_bps: 256_000,
            frame_duration_ms: 5.0, // Low latency: 5ms frames
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

/// Opus audio encoder wrapping libopus via FFI.
pub struct OpusEncoder {
    _config: OpusEncoderConfig,
    sequence: u64,
    samples_per_frame: usize,
    // TODO: *mut OpusEncoder (FFI handle from opus_encoder_create)
}

impl OpusEncoder {
    pub fn new(config: OpusEncoderConfig) -> Result<Self> {
        tracing::info!(
            "Creating Opus encoder: {}Hz {}ch {}bps {:.1}ms",
            config.sample_rate,
            config.channels,
            config.bitrate_bps,
            config.frame_duration_ms,
        );

        let samples_per_frame =
            (config.sample_rate as f32 * config.frame_duration_ms / 1000.0) as usize;

        // TODO: FFI initialization:
        //
        //   let application = match config.application {
        //       OpusApplication::Voip => OPUS_APPLICATION_VOIP,
        //       OpusApplication::Audio => OPUS_APPLICATION_AUDIO,
        //       OpusApplication::LowDelay => OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        //   };
        //
        //   let mut error: c_int = 0;
        //   let encoder = opus_encoder_create(
        //       config.sample_rate as i32,
        //       config.channels as i32,
        //       application,
        //       &mut error,
        //   );
        //
        //   opus_encoder_ctl(encoder, OPUS_SET_BITRATE(config.bitrate_bps as i32));
        //   opus_encoder_ctl(encoder, OPUS_SET_SIGNAL(OPUS_SIGNAL_MUSIC));
        //   opus_encoder_ctl(encoder, OPUS_SET_COMPLEXITY(10)); // max quality

        Ok(Self {
            _config: config,
            sequence: 0,
            samples_per_frame,
        })
    }

    /// Number of samples per channel in each Opus frame.
    pub fn samples_per_frame(&self) -> usize {
        self.samples_per_frame
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, _samples: &AudioSamples) -> Result<EncodedAudioPacket> {
        self.sequence += 1;

        // TODO: FFI encode:
        //
        //   let frame_size = self.samples_per_frame;
        //   let mut output = vec![0u8; 4000]; // max Opus packet size
        //
        //   let encoded_bytes = opus_encode_float(
        //       self.encoder,
        //       samples.data.as_ptr(),
        //       frame_size as i32,
        //       output.as_mut_ptr(),
        //       output.len() as i32,
        //   );
        //
        //   output.truncate(encoded_bytes as usize);

        tracing::trace!("Opus encode frame {}", self.sequence);

        Ok(EncodedAudioPacket {
            sequence: self.sequence,
            timestamp: self.sequence * self.samples_per_frame as u64,
            data: Vec::new(), // placeholder
        })
    }

    fn flush(&mut self) -> Result<Vec<EncodedAudioPacket>> {
        // Opus doesn't buffer — each call to encode is self-contained.
        Ok(vec![])
    }
}
