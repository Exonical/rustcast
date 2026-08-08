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

    let mut out = vec![0u8; dw * dh * 4];
    for dy in 0..dh {
        let fy = dy as u64 * y_ratio as u64;
        let sy = ((fy >> FP_BITS) as usize).min(sh - 2);
        let wy = (fy & (FP_ONE as u64 - 1)) as u32;
        let row0 = &frame.data[sy * stride..sy * stride + sw * 4];
        let row1 = &frame.data[(sy + 1) * stride..(sy + 1) * stride + sw * 4];
        let dst = &mut out[dy * dw * 4..(dy + 1) * dw * 4];
        for (dx, &(sx, wx)) in xs.iter().enumerate() {
            let o = sx * 4;
            for c in 0..4 {
                let p00 = row0[o + c] as u32;
                let p01 = row0[o + 4 + c] as u32;
                let p10 = row1[o + c] as u32;
                let p11 = row1[o + 4 + c] as u32;
                let top = p00 * (FP_ONE - wx) + p01 * wx;
                let bot = p10 * (FP_ONE - wx) + p11 * wx;
                let val = (top * (FP_ONE - wy) + bot * wy) >> (2 * FP_BITS);
                dst[dx * 4 + c] = val as u8;
            }
        }
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
}
