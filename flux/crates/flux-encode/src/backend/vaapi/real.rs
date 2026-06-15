//! Real VA-API H.264 encoder.
//!
//! Built on [`cros-codecs`] (H.264 bitstream synthesis + VA encode pipeline)
//! and [`cros-libva`] (safe libva wrappers). The libva `Display` and the
//! `cros-codecs` encoder are `!Send` (they hold `Rc`s), so — exactly like the
//! PipeWire capture loop — the whole VA pipeline runs on one dedicated thread
//! and the public [`EncodeSession`] talks to it over channels.
//!
//! Phase 2 scope (matches the roadmap): VA-API init/probe + H.264 encode of
//! SHM-uploaded NV12 surfaces, producing a real Annex-B bitstream. Zero-copy
//! DMA-BUF import and on-GPU colour conversion (VPP) are deliberately left for
//! a later step; CPU frames are converted to NV12 and uploaded via `vaPutImage`.

use std::borrow::Borrow;
use std::rc::Rc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, unbounded, Sender};

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, EncodedPacket};
use flux_core::types::{PixelFormat, Resolution, VideoCodec};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

use cros_codecs::backend::vaapi::surface_pool::{PooledVaSurface, VaSurfacePool};
use cros_codecs::decoder::FramePool;
use cros_codecs::encoder::h264::EncoderConfig as H264Config;
use cros_codecs::encoder::stateless::h264;
use cros_codecs::encoder::{FrameMetadata, PredictionStructure, RateControl, Tunings, VideoEncoder as CcEncoder};
use cros_codecs::{BlockingMode, Fourcc, FrameLayout, PlaneLayout, Resolution as CcResolution};
use libva::{Display, Image, Surface, UsageHint, VAEntrypoint, VAProfile};

/// Number of NV12 surfaces kept in the encoder's input pool.
const SURFACE_POOL_SIZE: usize = 16;

/// H.264 NAL unit type for an IDR (key) slice (`nal_unit_type == 5`).
const NAL_UNIT_TYPE_IDR: u8 = 5;

// ───────────────────────────── encoder ─────────────────────────────

/// VA-API hardware encoder (Linux, AMD/Intel via Mesa).
///
/// Construction probes the driver once (which encode entrypoints exist) and
/// caches the result; the transient `Display` is dropped immediately because it
/// is `!Send`. Each session opens its own `Display` on the encode thread.
pub struct VaapiEncoder {
    capabilities: EncoderCapabilities,
    driver: String,
}

impl VaapiEncoder {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing VA-API encoder (cros-codecs)");
        let display = open_display()?;
        let driver = display.query_vendor_string().unwrap_or_default().to_string();
        let capabilities = probe_capabilities(&display)?;
        tracing::info!(
            driver = %driver,
            codecs = ?capabilities.supported_codecs,
            "VA-API encoder ready"
        );
        // `display` (an `Rc`) is intentionally dropped here — it is `!Send` and
        // each session re-opens its own on the encode thread.
        drop(display);
        Ok(Self { capabilities, driver })
    }

    /// Driver / vendor string reported by `vaQueryVendorString`.
    pub fn driver(&self) -> &str {
        &self.driver
    }
}

impl VideoEncoder for VaapiEncoder {
    fn name(&self) -> &'static str {
        "VA-API"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Ok(self.capabilities.clone())
    }

    fn validate_config(&self, config: &EncodeConfig) -> Result<()> {
        if config.codec != VideoCodec::H264 {
            return Err(FluxError::EncoderInit(format!(
                "VA-API backend currently encodes H.264 only (requested {:?}); \
                 H.265/AV1 land in a later phase",
                config.codec
            )));
        }
        if !self.capabilities.supported_codecs.contains(&VideoCodec::H264) {
            return Err(FluxError::EncoderInit(
                "VA-API driver does not expose an H.264 encode entrypoint".into(),
            ));
        }
        let max = self.capabilities.max_resolution;
        if config.resolution.width > max.width || config.resolution.height > max.height {
            return Err(FluxError::EncoderInit(format!(
                "resolution {} exceeds VA-API maximum {}",
                config.resolution, max
            )));
        }
        Ok(())
    }

    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        self.validate_config(&config)?;
        Ok(Box::new(VaapiSession::spawn(config)?))
    }
}

