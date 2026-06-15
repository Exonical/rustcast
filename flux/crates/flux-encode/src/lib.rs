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
