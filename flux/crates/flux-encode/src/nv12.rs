//! CPU pixel-format conversion to packed NV12.
//!
//! Shared by the hardware backends' CPU-upload paths (VA-API, Vulkan Video):
//! a [`CapturedFrame`] carrying SHM pixel data is converted to a tightly
//! packed NV12 buffer (`w*h` luma followed by `w*h/2` interleaved chroma)
//! ready to upload into a GPU surface. DMA-BUF zero-copy frames bypass this.

use flux_core::error::{FluxError, Result};
use flux_core::frame::CapturedFrame;
use flux_core::types::PixelFormat;

/// Convert a [`CapturedFrame`] to a packed NV12 buffer. Only CPU-resident
/// (SHM) frames are supported on this path; DMA-BUF zero-copy import is a
/// later step.
pub(crate) fn frame_to_nv12(frame: &CapturedFrame, width: u32, height: u32) -> Result<Vec<u8>> {
    if frame.gpu_handle.is_some() && frame.data.is_empty() {
        return Err(FluxError::Encode {
            frame: frame.sequence,
            reason: "CPU upload path needs CPU pixel data; DMA-BUF import not yet wired".into(),
        });
    }
    if frame.data.is_empty() {
        return Err(FluxError::Encode {
            frame: frame.sequence,
            reason: "frame has no CPU pixel data to upload".into(),
        });
    }
    if frame.resolution.width != width || frame.resolution.height != height {
        return Err(FluxError::Encode {
            frame: frame.sequence,
            reason: format!(
                "frame resolution {} does not match encoder {}x{}",
                frame.resolution, width, height
            ),
        });
    }

    let w = width as usize;
    let h = height as usize;
    let stride = frame.stride as usize;

    match frame.format {
        PixelFormat::Bgra8 => packed_rgb_to_nv12(&frame.data, stride, w, h, ChannelOrder::Bgra),
        PixelFormat::Rgba8 => packed_rgb_to_nv12(&frame.data, stride, w, h, ChannelOrder::Rgba),
        PixelFormat::Nv12 => passthrough_nv12(&frame.data, stride, w, h),
        PixelFormat::I420 => i420_to_nv12(&frame.data, stride, w, h),
        PixelFormat::P010 => Err("P010/10-bit input is not supported until the HDR phase".to_string()),
    }
    .map_err(|reason| FluxError::Encode {
        frame: frame.sequence,
        reason,
    })
}

#[derive(Clone, Copy)]
pub(crate) enum ChannelOrder {
    Bgra,
    Rgba,
}

/// BT.601 limited-range RGB→NV12 with 2×2 chroma box averaging.
pub(crate) fn packed_rgb_to_nv12(
    data: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    order: ChannelOrder,
) -> std::result::Result<Vec<u8>, String> {
    let row_bytes = w * 4;
    if stride < row_bytes || data.len() < stride * h {
        return Err(format!(
            "RGB buffer too small: {} bytes, need stride {} * height {}",
            data.len(),
            stride,
            h
        ));
    }

    let mut nv12 = vec![0u8; w * h + w * (h / 2)];
    let (y_plane, uv_plane) = nv12.split_at_mut(w * h);

    match order {
        ChannelOrder::Bgra => convert_rgb_rows::<2, 1, 0>(data, stride, w, h, y_plane, uv_plane),
        ChannelOrder::Rgba => convert_rgb_rows::<0, 1, 2>(data, stride, w, h, y_plane, uv_plane),
    }

    Ok(nv12)
}

