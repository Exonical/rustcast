//! Encoder backend selection, bitrate targets and rebuild backoff.

use std::time::{Duration, Instant};

/// The ordered H.264 encoder backends to try for this build/platform: the
/// vendor hardware encoder first (AMF on Windows, FFmpeg-VA-API or
/// cros-codecs VA-API on Linux depending on the compiled features), then the
/// vendor-neutral Vulkan Video encoder, then the software fallback.
// Vec::new + cfg-gated pushes: `vec![]` can't hold conditional elements.
#[allow(clippy::vec_init_then_push)]
fn encoder_backend_candidates(
    forced: Option<flux_core::types::EncoderBackend>,
) -> Vec<flux_core::types::EncoderBackend> {
    use flux_core::types::EncoderBackend;
    if let Some(backend) = forced {
        tracing::info!("Encoder backend forced by config: {:?}", backend);
        // Keep the software fallback as the next candidate if the forced backend fails.
        if backend == EncoderBackend::Software {
            return vec![backend];
        }
        return vec![backend, EncoderBackend::Software];
    }
    let mut candidates = Vec::new();
    #[cfg(target_os = "windows")]
    candidates.push(EncoderBackend::Amf);
    #[cfg(all(target_os = "linux", feature = "encoder-ffmpeg"))]
    candidates.push(EncoderBackend::FfmpegVaapi);
    #[cfg(all(target_os = "linux", feature = "encoder-vaapi", not(feature = "encoder-ffmpeg")))]
    candidates.push(EncoderBackend::Vaapi);
    #[cfg(feature = "encoder-vulkan")]
    candidates.push(EncoderBackend::VulkanVideo);
    candidates.push(EncoderBackend::Software);
    candidates
}

/// Bits-per-pixel-per-frame coefficients for quality levels 1 through 10.
/// Level 3 is the former 0.05 setting; the default level 6 is 0.10.
/// Mirrored by `QUALITY_BPP` in `flux-web/ui/lib/protocol.ts`; its parity test parses this table.
fn quality_bpp(level: u8) -> f64 {
    match level.clamp(1, 10) {
        1 => 0.025,
        2 => 0.035,
        3 => 0.050,
        4 => 0.065,
        5 => 0.080,
        6 => 0.100,
        7 => 0.120,
        8 => 0.140,
        9 => 0.160,
        _ => 0.200,
    }
}

/// The clamp bounds are mirrored by `flux-web/ui/lib/protocol.ts` and checked by its parity test.
pub(crate) fn bitrate_kbps_for(
    resolution: flux_core::types::Resolution,
    fps: u32,
    level: u8,
) -> u32 {
    let bps = resolution.width as f64
        * resolution.height as f64
        * fps as f64
        * quality_bpp(level);
    (bps / 1000.0).round().clamp(3_000.0, 50_000.0) as u32
}

const ENCODER_RETRY_INITIAL: Duration = Duration::from_secs(1);
const ENCODER_RETRY_MAX: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct EncoderRetry {
    retry_at: Option<Instant>,
    next_delay: Duration,
}

impl Default for EncoderRetry {
    fn default() -> Self {
        Self {
            retry_at: None,
            next_delay: ENCODER_RETRY_INITIAL,
        }
    }
}

impl EncoderRetry {
    pub(crate) fn on_failure(&mut self, now: Instant) -> Duration {
        let delay = self.next_delay;
        self.retry_at = Some(now + delay);
        self.next_delay = delay.saturating_mul(2).min(ENCODER_RETRY_MAX);
        delay
    }

    pub(crate) fn on_success(&mut self) {
        self.retry_at = None;
        self.next_delay = ENCODER_RETRY_INITIAL;
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.retry_at.map_or(true, |retry_at| now >= retry_at)
    }
}

/// Create an encoder of the given backend and open a session, returning `None`
/// (with a warning logged) if either the encoder or the session can't be built.
/// If the requested resolution exceeds the encoder's maximum, the session is
/// opened at an aspect-preserving downscaled resolution instead; the actually
/// used resolution is returned alongside the session.
fn create_encode_session(
    backend: flux_core::types::EncoderBackend,
    mut config: flux_encode::traits::EncodeConfig,
    quality_level: u8,
    capture_device: Option<flux_core::frame::GpuDeviceHandle>,
) -> Option<(Box<dyn flux_encode::traits::EncodeSession>, flux_core::types::Resolution)> {
    let encoder = match flux_encode::create_encoder(Some(backend)) {
        Ok(enc) => {
            tracing::info!("Encoder created: {} ({:?})", enc.name(), backend);
            enc
        }
        Err(e) => {
            tracing::warn!("{:?} encoder not available: {}", backend, e);
            return None;
        }
    };
    if let Ok(caps) = encoder.capabilities() {
        let fitted = flux_encode::scale::fit_within(config.resolution, caps.max_resolution);
        if fitted != config.resolution {
            tracing::info!(
                "{:?}: downscaling {} → {} to fit encoder maximum {}",
                backend,
                config.resolution,
                fitted,
                caps.max_resolution
            );
            config.resolution = fitted;
        }
    }
    let resolution = config.resolution;
    config.bitrate_kbps = bitrate_kbps_for(resolution, config.framerate, quality_level);
    match encoder.create_session_with_device(config, capture_device) {
        Ok(s) => {
            tracing::info!("{:?} H.264 encode session started at {}", backend, resolution);
            Some((s, resolution))
        }
        Err(e) => {
            tracing::warn!("Failed to create {:?} encode session: {}", backend, e);
            None
        }
    }
}

