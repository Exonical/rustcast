//! Linux FFmpeg (libavcodec) VA-API encoder backend.
//!
//! Two implementations live behind a feature flag so the default build needs no
//! libav* headers at compile time:
//!
//! - [`stub`] — a dependency-free placeholder used when `encoder-ffmpeg` is off.
//! - [`real`] — the real encoder built on `ffmpeg-next`, enabled with
//!   `--features encoder-ffmpeg`. It opens a VA-API hardware device, drives
//!   `h264_vaapi`/`hevc_vaapi` (HEVC + HDR10) with low-latency tuning, and
//!   uploads NV12/P010 frames into VA-API surfaces for encode.
//!
//! Both expose the same [`FfmpegVaapiEncoder`] type implementing
//! [`VideoEncoder`](crate::traits::VideoEncoder).

#[cfg(feature = "encoder-ffmpeg")]
mod real;
#[cfg(feature = "encoder-ffmpeg")]
pub use real::{FfmpegSoftwareEncoder, FfmpegVaapiEncoder};

#[cfg(not(feature = "encoder-ffmpeg"))]
mod stub;
#[cfg(not(feature = "encoder-ffmpeg"))]
pub use stub::{FfmpegSoftwareEncoder, FfmpegVaapiEncoder};