// ───────────────────────────── session ─────────────────────────────

/// A command sent from [`VaapiSession`] to the encode thread.
enum Cmd {
    Encode {
        frame: Box<CapturedFrame>,
        reply: Sender<Result<Vec<EncodedPacket>>>,
    },
    RequestIdr,
    SetBitrate(u32),
    Flush {
        reply: Sender<Result<Vec<EncodedPacket>>>,
    },
    Stop,
}

/// Handle to a VA-API encode session running on its own thread.
pub struct VaapiSession {
    tx: Sender<Cmd>,
    handle: Option<JoinHandle<()>>,
}

impl VaapiSession {
    fn spawn(config: EncodeConfig) -> Result<Self> {
        let (tx, rx) = unbounded::<Cmd>();
        // Surface init failures synchronously so `create_session` can report them.
        let (ready_tx, ready_rx) = bounded::<Result<()>>(1);

        let handle = thread::Builder::new()
            .name("flux-vaapi-encode".into())
            .spawn(move || run_encode_thread(config, rx, ready_tx))
            .map_err(|e| FluxError::EncoderInit(format!("failed to spawn encode thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                handle: Some(handle),
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(FluxError::EncoderInit(
                    "encode thread exited before signalling readiness".into(),
                ))
            }
        }
    }

    fn request(&self, make: impl FnOnce(Sender<Result<Vec<EncodedPacket>>>) -> Cmd) -> Result<Vec<EncodedPacket>> {
        let (reply_tx, reply_rx) = bounded::<Result<Vec<EncodedPacket>>>(1);
        self.tx
            .send(make(reply_tx))
            .map_err(|_| FluxError::EncoderInit("encode thread is gone".into()))?;
        reply_rx
            .recv()
            .map_err(|_| FluxError::EncoderInit("encode thread dropped the reply".into()))?
    }
}

impl EncodeSession for VaapiSession {
    fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>> {
        let frame = Box::new(frame.clone());
        self.request(|reply| Cmd::Encode { frame, reply })
    }

    fn request_idr(&mut self) {
        let _ = self.tx.send(Cmd::RequestIdr);
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        self.request(|reply| Cmd::Flush { reply })
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        self.tx
            .send(Cmd::SetBitrate(bitrate_kbps))
            .map_err(|_| FluxError::EncoderInit("encode thread is gone".into()))
    }
}

impl Drop for VaapiSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ─────────────────────────── encode thread ───────────────────────────

fn run_encode_thread(config: EncodeConfig, rx: crossbeam_channel::Receiver<Cmd>, ready_tx: Sender<Result<()>>) {
    let mut state = match EncoderState::new(&config) {
        Ok(state) => {
            let _ = ready_tx.send(Ok(()));
            state
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Encode { frame, reply } => {
                let _ = reply.send(state.encode(&frame));
            }
            Cmd::Flush { reply } => {
                let _ = reply.send(state.flush());
            }
            Cmd::RequestIdr => state.force_idr = true,
            Cmd::SetBitrate(kbps) => {
                if let Err(e) = state.set_bitrate(kbps) {
                    tracing::warn!("VA-API set_bitrate failed: {e}");
                }
            }
            Cmd::Stop => break,
        }
    }
}

/// All `!Send` VA state, owned exclusively by the encode thread.
struct EncoderState {
    display: Rc<Display>,
    encoder: Box<dyn CcEncoder<PooledVaSurface<()>>>,
    pool: VaSurfacePool<()>,
    width: u32,
    height: u32,
    timestamp: u64,
    tunings: Tunings,
    force_idr: bool,
}

impl EncoderState {
    fn new(config: &EncodeConfig) -> Result<Self> {
        let width = config.resolution.width;
        let height = config.resolution.height;
        if width == 0 || height == 0 {
            return Err(FluxError::EncoderInit("encode resolution must be non-zero".into()));
        }

        let display = open_display()?;
        let coded = CcResolution { width, height };
        let tunings = low_latency_tunings(config);
        let low_power = pick_h264_low_power(&display)?;

        let h264_config = H264Config {
            resolution: coded,
            pred_structure: PredictionStructure::LowDelay {
                limit: gop_limit(config.gop_size),
            },
            initial_tunings: tunings.clone(),
            ..Default::default()
        };

        let fourcc: Fourcc = b"NV12".into();
        let encoder = h264::StatelessEncoder::new_vaapi(
            Rc::clone(&display),
            h264_config,
            fourcc,
            coded,
            low_power,
            BlockingMode::Blocking,
        )
        .map_err(|e| FluxError::EncoderInit(format!("VA-API H.264 encoder init failed: {e}")))?;

        let mut pool = VaSurfacePool::<()>::new(
            Rc::clone(&display),
            libva::constants::VA_RT_FORMAT_YUV420,
            Some(UsageHint::USAGE_HINT_ENCODER),
            coded,
        );
        pool.add_frames(vec![(); SURFACE_POOL_SIZE])
            .map_err(|e| FluxError::EncoderInit(format!("failed to allocate VA surfaces: {e}")))?;

        Ok(Self {
            display,
            encoder: Box::new(encoder),
            pool,
            width,
            height,
            timestamp: 0,
            tunings,
            force_idr: true,
        })
    }

    fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>> {
        let nv12 = frame_to_nv12(frame, self.width, self.height)?;

        let handle = self.pool.get_surface().ok_or_else(|| FluxError::Encode {
            frame: self.timestamp,
            reason: "no free VA surface in pool (encoder backpressure)".into(),
        })?;

        let layout = upload_nv12(&self.display, handle.borrow(), self.width, self.height, &nv12).map_err(|e| {
            FluxError::Encode {
                frame: self.timestamp,
                reason: e,
            }
        })?;

        let meta = FrameMetadata {
            timestamp: self.timestamp,
            layout,
            force_keyframe: std::mem::take(&mut self.force_idr),
        };
        self.timestamp += 1;

        self.encoder.encode(meta, handle).map_err(|e| FluxError::Encode {
            frame: self.timestamp,
            reason: e.to_string(),
        })?;

        let mut packets = Vec::new();
        self.collect(&mut packets)?;
        Ok(packets)
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        self.encoder.drain().map_err(|e| FluxError::Encode {
            frame: self.timestamp,
            reason: e.to_string(),
        })?;
        let mut packets = Vec::new();
        self.collect(&mut packets)?;
        Ok(packets)
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        self.tunings.rate_control = RateControl::ConstantBitrate(bits_per_sec(bitrate_kbps));
        self.encoder
            .tune(self.tunings.clone())
            .map_err(|e| FluxError::EncoderInit(format!("VA-API tune failed: {e}")))
    }

