//! Linux VA-API encoder backend.
//!
//! Two implementations live behind a feature flag so the default build needs no
//! libva headers at compile time:
//!
//! - [`stub`] — a dependency-free placeholder used when `encoder-vaapi` is off.
//! - [`real`] — the real hardware encoder built on `cros-codecs` + `cros-libva`,
//!   enabled with `--features encoder-vaapi`. It opens a DRM render node, probes
//!   the driver's encode entrypoints, and runs an H.264 encode pipeline (NV12
//!   surface upload → bitstream) on a dedicated thread.
//!
//! Both expose the same [`VaapiEncoder`] type implementing
//! [`VideoEncoder`](crate::traits::VideoEncoder).

#[cfg(feature = "encoder-vaapi")]
mod real;
#[cfg(feature = "encoder-vaapi")]
pub use real::VaapiEncoder;

#[cfg(not(feature = "encoder-vaapi"))]
mod stub;
#[cfg(not(feature = "encoder-vaapi"))]
pub use stub::VaapiEncoder;
