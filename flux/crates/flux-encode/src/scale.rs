//! CPU frame downscaling for encoders with a maximum coded resolution.
//!
//! Hardware encoders commonly cap H.264 at 4096x4096 (e.g. AMD VCN), which is
//! smaller than wide desktop resolutions like 5120x2160. `fit_within` computes
//! an aspect-preserving target that the encoder accepts, and `downscale_frame`
//! resizes a packed 32-bit RGB frame to it with fixed-point bilinear filtering.

use flux_core::error::{FluxError, Result};
use flux_core::frame::CapturedFrame;
use flux_core::types::{PixelFormat, Resolution};

/// Fixed-point fractional bits for bilinear weights.
const FP_BITS: u32 = 8;
const FP_ONE: u32 = 1 << FP_BITS;

fn err(frame: &CapturedFrame, reason: String) -> FluxError {
    FluxError::Encode {
        frame: frame.sequence,
        reason: format!("downscale: {reason}"),
    }
}

/// Largest resolution that fits inside `max` while preserving `src`'s aspect
/// ratio. Never upscales; dimensions are rounded down to even values (4:2:0
/// chroma subsampling requires even luma dimensions).
pub fn fit_within(src: Resolution, max: Resolution) -> Resolution {
    if src.width == 0 || src.height == 0 {
        return src;
    }
    if src.width <= max.width && src.height <= max.height {
        return src;
    }
    // Scale = min(max.w/src.w, max.h/src.h), applied in u64 to avoid overflow.
    let (num, den) = if (max.width as u64) * (src.height as u64) <= (max.height as u64) * (src.width as u64) {
        (max.width as u64, src.width as u64)
    } else {
        (max.height as u64, src.height as u64)
    };
    let width = ((src.width as u64 * num / den) & !1) as u32;
    let height = ((src.height as u64 * num / den) & !1) as u32;
    Resolution::new(width.max(2), height.max(2))
}

/// Downscale a packed 32-bit (BGRA/RGBA) CPU frame to `target` using bilinear
/// filtering. Returns the frame unchanged (cloned metadata, no copy avoided)
/// only when the resolutions already match — callers should skip the call in
/// that case.
pub fn downscale_frame(frame: &CapturedFrame, target: Resolution) -> Result<CapturedFrame> {
    if frame.resolution == target {
        return Ok(frame.clone());
    }
    if !matches!(frame.format, PixelFormat::Bgra8 | PixelFormat::Rgba8) {
        return Err(err(
            frame,
            format!("unsupported pixel format {:?} (expected BGRA/RGBA)", frame.format),
        ));
    }
    if frame.data.is_empty() {
        return Err(err(
            frame,
            "frame has no CPU pixel data (GPU-only frames unsupported)".into(),
        ));
    }
    let (sw, sh) = (frame.resolution.width as usize, frame.resolution.height as usize);
    let (dw, dh) = (target.width as usize, target.height as usize);
    if dw == 0 || dh == 0 || dw > sw || dh > sh {
        return Err(err(
            frame,
            format!("invalid target {} for source {}", target, frame.resolution),
        ));
    }
    let stride = frame.stride as usize;
    if frame.data.len() < stride * sh || stride < sw * 4 {
        return Err(err(
            frame,
            format!(
                "source buffer too small ({} bytes for {} stride {})",
                frame.data.len(),
                frame.resolution,
                stride
            ),
        ));
    }

    // Precompute per-column source offsets and weights (fixed-point).
    let x_ratio = ((sw - 1) as u64 * FP_ONE as u64 / dw.max(1) as u64) as u32;
    let y_ratio = ((sh - 1) as u64 * FP_ONE as u64 / dh.max(1) as u64) as u32;
    let xs: Vec<(usize, u32)> = (0..dw)
        .map(|dx| {
            let fx = dx as u64 * x_ratio as u64;
            (
                ((fx >> FP_BITS) as usize).min(sw - 2),
                (fx & (FP_ONE as u64 - 1)) as u32,
            )
        })
        .collect();

    // Separable filter: interpolate each needed source row horizontally once
    // (exact in u16), then blend two such rows vertically per output row.
    // Consecutive output rows usually share a source row, so the horizontal
    // pass for `sy + 1` is reused as the next row's `sy`.
    let row_len = dw * 4;
    let mut out = vec![0u8; row_len * dh];
    let mut top = vec![0u16; row_len];
    let mut bot = vec![0u16; row_len];
    let (mut top_row, mut bot_row) = (usize::MAX, usize::MAX);
    let src_row = |y: usize| &frame.data[y * stride..y * stride + sw * 4];
    for (dy, dst) in out.chunks_exact_mut(row_len).enumerate() {
        let fy = dy as u64 * y_ratio as u64;
        let sy = ((fy >> FP_BITS) as usize).min(sh - 2);
        let wy = (fy & (FP_ONE as u64 - 1)) as u32;
        if top_row != sy {
            if bot_row == sy {
                std::mem::swap(&mut top, &mut bot);
                bot_row = usize::MAX;
            } else {
                interpolate_row(src_row(sy), &xs, &mut top);
            }
            top_row = sy;
        }
        if bot_row != sy + 1 {
            interpolate_row(src_row(sy + 1), &xs, &mut bot);
            bot_row = sy + 1;
        }
        blend_rows(&top, &bot, wy, dst);
    }

    Ok(CapturedFrame {
        sequence: frame.sequence,
        timestamp: frame.timestamp,
        format: frame.format,
        resolution: target,
        stride: (dw * 4) as u32,
        data: out,
        gpu_handle: None,
    })
}

