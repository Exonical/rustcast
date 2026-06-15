pub mod backend;
pub mod color;
// DMA-BUF GPU import is a unix/Linux concept (VA-API/Vulkan); Windows imports
// DXGI shared textures directly inside the AMF backend.
#[cfg(unix)]
pub mod import;
pub mod traits;

#[cfg(unix)]
pub use import::{classify_frame, GpuFrameImport, ImportPath};
pub use traits::{EncodeSession, VideoEncoder};

use flux_core::error::Result;
use flux_core::types::EncoderBackend;

/// Create the best available encoder backend for this platform and GPU.
pub fn create_encoder(backend: Option<EncoderBackend>) -> Result<Box<dyn VideoEncoder>> {
    let backend = backend.unwrap_or_else(default_backend);

    tracing::info!("Initializing encoder backend: {:?}", backend);

    match backend {
        EncoderBackend::Nvenc => Ok(Box::new(backend::nvenc::NvencEncoder::new()?)),

        #[cfg(target_os = "linux")]
        EncoderBackend::Vaapi => Ok(Box::new(backend::vaapi::VaapiEncoder::new()?)),

        #[cfg(target_os = "linux")]
        EncoderBackend::FfmpegVaapi => Ok(Box::new(backend::ffmpeg::FfmpegVaapiEncoder::new()?)),

        #[cfg(target_os = "windows")]
        EncoderBackend::Amf => Ok(Box::new(backend::amf::AmfEncoder::new()?)),

        EncoderBackend::VulkanVideo => Ok(Box::new(backend::vulkan::VulkanVideoEncoder::new()?)),

        EncoderBackend::Software => Ok(Box::new(backend::software::SoftwareEncoder::new()?)),

        #[allow(unreachable_patterns)]
        _ => Err(flux_core::FluxError::NoEncoderBackend),
    }
}

fn default_backend() -> EncoderBackend {
    // Vulkan Video is the preferred cross-platform, cross-vendor default.
    // Specific vendor backends (NVENC, AMF) can be selected explicitly.
    EncoderBackend::VulkanVideo
}

/// Probe the VA-API driver and report its *drivable* encode capabilities as a
/// [`flux_core::capability::EncodeCapabilities`], for capability negotiation.
///
/// Opens a DRM render node, queries the driver's encode entrypoints, and maps
/// the result onto the core capability type. Only codecs the backend can
/// actually drive are reported (the VA-API backend encodes H.264 today, even
/// where the driver advertises HEVC/AV1), so negotiation never selects a codec
/// that `create_session` would then reject. Returns `None` when no VA-API
/// encoder can be opened (no render node, no encode entrypoint, etc.).
#[cfg(all(target_os = "linux", feature = "encoder-vaapi"))]
pub fn probe_encode_capabilities() -> Option<flux_core::capability::EncodeCapabilities> {
    let encoder = backend::vaapi::VaapiEncoder::new().ok()?;
    let caps = encoder.capabilities().ok()?;
    Some(to_core_encode_caps(
        encoder.driver(),
        &caps.supported_codecs,
        caps.supports_hdr,
    ))
}

/// Map a VA-API backend's drivable codec set onto the core capability type.
#[cfg(all(target_os = "linux", feature = "encoder-vaapi"))]
fn to_core_encode_caps(
    driver: &str,
    supported: &[flux_core::types::VideoCodec],
    supports_hdr: bool,
) -> flux_core::capability::EncodeCapabilities {
    use flux_core::types::VideoCodec;
    flux_core::capability::EncodeCapabilities {
        backend: Some(EncoderBackend::Vaapi),
        driver: (!driver.is_empty()).then(|| driver.to_string()),
        h264: supported.contains(&VideoCodec::H264),
        h265: supported.contains(&VideoCodec::H265),
        av1: supported.contains(&VideoCodec::Av1),
        hdr10: supports_hdr,
    }
}

/// Probe the FFmpeg VA-API backend and report its drivable encode
/// capabilities (H.264 + HEVC, plus HDR10 when HEVC is present).
///
/// Opens a VA-API hardware device and checks which `*_vaapi` encoders the
/// FFmpeg build exposes. Reported as [`EncoderBackend::FfmpegVaapi`] so
/// negotiation can prefer it for codecs the cros-codecs path can't drive
/// (HEVC/HDR). Returns `None` when no VA-API device/encoder is available.
#[cfg(all(target_os = "linux", feature = "encoder-ffmpeg"))]
pub fn probe_ffmpeg_encode_capabilities() -> Option<flux_core::capability::EncodeCapabilities> {
    use flux_core::types::VideoCodec;
    let encoder = backend::ffmpeg::FfmpegVaapiEncoder::new().ok()?;
    let caps = encoder.capabilities().ok()?;
    Some(flux_core::capability::EncodeCapabilities {
        backend: Some(EncoderBackend::FfmpegVaapi),
        driver: None,
        h264: caps.supported_codecs.contains(&VideoCodec::H264),
        h265: caps.supported_codecs.contains(&VideoCodec::H265),
        av1: caps.supported_codecs.contains(&VideoCodec::Av1),
        hdr10: caps.supports_hdr,
    })
}

#[cfg(all(target_os = "linux", feature = "encoder-vaapi", test))]
mod probe_tests {
    use super::*;
    use flux_core::types::VideoCodec;

    #[test]
    fn maps_drivable_codecs_onto_core_caps() {
        let caps = to_core_encode_caps("Mesa Gallium", &[VideoCodec::H264], false);
        assert_eq!(caps.backend, Some(EncoderBackend::Vaapi));
        assert_eq!(caps.driver.as_deref(), Some("Mesa Gallium"));
        assert!(caps.h264);
        assert!(!caps.h265);
        assert!(!caps.av1);
        assert!(!caps.hdr10);
    }

    #[test]
    fn empty_driver_string_maps_to_none() {
        let caps = to_core_encode_caps("", &[], false);
        assert!(caps.driver.is_none());
        assert!(!caps.h264);
    }
}
