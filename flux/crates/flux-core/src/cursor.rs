//! Cursor metadata model.
//!
//! When a Wayland capture session negotiates `CursorMode::Metadata`, the
//! compositor delivers the pointer cursor *out of band* (rather than composited
//! into the frame pixels) so the remote side can render it locally with low
//! latency and at the client's native resolution. These types are the
//! platform-independent representation of that metadata; the PipeWire/SPA
//! decoding lives in `flux-capture`.
use serde::{Deserialize, Serialize};

mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        STANDARD.decode(encoded).map_err(serde::de::Error::custom)
    }
}

/// Cursor position and (optionally) shape for a single moment in time.
///
/// A cursor update may carry only a new position (the common case, every
/// frame) or also a new shape (`bitmap`), which changes far less often. When
/// the cursor is hidden / cleared, `position` is `None`.

pub const CURSOR_FORMAT_RGBA8888: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    #[serde(with = "base64_bytes")]
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