/// Horizontal bilinear pass over one source row into `FP_BITS`-scaled
/// intermediates. `p0 * (FP_ONE - wx) + p1 * wx <= 255 * FP_ONE` fits in u16.
fn interpolate_row(src: &[u8], xs: &[(usize, u32)], dst: &mut [u16]) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: SSE2 is part of the x86_64 baseline target features.
    unsafe {
        interpolate_row_sse2(src, xs, dst)
    }
    #[cfg(not(target_arch = "x86_64"))]
    interpolate_row_scalar(src, xs, dst)
}

/// Weights all 4 channels of a source pixel pair with one 8-lane u16 multiply.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse2")]
fn interpolate_row_sse2(src: &[u8], xs: &[(usize, u32)], dst: &mut [u16]) {
    use std::arch::x86_64::*;
    for (d, &(sx, wx)) in dst.as_chunks_mut::<4>().0.iter_mut().zip(xs) {
        let o = sx * 4;
        let p: &[u8; 8] = src[o..o + 8].try_into().expect("8-byte pixel pair");
        let (w0, w1) = ((FP_ONE - wx) as i16, wx as i16);
        let v = _mm_unpacklo_epi8(_mm_cvtsi64_si128(i64::from_le_bytes(*p)), _mm_setzero_si128());
        let m = _mm_mullo_epi16(v, _mm_set_epi16(w1, w1, w1, w1, w0, w0, w0, w0));
        let r = _mm_cvtsi128_si64(_mm_add_epi16(m, _mm_srli_si128::<8>(m))) as u64;
        *d = [r as u16, (r >> 16) as u16, (r >> 32) as u16, (r >> 48) as u16];
    }
}

#[cfg(any(test, not(target_arch = "x86_64")))]
fn interpolate_row_scalar(src: &[u8], xs: &[(usize, u32)], dst: &mut [u16]) {
    for (d, &(sx, wx)) in dst.as_chunks_mut::<4>().0.iter_mut().zip(xs) {
        let o = sx * 4;
        let p: &[u8; 8] = src[o..o + 8].try_into().expect("8-byte pixel pair");
        let (w0, w1) = ((FP_ONE - wx) as u16, wx as u16);
        *d = std::array::from_fn(|c| p[c] as u16 * w0 + p[c + 4] as u16 * w1);
    }
}

