//! Decoding of PipeWire/SPA cursor metadata (`SPA_META_Cursor`).
//!
//! When a ScreenCast session negotiates `CursorMode::Metadata`, each PipeWire
//! buffer may carry a `struct spa_meta_cursor` describing the pointer position
//! and (occasionally) its shape. This module turns that raw, C-ABI byte region
//! into a platform-independent [`CursorMetadata`].
//!
//! The C layout (from `spa/buffer/meta.h`), all fields host-endian:
//!
//! ```text
//! struct spa_meta_cursor {        // 28 bytes
//!     uint32_t id;                // 0 => invalid/clear cursor
//!     uint32_t flags;
//!     struct spa_point position;  // int32 x, y
//!     struct spa_point hotspot;   // int32 x, y
//!     uint32_t bitmap_offset;     // 0 => no new shape; else offset to bitmap
//! };
//! struct spa_meta_bitmap {        // 20 bytes
//!     uint32_t format;            // spa_video_format; 0 => invalid
//!     struct spa_rectangle size;  // uint32 width, height
//!     int32_t  stride;
//!     uint32_t offset;            // offset to pixels, from the bitmap struct
//! };
//! ```
//!
//! Offsets in the C structs are relative to the start of the `spa_meta_cursor`
//! region (for `bitmap_offset`) and to the start of the `spa_meta_bitmap`
//! (for the bitmap's pixel `offset`).
//!
//! Extracting the meta region from a live PipeWire buffer requires a
//! buffer-metadata accessor that the `pipewire`/`libspa` 0.8 Rust bindings do
//! not yet expose (only data planes are reachable via `Buffer::datas_mut`).
//! Once such an accessor lands (or via a small `libspa-sys` shim), the capture
//! loop can feed the meta bytes straight into [`parse_spa_meta_cursor`].

use flux_core::cursor::{CursorBitmap, CursorMetadata};

const CURSOR_HEADER_LEN: usize = 28;
const BITMAP_HEADER_LEN: usize = 20;

fn read_u32(buf: &[u8], at: usize) -> Option<u32> {
    buf.get(at..at + 4)?.try_into().ok().map(u32::from_ne_bytes)
}

fn read_i32(buf: &[u8], at: usize) -> Option<i32> {
    buf.get(at..at + 4)?.try_into().ok().map(i32::from_ne_bytes)
}

/// Decode a `spa_meta_cursor` byte region into [`CursorMetadata`].
///
/// Returns `None` only when `buf` is too small to contain the fixed
/// `spa_meta_cursor` header. An `id` of `0` is a valid "cursor cleared" update
/// and decodes to [`CursorMetadata::hidden`]. A bitmap is decoded only when
/// `bitmap_offset` is non-zero and the referenced bitmap header and pixels lie
/// within `buf`; a malformed/out-of-bounds bitmap is dropped (position is still
/// reported) rather than failing the whole parse.
pub fn parse_spa_meta_cursor(buf: &[u8]) -> Option<CursorMetadata> {
    if buf.len() < CURSOR_HEADER_LEN {
        return None;
    }

    let id = read_u32(buf, 0)?;
    if id == 0 {
        return Some(CursorMetadata::hidden());
    }

    let position = (read_i32(buf, 8)?, read_i32(buf, 12)?);
    let hotspot = (read_i32(buf, 16)?, read_i32(buf, 20)?);
    let bitmap_offset = read_u32(buf, 24)? as usize;

    let bitmap = if bitmap_offset == 0 {
        None
    } else {
        parse_bitmap(buf, bitmap_offset)
    };

    Some(CursorMetadata {
        position: Some(position),
        hotspot,
        bitmap,
    })
}