/// Single pass over the source: each 2×2 block is loaded once and feeds both
/// its four luma samples and its averaged chroma pair. A trailing odd column
/// or row only contributes luma.
fn convert_rgb_rows<const RO: usize, const GO: usize, const BO: usize>(
    data: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    y_plane: &mut [u8],
    uv_plane: &mut [u8],
) {
    let row_bytes = w * 4;
    let cw = w / 2;
    let px = |p: &[u8]| (p[RO] as i32, p[GO] as i32, p[BO] as i32);

    for cy in 0..(h / 2) {
        let top = &data[2 * cy * stride..][..row_bytes];
        let bottom = &data[(2 * cy + 1) * stride..][..row_bytes];
        let (y_top, y_bottom) = y_plane[2 * cy * w..][..2 * w].split_at_mut(w);
        let uv_row = &mut uv_plane[cy * w..][..2 * cw];

        let blocks = top
            .chunks_exact(8)
            .zip(bottom.chunks_exact(8))
            .zip(y_top.chunks_exact_mut(2))
            .zip(y_bottom.chunks_exact_mut(2))
            .zip(uv_row.chunks_exact_mut(2));
        for ((((t, b), yt), yb), uv) in blocks {
            let (r0, g0, b0) = px(&t[..4]);
            let (r1, g1, b1) = px(&t[4..]);
            let (r2, g2, b2) = px(&b[..4]);
            let (r3, g3, b3) = px(&b[4..]);
            yt[0] = bt601_luma(r0, g0, b0);
            yt[1] = bt601_luma(r1, g1, b1);
            yb[0] = bt601_luma(r2, g2, b2);
            yb[1] = bt601_luma(r3, g3, b3);

            // Chroma is subsampled 2×2; average the block to reduce aliasing.
            let r = (r0 + r1 + r2 + r3) / 4;
            let g = (g0 + g1 + g2 + g3) / 4;
            let b = (b0 + b1 + b2 + b3) / 4;
            uv[0] = bt601_u(r, g, b);
            uv[1] = bt601_v(r, g, b);
        }

        if w % 2 == 1 {
            let x = w - 1;
            let (r, g, b) = px(&top[x * 4..]);
            y_top[x] = bt601_luma(r, g, b);
            let (r, g, b) = px(&bottom[x * 4..]);
            y_bottom[x] = bt601_luma(r, g, b);
        }
    }

    if h % 2 == 1 {
        let y = h - 1;
        let src = &data[y * stride..][..row_bytes];
        for (p, out) in src.chunks_exact(4).zip(&mut y_plane[y * w..][..w]) {
            let (r, g, b) = px(p);
            *out = bt601_luma(r, g, b);
        }
    }
}

pub(crate) fn bt601_luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

