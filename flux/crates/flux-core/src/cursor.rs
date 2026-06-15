//! Cursor metadata model.
//!
//! When a Wayland capture session negotiates `CursorMode::Metadata`, the
//! compositor delivers the pointer cursor *out of band* (rather than composited
//! into the frame pixels) so the remote side can render it locally with low
//! latency and at the client's native resolution. These types are the
//! platform-independent representation of that metadata; the PipeWire/SPA
//! decoding lives in `flux-capture`.

/// Cursor position and (optionally) shape for a single moment in time.
///
/// A cursor update may carry only a new position (the common case, every
/// frame) or also a new shape (`bitmap`), which changes far less often. When
/// the cursor is hidden / cleared, `position` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorMetadata {
    /// Cursor position in the capture stream's coordinate space, or `None`
    /// when the cursor is hidden / has left the captured region.
    pub position: Option<(i32, i32)>,

    /// Hotspot offset within [`CursorBitmap`] (the pixel that tracks
    /// `position`). Meaningless unless a bitmap has been seen.
    pub hotspot: (i32, i32),

    /// Cursor shape, present only on updates that change it. Once received it
    /// remains valid until the next shape change, so consumers should cache
    /// the most recent non-`None` bitmap.
    pub bitmap: Option<CursorBitmap>,
}

/// A cursor shape: raw pixels plus their geometry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorBitmap {
    /// Bitmap width in pixels.
    pub width: u32,
    /// Bitmap height in pixels.
    pub height: u32,
    /// Row stride in bytes (may exceed `width * bytes_per_pixel`).
    pub stride: u32,
    /// Pixel format as a SPA `spa_video_format` id (e.g. BGRA). `0` is invalid.
    pub format: u32,
    /// Tightly-referenced pixel data of length `stride * height`.
    pub pixels: Vec<u8>,
}

impl CursorMetadata {
    /// A "cursor hidden / cleared" update: no position and no shape.
    pub fn hidden() -> Self {
        Self {
            position: None,
            hotspot: (0, 0),
            bitmap: None,
        }
    }
}
