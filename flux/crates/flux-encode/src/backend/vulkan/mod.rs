//! Vulkan Video encoder backend.
//!
//! Uses the `VK_KHR_video_encode_queue` family of extensions to perform
//! hardware-accelerated video encoding through the Vulkan API. This is the
//! most portable GPU encoding path — it works on AMD (RADV, Mesa 24.1+),
//! Intel (ANV) and NVIDIA GPUs that expose the Vulkan Video extensions,
//! with no dependency beyond the Vulkan loader and the GPU driver itself.
//!
//! The real encoder is compiled in with the `encoder-vulkan` feature; without
//! it a placeholder implementation is used (mirrors the VA-API backend's
//! real/stub split).

#[cfg(feature = "encoder-vulkan")]
mod h264;
#[cfg(feature = "encoder-vulkan")]
mod real;
#[cfg(feature = "encoder-vulkan")]
pub use real::VulkanVideoEncoder;

#[cfg(not(feature = "encoder-vulkan"))]
mod stub;
#[cfg(not(feature = "encoder-vulkan"))]
pub use stub::VulkanVideoEncoder;