pub(crate) fn bt601_u(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

pub(crate) fn bt601_v(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

/// Repack possibly-padded NV12 into a tightly packed `w*h*3/2` buffer.
pub(crate) fn passthrough_nv12(data: &[u8], stride: usize, w: usize, h: usize) -> std::result::Result<Vec<u8>, String> {
    let stride = stride.max(w);
    if data.len() < stride * h + stride * (h / 2) {
        return Err(format!("NV12 buffer too small: {} bytes", data.len()));
    }
    let mut out = vec![0u8; w * h + w * (h / 2)];
    for row in 0..h {
        out[row * w..row * w + w].copy_from_slice(&data[row * stride..row * stride + w]);
    }
    let uv_src = stride * h;
    let uv_dst = w * h;
    for row in 0..(h / 2) {
        out[uv_dst + row * w..uv_dst + row * w + w]
            .copy_from_slice(&data[uv_src + row * stride..uv_src + row * stride + w]);
    }
    Ok(out)
}

/// Convert planar I420 (Y, then U, then V) into interleaved NV12.
pub(crate) fn i420_to_nv12(data: &[u8], stride: usize, w: usize, h: usize) -> std::result::Result<Vec<u8>, String> {
    let stride = stride.max(w);
    let cw = w / 2;
    let ch = h / 2;
    let y_size = stride * h;
    let c_stride = stride / 2;
    let c_size = c_stride * ch;
    if data.len() < y_size + 2 * c_size {
        return Err(format!("I420 buffer too small: {} bytes", data.len()));
    }

    let mut out = vec![0u8; w * h + w * (h / 2)];
    for row in 0..h {
        out[row * w..row * w + w].copy_from_slice(&data[row * stride..row * stride + w]);
    }

    let u_base = y_size;
    let v_base = y_size + c_size;
    let uv_dst = w * h;
    for row in 0..ch {
        for col in 0..cw {
            let u = data[u_base + row * c_stride + col];
            let v = data[v_base + row * c_stride + col];
            let idx = uv_dst + row * w + col * 2;
            out[idx] = u;
            out[idx + 1] = v;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bt601_reference_values() {
        // Black and white land on the BT.601 limited-range endpoints.
        assert_eq!(bt601_luma(0, 0, 0), 16);
        assert_eq!(bt601_luma(255, 255, 255), 235);
        assert_eq!(bt601_u(0, 0, 0), 128);
        assert_eq!(bt601_v(0, 0, 0), 128);
        // Pure red is a chroma extreme.
        assert!(bt601_v(255, 0, 0) > 200);
        assert!(bt601_u(0, 0, 255) > 200);
    }

    #[test]
    fn bgra_solid_color_to_nv12() {
        let (w, h) = (4usize, 4usize);
        let stride = w * 4;
        // Solid opaque red in BGRA byte order: B=0 G=0 R=255 A=255.
        let mut data = vec![0u8; stride * h];
        for px in data.chunks_mut(4) {
            px[0] = 0;
            px[1] = 0;
            px[2] = 255;
            px[3] = 255;
        }
        let nv12 = packed_rgb_to_nv12(&data, stride, w, h, ChannelOrder::Bgra).unwrap();
        assert_eq!(nv12.len(), w * h + w * (h / 2));

        let y = bt601_luma(255, 0, 0);
        assert!(nv12[..w * h].iter().all(|&p| p == y));

        let u = bt601_u(255, 0, 0);
        let v = bt601_v(255, 0, 0);
        for pair in nv12[w * h..].chunks(2) {
            assert_eq!(pair[0], u);
            assert_eq!(pair[1], v);
        }
    }

    #[test]
    fn rgba_and_bgra_channel_orders_differ_for_red() {
        let (w, h) = (2usize, 2usize);
        let stride = w * 4;
        let mut rgba = vec![0u8; stride * h];
        for px in rgba.chunks_mut(4) {
            px[0] = 255; // R
            px[3] = 255; // A
        }
        let from_rgba = packed_rgb_to_nv12(&rgba, stride, w, h, ChannelOrder::Rgba).unwrap();
        // Same bytes, but interpreted as BGRA means the 255 is the blue channel.
        let from_bgra = packed_rgb_to_nv12(&rgba, stride, w, h, ChannelOrder::Bgra).unwrap();
        assert_ne!(from_rgba[0], from_bgra[0]);
    }

    /// The original two-pass conversion, kept as an oracle for the fused one.
    fn two_pass_reference(data: &[u8], stride: usize, w: usize, h: usize, order: ChannelOrder) -> Vec<u8> {
        let mut nv12 = vec![0u8; w * h + w * (h / 2)];
        let (y_plane, uv_plane) = nv12.split_at_mut(w * h);
        let (ro, go, bo) = match order {
            ChannelOrder::Bgra => (2usize, 1usize, 0usize),
            ChannelOrder::Rgba => (0usize, 1usize, 2usize),
        };
        let px = |x: usize, y: usize| -> (i32, i32, i32) {
            let base = y * stride + x * 4;
            (data[base + ro] as i32, data[base + go] as i32, data[base + bo] as i32)
        };
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = px(x, y);
                y_plane[y * w + x] = bt601_luma(r, g, b);
            }
        }
        for cy in 0..(h / 2) {
            for cx in 0..(w / 2) {
                let (mut rs, mut gs, mut bs) = (0i32, 0i32, 0i32);
                for dy in 0..2 {
                    for dx in 0..2 {
                        let (r, g, b) = px(cx * 2 + dx, cy * 2 + dy);
                        rs += r;
                        gs += g;
                        bs += b;
                    }
                }
                let (r, g, b) = (rs / 4, gs / 4, bs / 4);
                let idx = cy * w + cx * 2;
                uv_plane[idx] = bt601_u(r, g, b);
                uv_plane[idx + 1] = bt601_v(r, g, b);
            }
        }
        nv12
    }

    #[test]
    fn fused_conversion_matches_two_pass_reference() {
        let mut seed = 0x9e37_79b9_u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as u8
        };
        for &(w, h) in &[(1, 1), (1, 2), (2, 1), (2, 2), (3, 3), (5, 4), (4, 5), (7, 9), (64, 36)] {
            for pad in [0usize, 4, 12] {
                let stride = w * 4 + pad;
                let data: Vec<u8> = (0..stride * h).map(|_| next()).collect();
                for order in [ChannelOrder::Bgra, ChannelOrder::Rgba] {
                    let fused = packed_rgb_to_nv12(&data, stride, w, h, order).unwrap();
                    assert_eq!(
                        fused,
                        two_pass_reference(&data, stride, w, h, order),
                        "{w}x{h} pad {pad}"
                    );
                }
            }
        }
    }

    #[test]
    fn i420_interleaves_chroma() {
        let (w, h) = (2usize, 2usize);
        // Y(4) + U(1) + V(1) for a 2x2 frame.
        let data = vec![10, 11, 12, 13, /*U*/ 200, /*V*/ 50];
        let nv12 = i420_to_nv12(&data, w, w, h).unwrap();
        assert_eq!(&nv12[..4], &[10, 11, 12, 13]);
        assert_eq!(nv12[4], 200); // U
        assert_eq!(nv12[5], 50); // V
    }

    #[test]
    fn passthrough_nv12_strips_padding() {
        let (w, h) = (2usize, 2usize);
        let stride = 4; // padded
                        // Y rows (stride 4, width 2) then UV row.
        let data = vec![
            1, 2, 0, 0, // Y row 0
            3, 4, 0, 0, // Y row 1
            5, 6, 0, 0, // UV row 0
        ];
        let nv12 = passthrough_nv12(&data, stride, w, h).unwrap();
        assert_eq!(nv12, vec![1, 2, 3, 4, 5, 6]);
    }
}
