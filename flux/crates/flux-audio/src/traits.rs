use flux_core::error::Result;
use flux_core::frame::EncodedAudioPacket;

/// Raw PCM audio samples from the capture backend.
#[derive(Debug, Clone)]
pub struct AudioSamples {
    /// Interleaved PCM samples (f32).
    pub data: Vec<f32>,

    /// Sample rate in Hz.
    pub sample_rate: u32,

    /// Number of channels.
    pub channels: u16,

    /// Monotonic sequence number.
    pub sequence: u64,

    /// Timestamp of the first sample in this buffer (monotonic clock).
    pub timestamp: std::time::Instant,
}

/// Information about an audio output device.
#[derive(Debug, Clone)]
pub struct AudioDeviceInfo {
    /// Opaque device identifier.
    pub id: String,

    /// Human-readable name.
    pub name: String,

    /// Sample rate in Hz.
    pub sample_rate: u32,

    /// Number of channels.
    pub channels: u16,

    /// Whether this is the default device.
    pub is_default: bool,
}

/// An active audio capture session that produces PCM sample buffers.
pub trait AudioCaptureSession: Send {
    /// List available audio output devices.
    fn enumerate_devices(&self) -> Result<Vec<AudioDeviceInfo>>;

    /// Start capturing from a specific device (or the default if `None`).
    fn start(&mut self, device_id: Option<&str>) -> Result<()>;

    /// Block until the next audio buffer is available.
    fn next_samples(&mut self) -> Result<AudioSamples>;

    /// Stop capturing.
    fn stop(&mut self) -> Result<()>;
}

/// An audio encoder that converts PCM samples to compressed packets.
pub trait AudioEncoder: Send {
    /// Encode a buffer of PCM samples.
    fn encode(&mut self, samples: &AudioSamples) -> Result<EncodedAudioPacket>;

    /// Flush any remaining buffered samples.
    fn flush(&mut self) -> Result<Vec<EncodedAudioPacket>>;
}