    /// Drain whatever coded bitstream the encoder has ready into `out`.
    fn collect(&mut self, out: &mut Vec<EncodedPacket>) -> Result<()> {
        while let Some(coded) = self.encoder.poll().map_err(|e| FluxError::Encode {
            frame: self.timestamp,
            reason: e.to_string(),
        })? {
            out.push(EncodedPacket {
                frame_index: coded.metadata.timestamp,
                pts: coded.metadata.timestamp,
                is_keyframe: bitstream_has_idr(&coded.bitstream),
                data: coded.bitstream,
            });
        }
        Ok(())
    }
}

// ───────────────────────── libva helpers ─────────────────────────

fn open_display() -> Result<Rc<Display>> {
    Display::open().ok_or_else(|| {
        FluxError::EncoderInit("could not open a VA-API display (no DRM render node at /dev/dri/render*?)".into())
    })
}

/// Build [`EncoderCapabilities`] from the driver's real profile/entrypoint set.
///
/// `cros-codecs` 0.0.5 only implements an H.264 VA encoder, so even if the
/// driver advertises HEVC/AV1 encode we only report what we can actually drive.
fn probe_capabilities(display: &Rc<Display>) -> Result<EncoderCapabilities> {
    let h264 = profile_has_enc(display, VAProfile::VAProfileH264High)
        || profile_has_enc(display, VAProfile::VAProfileH264Main)
        || profile_has_enc(display, VAProfile::VAProfileH264ConstrainedBaseline);

    let mut supported_codecs = Vec::new();
    if h264 {
        supported_codecs.push(VideoCodec::H264);
    }

    Ok(EncoderCapabilities {
        name: "VA-API",
        supported_codecs,
        supports_hdr: false,
        supports_yuv444: false,
        max_resolution: Resolution::new(7680, 4320),
        max_framerate: 240,
    })
}

/// Whether `profile` exposes any encode-slice entrypoint.
fn profile_has_enc(display: &Rc<Display>, profile: VAProfile::Type) -> bool {
    match display.query_config_entrypoints(profile) {
        Ok(eps) => eps
            .iter()
            .any(|e| *e == VAEntrypoint::VAEntrypointEncSlice || *e == VAEntrypoint::VAEntrypointEncSliceLP),
        Err(_) => false,
    }
}

/// Pick the H.264 encode entrypoint, preferring the full-feature `EncSlice`
/// over the low-power `EncSliceLP`. Returns whether the low-power path is used.
fn pick_h264_low_power(display: &Rc<Display>) -> Result<bool> {
    for profile in [
        VAProfile::VAProfileH264High,
        VAProfile::VAProfileH264Main,
        VAProfile::VAProfileH264ConstrainedBaseline,
    ] {
        if let Ok(eps) = display.query_config_entrypoints(profile) {
            if eps.contains(&VAEntrypoint::VAEntrypointEncSlice) {
                return Ok(false);
            }
            if eps.contains(&VAEntrypoint::VAEntrypointEncSliceLP) {
                return Ok(true);
            }
        }
    }
    Err(FluxError::EncoderInit(
        "VA-API driver exposes no H.264 encode entrypoint".into(),
    ))
}

/// Upload a packed NV12 buffer into a VA surface via `vaPutImage`, returning the
/// surface's plane layout. Mirrors the `cros-codecs` `ccenc` upload example.
fn upload_nv12(
    display: &Rc<Display>,
    surface: &Surface<()>,
    width: u32,
    height: u32,
    nv12: &[u8],
) -> std::result::Result<FrameLayout, String> {
    let image_fmts = display
        .query_image_formats()
        .map_err(|e| format!("vaQueryImageFormats failed: {e}"))?;
    let image_fmt = image_fmts
        .into_iter()
        .find(|f| f.fourcc == libva::constants::VA_FOURCC_NV12)
        .ok_or_else(|| "driver does not expose an NV12 image format".to_string())?;

    let mut image = Image::create_from(surface, image_fmt, (width, height), (width, height))
        .map_err(|e| format!("vaCreateImage(NV12) failed: {e}"))?;

    let va_image = *image.image();
    let w = width as usize;
    let h = height as usize;
    let dest = image.as_mut();

    // Luma plane.
    let mut src = nv12;
    let mut dst = &mut dest[va_image.offsets[0] as usize..];
    for _ in 0..h {
        dst[..w].copy_from_slice(&src[..w]);
        dst = &mut dst[va_image.pitches[0] as usize..];
        src = &src[w..];
    }

    // Interleaved chroma plane (half height, full width of U/V pairs).
    let mut src = &nv12[w * h..];
    let mut dst = &mut dest[va_image.offsets[1] as usize..];
    for _ in 0..(h / 2) {
        dst[..w].copy_from_slice(&src[..w]);
        dst = &mut dst[va_image.pitches[1] as usize..];
        src = &src[w..];
    }

    drop(image);
    surface.sync().map_err(|e| format!("vaSyncSurface failed: {e}"))?;

    Ok(FrameLayout {
        format: (Fourcc::from(b"NV12"), 0),
        size: CcResolution { width, height },
        planes: vec![
            PlaneLayout {
                buffer_index: 0,
                offset: 0,
                stride: va_image.pitches[0] as usize,
            },
            PlaneLayout {
                buffer_index: 0,
                offset: va_image.offsets[0] as usize,
                stride: va_image.pitches[1] as usize,
            },
        ],
    })
}

// ─────────────────── pure helpers (unit-tested) ───────────────────

/// Build low-latency [`Tunings`] from the rustcast [`EncodeConfig`].
fn low_latency_tunings(config: &EncodeConfig) -> Tunings {
    Tunings {
        rate_control: RateControl::ConstantBitrate(bits_per_sec(config.bitrate_kbps)),
        framerate: config.framerate.max(1),
        min_quality: 0,
        max_quality: u32::MAX,
    }
}

fn bits_per_sec(kbps: u32) -> u64 {
    (kbps as u64).saturating_mul(1000)
}

/// Map `EncodeConfig::gop_size` onto a `LowDelay` keyframe interval. An infinite
/// GOP (`0`) becomes a large but finite interval so periodic IDRs still recover
/// from packet loss.
fn gop_limit(gop_size: u32) -> u16 {
    match gop_size {
        0 => 2048,
        n => n.clamp(1, u16::MAX as u32) as u16,
    }
}

/// Convert a [`CapturedFrame`] to a packed NV12 buffer (`w*h` luma followed by
/// `w*h/2` interleaved chroma). Only CPU-resident (SHM) frames are supported on
/// this path; DMA-BUF zero-copy import is a later step.
fn frame_to_nv12(frame: &CapturedFrame, width: u32, height: u32) -> Result<Vec<u8>> {
    if frame.gpu_handle.is_some() && frame.data.is_empty() {
        return Err(FluxError::Encode {
            frame: frame.sequence,
            reason: "VA-API SHM path needs CPU pixel data; DMA-BUF import not yet wired".into(),
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
enum ChannelOrder {
    Bgra,
    Rgba,
}

/// BT.601 limited-range RGB→NV12 with 2×2 chroma box averaging.
fn packed_rgb_to_nv12(
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

    // Chroma is subsampled 2×2; average the block to reduce aliasing.
    let cw = w / 2;
    for cy in 0..(h / 2) {
        for cx in 0..cw {
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

    Ok(nv12)
}

fn bt601_luma(r: i32, g: i32, b: i32) -> u8 {
    (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8
}

fn bt601_u(r: i32, g: i32, b: i32) -> u8 {
    (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

fn bt601_v(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255) as u8
}

/// Repack possibly-padded NV12 into a tightly packed `w*h*3/2` buffer.
fn passthrough_nv12(data: &[u8], stride: usize, w: usize, h: usize) -> std::result::Result<Vec<u8>, String> {
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
fn i420_to_nv12(data: &[u8], stride: usize, w: usize, h: usize) -> std::result::Result<Vec<u8>, String> {
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

/// Whether an Annex-B H.264 bitstream contains an IDR slice NAL (type 5).
fn bitstream_has_idr(data: &[u8]) -> bool {
    let mut i = 0usize;
    while i + 3 < data.len() {
        // Match a 3- or 4-byte start code.
        let three = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1;
        let four = data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1;
        if three || four {
            let nal_off = i + if four { 4 } else { 3 };
            if nal_off < data.len() && (data[nal_off] & 0x1f) == NAL_UNIT_TYPE_IDR {
                return true;
            }
            i = nal_off;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> EncodeConfig {
        EncodeConfig {
            codec: VideoCodec::H264,
            bitrate_kbps: 8_000,
            framerate: 60,
            gop_size: 0,
            ..Default::default()
        }
    }

    #[test]
    fn tunings_use_cbr_bits_per_second() {
        let t = low_latency_tunings(&cfg());
        assert_eq!(t.framerate, 60);
        match t.rate_control {
            RateControl::ConstantBitrate(bps) => assert_eq!(bps, 8_000_000),
            _ => panic!("expected CBR"),
        }
    }

    #[test]
    fn framerate_never_zero() {
        let mut c = cfg();
        c.framerate = 0;
        assert_eq!(low_latency_tunings(&c).framerate, 1);
    }

    #[test]
    fn gop_limit_maps_infinite_and_finite() {
        assert_eq!(gop_limit(0), 2048);
        assert_eq!(gop_limit(120), 120);
        assert_eq!(gop_limit(u32::MAX), u16::MAX);
    }

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
    fn detects_idr_nal_with_three_and_four_byte_start_codes() {
        // 4-byte start code, NAL type 5 (IDR): header byte 0x65.
        assert!(bitstream_has_idr(&[0, 0, 0, 1, 0x65, 0xab]));
        // 3-byte start code, NAL type 5.
        assert!(bitstream_has_idr(&[0, 0, 1, 0x65]));
        // Non-IDR slice (type 1, header 0x41) and SPS (type 7) only.
        assert!(!bitstream_has_idr(&[0, 0, 0, 1, 0x67, 0, 0, 0, 1, 0x41]));
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