/// Decode a `spa_meta_bitmap` at `base` within `buf`, returning `None` if it is
/// out of bounds, has an invalid format, or its pixel span exceeds `buf`.
fn parse_bitmap(buf: &[u8], base: usize) -> Option<CursorBitmap> {
    let header = buf.get(base..base.checked_add(BITMAP_HEADER_LEN)?)?;

    let format = read_u32(header, 0)?;
    if format == 0 {
        return None;
    }
    let width = read_u32(header, 4)?;
    let height = read_u32(header, 8)?;
    let stride = read_i32(header, 12)?;
    let pixels_offset = read_u32(header, 16)? as usize;

    if stride < 0 {
        return None;
    }
    let stride = stride as u32;

    let pixels_len = (stride as usize).checked_mul(height as usize)?;
    let pixels_start = base.checked_add(pixels_offset)?;
    let pixels_end = pixels_start.checked_add(pixels_len)?;
    let pixels = buf.get(pixels_start..pixels_end)?.to_vec();

    Some(CursorBitmap {
        width,
        height,
        stride,
        format,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `spa_meta_cursor` region (host-endian), optionally followed by a
    /// `spa_meta_bitmap` + pixels placed immediately after the 28-byte header.
    fn cursor_bytes(id: u32, pos: (i32, i32), hotspot: (i32, i32), bitmap: Option<(u32, u32, u32, i32, &[u8])>) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&id.to_ne_bytes());
        b.extend_from_slice(&0u32.to_ne_bytes()); // flags
        b.extend_from_slice(&pos.0.to_ne_bytes());
        b.extend_from_slice(&pos.1.to_ne_bytes());
        b.extend_from_slice(&hotspot.0.to_ne_bytes());
        b.extend_from_slice(&hotspot.1.to_ne_bytes());

        match bitmap {
            None => b.extend_from_slice(&0u32.to_ne_bytes()), // bitmap_offset = 0
            Some((format, w, h, stride, pixels)) => {
                let bitmap_offset = CURSOR_HEADER_LEN as u32;
                b.extend_from_slice(&bitmap_offset.to_ne_bytes());
                // spa_meta_bitmap header (pixels placed right after it).
                b.extend_from_slice(&format.to_ne_bytes());
                b.extend_from_slice(&w.to_ne_bytes());
                b.extend_from_slice(&h.to_ne_bytes());
                b.extend_from_slice(&stride.to_ne_bytes());
                b.extend_from_slice(&(BITMAP_HEADER_LEN as u32).to_ne_bytes()); // pixels offset
                b.extend_from_slice(pixels);
            }
        }
        b
    }

    #[test]
    fn too_small_returns_none() {
        assert!(parse_spa_meta_cursor(&[0u8; 8]).is_none());
    }

    #[test]
    fn id_zero_is_hidden() {
        let buf = cursor_bytes(0, (10, 20), (0, 0), None);
        assert_eq!(parse_spa_meta_cursor(&buf), Some(CursorMetadata::hidden()));
    }

    #[test]
    fn position_only_update() {
        let buf = cursor_bytes(7, (640, 480), (2, 3), None);
        let c = parse_spa_meta_cursor(&buf).unwrap();
        assert_eq!(c.position, Some((640, 480)));
        assert_eq!(c.hotspot, (2, 3));
        assert!(c.bitmap.is_none());
    }

    #[test]
    fn decodes_bitmap_pixels() {
        // 2x2 BGRA cursor, stride 8 (4 bytes/px * 2).
        let pixels: Vec<u8> = (0..16).collect();
        let buf = cursor_bytes(1, (0, 0), (1, 1), Some((/*BGRA*/ 12, 2, 2, 8, &pixels)));
        let c = parse_spa_meta_cursor(&buf).unwrap();
        let bmp = c.bitmap.expect("bitmap decoded");
        assert_eq!((bmp.width, bmp.height, bmp.stride, bmp.format), (2, 2, 8, 12));
        assert_eq!(bmp.pixels, pixels);
    }

    #[test]
    fn invalid_bitmap_format_drops_bitmap_but_keeps_position() {
        let buf = cursor_bytes(1, (5, 5), (0, 0), Some((0, 2, 2, 8, &[0u8; 16])));
        let c = parse_spa_meta_cursor(&buf).unwrap();
        assert_eq!(c.position, Some((5, 5)));
        assert!(c.bitmap.is_none());
    }

    #[test]
    fn out_of_bounds_pixels_drop_bitmap() {
        // Claim a 2x2 bitmap (needs 16 px bytes) but supply only 4.
        let buf = cursor_bytes(1, (5, 5), (0, 0), Some((12, 2, 2, 8, &[0u8; 4])));
        let c = parse_spa_meta_cursor(&buf).unwrap();
        assert_eq!(c.position, Some((5, 5)));
        assert!(c.bitmap.is_none());
    }

    #[test]
    fn negative_stride_drops_bitmap() {
        let buf = cursor_bytes(1, (5, 5), (0, 0), Some((12, 2, 2, -8, &[0u8; 16])));
        let c = parse_spa_meta_cursor(&buf).unwrap();
        assert!(c.bitmap.is_none());
    }
}