/// Vertical bilinear pass blending two horizontally-interpolated rows.
fn blend_rows(top: &[u16], bot: &[u16], wy: u32, dst: &mut [u8]) {
    let (w0, w1) = (FP_ONE - wy, wy);
    for ((d, &t), &b) in dst.iter_mut().zip(top).zip(bot) {
        *d = ((t as u32 * w0 + b as u32 * w1) >> (2 * FP_BITS)) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_frame(w: u32, h: u32, px: [u8; 4]) -> CapturedFrame {
        CapturedFrame {
            sequence: 0,
            timestamp: std::time::Instant::now(),
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(w, h),
            stride: w * 4,
            data: px.repeat((w * h) as usize),
            gpu_handle: None,
        }
    }

    #[test]
    fn fit_within_no_change_when_smaller() {
        let r = fit_within(Resolution::new(1920, 1080), Resolution::new(4096, 4096));
        assert_eq!(r, Resolution::new(1920, 1080));
    }

    #[test]
    fn fit_within_preserves_aspect_and_evenness() {
        let r = fit_within(Resolution::new(5120, 2160), Resolution::new(4096, 4096));
        assert_eq!(r, Resolution::new(4096, 1728));
        assert_eq!(r.width % 2, 0);
        assert_eq!(r.height % 2, 0);
    }

    #[test]
    fn fit_within_height_limited() {
        let r = fit_within(Resolution::new(2160, 5120), Resolution::new(4096, 4096));
        assert_eq!(r, Resolution::new(1728, 4096));
    }

    #[test]
    fn downscale_preserves_solid_color() {
        let frame = solid_frame(64, 64, [10, 200, 30, 255]);
        let out = downscale_frame(&frame, Resolution::new(32, 32)).unwrap();
        assert_eq!(out.resolution, Resolution::new(32, 32));
        assert_eq!(out.stride, 32 * 4);
        assert!(out.data.as_chunks::<4>().0.iter().all(|p| *p == [10, 200, 30, 255]));
    }

    #[test]
    fn downscale_rejects_planar_formats() {
        let mut frame = solid_frame(64, 64, [0; 4]);
        frame.format = PixelFormat::Nv12;
        assert!(downscale_frame(&frame, Resolution::new(32, 32)).is_err());
    }

    #[test]
    fn downscale_rejects_gpu_only_frames() {
        let mut frame = solid_frame(64, 64, [0; 4]);
        frame.data.clear();
        assert!(downscale_frame(&frame, Resolution::new(32, 32)).is_err());
    }

    #[test]
    fn downscale_rejects_upscale() {
        let frame = solid_frame(32, 32, [0; 4]);
        assert!(downscale_frame(&frame, Resolution::new(64, 64)).is_err());
    }

    /// Original per-pixel scalar bilinear, kept as the bit-exact oracle.
    fn reference_downscale(frame: &CapturedFrame, target: Resolution) -> Vec<u8> {
        let (sw, sh) = (frame.resolution.width as usize, frame.resolution.height as usize);
        let (dw, dh) = (target.width as usize, target.height as usize);
        let stride = frame.stride as usize;
        let x_ratio = ((sw - 1) as u64 * FP_ONE as u64 / dw as u64) as u32;
        let y_ratio = ((sh - 1) as u64 * FP_ONE as u64 / dh as u64) as u32;
        let mut out = vec![0u8; dw * dh * 4];
        for dy in 0..dh {
            let fy = dy as u64 * y_ratio as u64;
            let sy = ((fy >> FP_BITS) as usize).min(sh - 2);
            let wy = (fy & (FP_ONE as u64 - 1)) as u32;
            for dx in 0..dw {
                let fx = dx as u64 * x_ratio as u64;
                let sx = ((fx >> FP_BITS) as usize).min(sw - 2);
                let wx = (fx & (FP_ONE as u64 - 1)) as u32;
                for c in 0..4 {
                    let px = |y: usize, x: usize| frame.data[y * stride + x * 4 + c] as u32;
                    let top = px(sy, sx) * (FP_ONE - wx) + px(sy, sx + 1) * wx;
                    let bot = px(sy + 1, sx) * (FP_ONE - wx) + px(sy + 1, sx + 1) * wx;
                    out[(dy * dw + dx) * 4 + c] = ((top * (FP_ONE - wy) + bot * wy) >> (2 * FP_BITS)) as u8;
                }
            }
        }
        out
    }

    fn noise_frame(w: u32, h: u32, stride: u32, seed: u64) -> CapturedFrame {
        let mut state = seed | 1;
        let data = (0..stride as usize * h as usize)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect();
        CapturedFrame {
            sequence: 0,
            timestamp: std::time::Instant::now(),
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(w, h),
            stride,
            data,
            gpu_handle: None,
        }
    }

    #[test]
    fn downscale_matches_reference_bilinear() {
        let cases = [
            ((5120, 2160, 5120 * 4), (4096, 1728)),
            ((64, 64, 64 * 4 + 32), (31, 17)),
            ((33, 17, 33 * 4), (20, 9)),
            ((100, 50, 100 * 4), (100, 25)),
            ((100, 50, 104 * 4), (51, 50)),
            ((2, 4, 2 * 4), (2, 2)),
        ];
        for (i, ((sw, sh, stride), (dw, dh))) in cases.into_iter().enumerate() {
            let frame = noise_frame(sw, sh, stride, i as u64 + 1);
            let target = Resolution::new(dw, dh);
            let out = downscale_frame(&frame, target).unwrap();
            assert_eq!(out.data, reference_downscale(&frame, target), "case {i}");
        }
    }

    #[test]
    fn interpolate_row_kernels_agree() {
        let frame = noise_frame(257, 1, 257 * 4, 9);
        let xs: Vec<(usize, u32)> = (0..200)
            .map(|dx| ((dx * 5 / 4).min(255), (dx * 37 % 256) as u32))
            .collect();
        let (mut a, mut b) = (vec![0u16; 800], vec![0u16; 800]);
        interpolate_row(&frame.data, &xs, &mut a);
        interpolate_row_scalar(&frame.data, &xs, &mut b);
        assert_eq!(a, b);
    }
}
