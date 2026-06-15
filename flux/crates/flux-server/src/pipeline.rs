//! Streaming pipeline: capture → encode → packetize → transmit.
//!
//! Orchestrates the full media pipeline for a single session. Each pipeline
//! runs on its own thread (capture/encode are synchronous GPU operations)
//! with async I/O for network transmission.
//!
//! This is the Phase 0 skeleton: it performs capability detection, selects the
//! capture/encoder/input backends, constructs them, and builds the encoder
//! configuration. The dedicated capture→encode→transmit thread and the audio
//! path are wired up in later phases (see the project plan).

use parking_lot::Mutex;

use flux_capture::CaptureSession;
use flux_core::capability::{BaseCapabilityProbe, InputBackendKind, PlatformCapabilities};
use flux_core::config::FluxConfig;
use flux_core::error::Result;
use flux_core::platform::PlatformInfo;
use flux_core::types::{CaptureBackend, EncoderBackend};
use flux_encode::traits::EncodeConfig;
use flux_encode::EncodeSession;
use flux_input::{select_input_backend, InputBackend};

use crate::session::SessionParams;

/// The full capture → encode → transmit pipeline for one session.
#[allow(dead_code)]
pub struct StreamingPipeline {
    running: bool,
    /// Probed host capabilities used to drive backend selection.
    capabilities: PlatformCapabilities,
    capture_backend: CaptureBackend,
    encoder_backend: EncoderBackend,
    encode_config: EncodeConfig,
    /// Active capture session (Phase 1+ feeds this from PipeWire).
    capture: Mutex<Box<dyn CaptureSession>>,
    /// Active encode session.
    encode: Mutex<Box<dyn EncodeSession>>,
    /// Input injection backend, present when input forwarding is enabled.
    input: Option<Box<dyn InputBackend>>,
}

// Accessors and live controls (request_idr/set_bitrate) are consumed by the
// HTTP control path wired up in later phases; allow them ahead of that.
#[allow(dead_code)]
impl StreamingPipeline {
    /// Create and initialize the pipeline components.
    pub fn new(config: &FluxConfig, platform: &PlatformInfo, params: &SessionParams) -> Result<Self> {
        tracing::info!(
            "Initializing streaming pipeline: {} {}@{}fps {}kbps",
            params.codec,
            params.resolution,
            params.fps,
            params.video_bitrate_kbps,
        );

        let capabilities = BaseCapabilityProbe::from_platform_info(platform);

        // ── Negotiate backends from probed capabilities ──────────────
        let plan = capabilities.negotiate(params.codec, params.enable_input)?;
        let capture_backend = plan.capture;
        let encoder_backend = plan.encoder;

        if plan.codec != params.codec {
            tracing::warn!(
                "requested codec {:?} not supported by encoder; using {:?}",
                params.codec,
                plan.codec,
            );
        }
        tracing::info!(
            "Negotiated backends: capture={:?} encoder={:?} codec={:?} input={:?} zero_copy={}",
            capture_backend,
            encoder_backend,
            plan.codec,
            plan.input,
            plan.zero_copy,
        );

        // ── Capture ──────────────────────────────────────────────────
        let capture_factory = flux_capture::create_capture(Some(capture_backend))?;
        let capture_session = capture_factory.start_capture(None, params.resolution, params.fps)?;

        // ── Encoder ──────────────────────────────────────────────────
        let encode_config = EncodeConfig {
            codec: plan.codec,
            resolution: params.resolution,
            framerate: params.fps,
            bitrate_kbps: params.video_bitrate_kbps,
            ..Default::default()
        };
        let encoder = flux_encode::create_encoder(Some(encoder_backend))?;
        encoder.validate_config(&encode_config)?;
        let encode_session = encoder.create_session(encode_config.clone())?;

        // ── Input ────────────────────────────────────────────────────
        let input = match plan.input {
            InputBackendKind::None => None,
            kind => Some(select_input_backend(kind)),
        };

        // ── Transport / threads (Phase 1+) ───────────────────────────
        // The dedicated capture→encode loop and UDP/RTP transmit path are
        // wired up once the real PipeWire capture and VA-API encode backends
        // land. `config` is retained for that work.
        let _ = config;

        Ok(Self {
            running: true,
            capabilities,
            capture_backend,
            encoder_backend,
            encode_config,
            capture: Mutex::new(capture_session),
            encode: Mutex::new(encode_session),
            input,
        })
    }

    /// The capture backend selected for this session.
    pub fn capture_backend(&self) -> CaptureBackend {
        self.capture_backend
    }

    /// The encoder backend selected for this session.
    pub fn encoder_backend(&self) -> EncoderBackend {
        self.encoder_backend
    }

    /// The encoder configuration in use.
    pub fn encode_config(&self) -> &EncodeConfig {
        &self.encode_config
    }

    /// The input backend kind, if input forwarding is enabled.
    pub fn input_backend(&self) -> Option<InputBackendKind> {
        self.input.as_ref().map(|_| self.capabilities.input_backend)
    }

    /// Whether the pipeline is running.
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Request an IDR / key-frame from the encoder.
    pub fn request_idr(&self) {
        self.encode.lock().request_idr();
        tracing::debug!("IDR frame requested");
    }

    /// Update the target video bitrate.
    pub fn set_bitrate(&self, bitrate_kbps: u32) -> Result<()> {
        self.encode.lock().set_bitrate(bitrate_kbps)?;
        tracing::info!("Video bitrate updated to {} kbps", bitrate_kbps);
        Ok(())
    }

    /// Stop the pipeline and release resources.
    pub fn stop(self) -> Result<()> {
        tracing::info!("Stopping streaming pipeline");
        self.capture.lock().stop()?;
        let _ = self.encode.lock().flush();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::platform::Os;
    use flux_core::types::{GpuVendor, Resolution, VideoCodec};

    fn linux_amd_platform() -> PlatformInfo {
        PlatformInfo {
            os: Os::Linux,
            gpu_vendor: GpuVendor::Amd,
            available_capture_backends: vec![CaptureBackend::PipeWire, CaptureBackend::Drm],
            available_encoder_backends: vec![EncoderBackend::Vaapi, EncoderBackend::Software],
        }
    }

    fn params(enable_input: bool) -> SessionParams {
        SessionParams {
            client_name: "test".into(),
            codec: VideoCodec::H264,
            resolution: Resolution::new(1280, 720),
            fps: 60,
            video_bitrate_kbps: 8000,
            audio_bitrate_kbps: 128,
            enable_input,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn builds_and_selects_linux_amd_backends() {
        let config = FluxConfig::default();
        let platform = linux_amd_platform();
        let pipeline = StreamingPipeline::new(&config, &platform, &params(true)).unwrap();

        assert_eq!(pipeline.capture_backend(), CaptureBackend::PipeWire);
        assert_eq!(pipeline.encoder_backend(), EncoderBackend::Vaapi);
        assert_eq!(pipeline.encode_config().codec, VideoCodec::H264);
        assert_eq!(pipeline.encode_config().bitrate_kbps, 8000);
        assert_eq!(pipeline.input_backend(), Some(InputBackendKind::Portal));
        assert!(pipeline.is_running());

        pipeline.request_idr();
        pipeline.set_bitrate(4000).unwrap();
        pipeline.stop().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn input_disabled_has_no_backend() {
        let config = FluxConfig::default();
        let platform = linux_amd_platform();
        let pipeline = StreamingPipeline::new(&config, &platform, &params(false)).unwrap();
        assert!(pipeline.input_backend().is_none());
    }
}
