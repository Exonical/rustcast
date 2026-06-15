use crate::types::{PixelFormat, Resolution};

/// A captured video frame ready for encoding.
///
/// Frames may be backed by a CPU buffer (`data`) **or** by a GPU handle
/// (`gpu_handle`) when zero-copy capture is possible (e.g. DMA-BUF, DXGI
/// shared texture).
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// Sequence number monotonically increasing per capture session.
    pub sequence: u64,

    /// Capture timestamp (monotonic clock).
    pub timestamp: std::time::Instant,

    /// Pixel format of the raw frame data.
    pub format: PixelFormat,

    /// Resolution of this frame.
    pub resolution: Resolution,

    /// Row stride in bytes (may include padding).
    pub stride: u32,

    /// Raw pixel data. Empty when the frame is GPU-resident only.
    pub data: Vec<u8>,

    /// Opaque GPU resource handle for zero-copy paths.
    ///
    /// On Windows this is a `HANDLE` to a DXGI shared texture.
    /// On Linux this is a DMA-BUF file descriptor.
    pub gpu_handle: Option<GpuFrameHandle>,
}

/// Opaque handle to a GPU-resident frame.
#[derive(Debug, Clone)]
pub enum GpuFrameHandle {
    /// DMA-BUF descriptor (Linux/unix only — see [`DmaBufHandle`]).
    #[cfg(unix)]
    DmaBuf(DmaBufHandle),
    /// DXGI shared texture handle (Windows).
    DxgiSharedTexture(DxgiTextureHandle),
}

/// A single plane of a DMA-BUF frame.
///
/// Multi-planar formats (e.g. NV12, P010) and some tiled/compressed DRM
/// modifiers split a frame across up to four planes, each with its own
/// file descriptor, byte offset, and row stride.
///
/// The descriptor is wrapped in `Arc<OwnedFd>` so that:
/// - the fd is closed exactly once when the last clone is dropped, and
/// - `CapturedFrame` (which owns this) can stay `Clone` without `dup(2)`-ing
///   the fd on every clone — clones share the same underlying DMA-BUF.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct DmaBufPlane {
    /// Owned DMA-BUF file descriptor for this plane.
    pub fd: std::sync::Arc<std::os::fd::OwnedFd>,
    /// Byte offset of the plane within the buffer referenced by `fd`.
    pub offset: u32,
    /// Row stride in bytes.
    pub stride: u32,
}

/// Linux DMA-BUF frame descriptor.
///
/// Describes a GPU-resident frame that can be imported into an encoder (e.g.
/// VA-API via `VASurfaceAttribExternalBuffers` / `DRM_PRIME_2`) with no CPU
/// copy.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct DmaBufHandle {
    /// One entry per plane (1 for packed RGB/BGRx, 2 for NV12/P010, …).
    pub planes: Vec<DmaBufPlane>,
    /// DRM format modifier shared by all planes (`DRM_FORMAT_MOD_*`).
    pub modifier: u64,
    /// DRM FourCC pixel format (`DRM_FORMAT_*`, e.g. `XR24`, `NV12`).
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
}

/// Windows DXGI shared texture descriptor.
#[derive(Debug, Clone)]
pub struct DxgiTextureHandle {
    /// The raw `HANDLE` value (transmuted to `u64` for portability).
    pub handle: u64,
    pub width: u32,
    pub height: u32,
}

/// An encoded video packet ready for network transmission.
#[derive(Debug, Clone)]
pub struct EncodedPacket {
    /// Monotonically increasing frame index.
    pub frame_index: u64,

    /// Presentation timestamp in encoder timebase units.
    pub pts: u64,

    /// Whether this packet is an IDR / key-frame.
    pub is_keyframe: bool,

    /// Compressed bitstream data.
    pub data: Vec<u8>,
}

/// An encoded audio packet.
#[derive(Debug, Clone)]
pub struct EncodedAudioPacket {
    /// Sequence number.
    pub sequence: u64,

    /// Timestamp in sample units.
    pub timestamp: u64,

    /// Compressed audio data.
    pub data: Vec<u8>,
}