/// Build an H.264 encode session for a concrete capture resolution, preferring
/// the platform hardware encoder and falling back to software when it can't be
/// opened (e.g. no VA-API driver). Returns the session and the backend used.
#[allow(clippy::type_complexity)]
pub(crate) fn build_encode_session_for(
    framerate: u32,
    resolution: flux_core::types::Resolution,
    forced_backend: Option<flux_core::types::EncoderBackend>,
    quality_level: u8,
    capture_device: Option<flux_core::frame::GpuDeviceHandle>,
) -> (
    Option<(Box<dyn flux_encode::traits::EncodeSession>, flux_core::types::Resolution)>,
    flux_core::types::EncoderBackend,
) {
    let encoder_config = flux_encode::traits::EncodeConfig {
        codec: flux_core::types::VideoCodec::H264,
        resolution,
        framerate,
        // Placeholder; recomputed per-backend from the fitted resolution.
        bitrate_kbps: 10_000,
        rate_control: flux_core::types::RateControlMode::Vbr,
        dynamic_range: flux_core::types::DynamicRange::Sdr,
        chroma_sampling: flux_core::types::ChromaSampling::Yuv420,
        // Emit a keyframe every ~2s so a late joiner or any decode desync
        // recovers on its own without depending solely on a keyframe request.
        gop_size: framerate.saturating_mul(2).max(1),
        b_frames: 0,
        max_ref_frames: 1,
    };

    let mut backend = flux_core::types::EncoderBackend::Software;
    let mut session = None;
    for candidate in encoder_backend_candidates(forced_backend) {
        session = create_encode_session(
            candidate,
            encoder_config.clone(),
            quality_level,
            capture_device.clone(),
        );
        backend = candidate;
        if session.is_some() {
            break;
        }
        tracing::warn!("{:?} encoder unavailable; trying next backend", candidate);
    }
    if session.is_none() {
        tracing::error!("No functional encoder backend available; video will not be streamed");
    }
    (session, backend)
}


#[cfg(test)]
mod quality_tests {
    use super::*;

    #[test]
    fn quality_levels_map_to_expected_1080p60_targets() {
        let resolution = flux_core::types::Resolution::new(1920, 1080);
        assert_eq!(bitrate_kbps_for(resolution, 60, 3), 6_221);
        assert_eq!(bitrate_kbps_for(resolution, 60, 6), 12_442);
        assert_eq!(bitrate_kbps_for(resolution, 60, 10), 24_883);
    }

    #[test]
    fn fps_cap_reduces_target_linearly() {
        let resolution = flux_core::types::Resolution::new(1920, 1080);
        assert_eq!(bitrate_kbps_for(resolution, 48, 6), 9_953);
    }
}

#[cfg(test)]
mod encoder_retry_tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_thirty_seconds_and_stays_capped() {
        let mut now = Instant::now();
        let mut retry = EncoderRetry::default();
        for seconds in [1, 2, 4, 8, 16, 30, 30] {
            assert!(retry.due(now));
            let delay = retry.on_failure(now);
            assert_eq!(delay, Duration::from_secs(seconds));
            now = now + delay;
        }
    }

    #[test]
    fn retry_is_not_due_before_its_deadline_and_is_due_at_it() {
        let now = Instant::now();
        let mut retry = EncoderRetry::default();
        let delay = retry.on_failure(now);

        assert!(!retry.due(now + delay - Duration::from_nanos(1)));
        assert!(retry.due(now + delay));
    }

    #[test]
    fn success_resets_backoff_to_one_second() {
        let now = Instant::now();
        let mut retry = EncoderRetry::default();
        retry.on_failure(now);
        retry.on_failure(now + Duration::from_secs(1));
        retry.on_success();

        assert!(retry.due(now + Duration::from_secs(1)));
        assert_eq!(
            retry.on_failure(now + Duration::from_secs(1)),
            Duration::from_secs(1)
        );
    }
}
