//! Linux FFmpeg (libavcodec) hardware encoder backends.
//!
//! Two implementations live behind a feature flag so the default build needs no
//! libav* headers at compile time:
//!
//! - [`stub`] — dependency-free placeholders used when `encoder-ffmpeg` is off.
//! - [`real`] — the real encoders built on `ffmpeg-next`, enabled with
//!   `--features encoder-ffmpeg`:
//!   - [`FfmpegVaapiEncoder`] opens a VA-API device and drives
//!     `h264_vaapi`/`hevc_vaapi` (HEVC + HDR10).
//!   - [`FfmpegVulkanEncoder`] opens a Vulkan device and drives
//!     `h264_vulkan`/`hevc_vulkan` (cross-vendor; works with no VA-API driver,
//!     requires FFmpeg >= 7.0).
//!   - [`FfmpegSoftwareEncoder`] is the libx264/libx265 CPU fallback.
//!
//!   All upload NV12/P010 frames (color-converted with swscale) into the
//!   encoder; the hardware paths transfer them onto GPU surfaces first.
//!
//! Both modules expose the same types implementing
//! [`VideoEncoder`](crate::traits::VideoEncoder).

#[cfg(feature = "encoder-ffmpeg")]
mod real;
#[cfg(feature = "encoder-ffmpeg")]
pub use real::{FfmpegSoftwareEncoder, FfmpegVaapiEncoder, FfmpegVulkanEncoder};

#[cfg(not(feature = "encoder-ffmpeg"))]
mod stub;
#[cfg(not(feature = "encoder-ffmpeg"))]
pub use stub::{FfmpegSoftwareEncoder, FfmpegVaapiEncoder, FfmpegVulkanEncoder};
