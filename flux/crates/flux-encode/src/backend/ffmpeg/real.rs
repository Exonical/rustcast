//! Real FFmpeg (libavcodec) VA-API encoder.
//!
//! Drives the `h264_vaapi` / `hevc_vaapi` encoders through `ffmpeg-next`,
//! complementing the cros-codecs H.264 backend by adding HEVC (and HDR10 via
//! HEVC Main10). libavcodec handles all bitstream synthesis (VPS/SPS/PPS, slice
//! headers, HRD), so this backend focuses on hardware setup + frame upload.
//!
//! Like the cros-codecs path, an `AVCodecContext` is `!Send`, so the whole
//! encode pipeline runs on one dedicated thread and the public
//! [`EncodeSession`] talks to it over channels.
//!
//! Frame flow (CPU-upload path, mirroring the Phase 2 SHM approach):
//!
//! ```text
//! CapturedFrame (BGRA/RGBA) ──swscale──▶ sw NV12/P010 AVFrame
//!                            ──av_hwframe_transfer_data──▶ VA-API surface
//!                            ──send_frame/receive_packet──▶ Annex-B / HEVC NAL
//! ```
//!
//! Zero-copy DMA-BUF import (binding a PipeWire DMA-BUF directly as a VA-API
//! surface) is a later slice; CPU frames are colour-converted and uploaded.
//! The live hardware path requires an AMD/Intel VA-API driver and is validated
//! on real hardware (like the Phase 1/2 portal/VA paths), not in unit tests —
//! the pure config-mapping helpers below are unit-tested.

use std::ffi::c_int;
use std::ptr;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{bounded, unbounded, Sender};
use ff::format::Pixel;
use ff::util::error::EAGAIN;
use ff::Error as FfError;
use ffmpeg_next as ff;

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, EncodedPacket};
use flux_core::types::{DynamicRange, PixelFormat, Resolution, VideoCodec};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

/// VA-API surfaces kept in the encoder's hardware frame pool.
const HW_POOL_SIZE: i32 = 20;

// ───────────────────────── pure config mapping ─────────────────────────

/// libavcodec encoder name for a codec on the VA-API hwaccel, or `None` when
/// this backend does not drive the codec (AV1 has no broadly-available
/// `av1_vaapi` encoder yet, so it is left to other backends).
fn encoder_name(codec: VideoCodec) -> Option<&'static str> {
    HwApi::Vaapi.encoder_name(codec)
}

/// The libavutil pixel format a [`CapturedFrame`] is presented as to swscale.
fn source_pixel_format(format: PixelFormat) -> Option<Pixel> {
    match format {
        PixelFormat::Bgra8 => Some(Pixel::BGRA),
        PixelFormat::Rgba8 => Some(Pixel::RGBA),
        // NV12/I420/P010 inputs aren't wired through the FFmpeg upload path yet
        // (the cros-codecs backend handles already-YUV SHM frames); the live
        // PipeWire RGB capture path produces packed BGRA/RGBA.
        PixelFormat::Nv12 | PixelFormat::I420 | PixelFormat::P010 => None,
    }
}

/// The software (CPU-side) pixel format uploaded into the VA-API surface pool:
/// 8-bit NV12 for SDR, 10-bit P010 for HDR10.
fn surface_sw_format(range: DynamicRange) -> Pixel {
    match range {
        DynamicRange::Sdr => Pixel::NV12,
        DynamicRange::Hdr10 => Pixel::P010LE,
    }
}

/// Resolve the GOP length passed to libavcodec. The config uses `0` to mean an
/// "infinite" GOP (IDR frames requested explicitly); libavcodec wants a finite
/// number, so map it to a large interval while still allowing periodic IDRs.
fn effective_gop(gop_size: u32) -> u32 {
    if gop_size == 0 {
        i32::MAX as u32
    } else {
        gop_size
    }
}

/// Low-latency private options handed to the VA-API encoder at open time.
///
/// `rc_mode` selects the rate-control algorithm; `async_depth=1` keeps the
/// driver from queuing extra frames (lower latency at a small throughput cost).
fn low_latency_options(config: &EncodeConfig) -> Vec<(&'static str, String)> {
    use flux_core::types::RateControlMode;
    let rc_mode = match config.rate_control {
        RateControlMode::Cbr => "CBR",
        RateControlMode::Vbr => "VBR",
        RateControlMode::Cqp => "CQP",
    };
    vec![("rc_mode", rc_mode.to_string()), ("async_depth", "1".to_string())]
}

/// Which FFmpeg hardware encode API a hardware session drives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HwApi {
    /// VA-API (`h264_vaapi`/`hevc_vaapi`) — Linux AMD/Intel via Mesa/iHD.
    Vaapi,
    /// Vulkan video (`h264_vulkan`/`hevc_vulkan`) — cross-vendor, driven on
    /// the system Vulkan driver (no VA-API driver required).
    Vulkan,
}

impl HwApi {
    /// The libavutil hardware surface pixel format for this API.
    fn hw_pix_fmt(self) -> ff::ffi::AVPixelFormat {
        match self {
            HwApi::Vaapi => ff::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            HwApi::Vulkan => ff::ffi::AVPixelFormat::AV_PIX_FMT_VULKAN,
        }
    }

    /// libavcodec encoder name for a codec on this API, or `None` when the API
    /// does not drive the codec (no AV1 hardware encoder is wired here).
    fn encoder_name(self, codec: VideoCodec) -> Option<&'static str> {
        match (self, codec) {
            (HwApi::Vaapi, VideoCodec::H264) => Some("h264_vaapi"),
            (HwApi::Vaapi, VideoCodec::H265) => Some("hevc_vaapi"),
            (HwApi::Vulkan, VideoCodec::H264) => Some("h264_vulkan"),
            (HwApi::Vulkan, VideoCodec::H265) => Some("hevc_vulkan"),
            (_, VideoCodec::Av1) => None,
        }
    }
}

/// Whether this session encodes on the GPU (hardware surfaces) or the CPU
/// (libx264/libx265 software fallback).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Accel {
    Hardware(HwApi),
    Software,
}

/// libavcodec software encoder name for a codec, picking the first one the
/// FFmpeg build actually exposes. Used by the CPU fallback when no VA-API
/// driver is available.
fn sw_encoder_name(codec: VideoCodec) -> Option<&'static str> {
    let candidates: &[&'static str] = match codec {
        VideoCodec::H264 => &["libx264", "libopenh264"],
        VideoCodec::H265 => &["libx265"],
        VideoCodec::Av1 => &[],
    };
    candidates
        .iter()
        .copied()
        .find(|name| ff::encoder::find_by_name(name).is_some())
}

/// Low-latency private options for a software encoder. x264/x265 take the
/// `ultrafast`/`zerolatency` preset+tune; other encoders are left at defaults.
fn sw_low_latency_options(encoder_name: &str) -> Vec<(&'static str, String)> {
    match encoder_name {
        "libx264" | "libx265" => vec![
            ("preset", "ultrafast".to_string()),
            ("tune", "zerolatency".to_string()),
        ],
        _ => Vec::new(),
    }
}

/// Low-latency private options for the Vulkan video encoder. Kept minimal:
/// `async_depth=1` avoids extra in-flight frames. Unknown private options are
/// ignored by `avcodec_open2`, so this stays safe across FFmpeg builds; the
/// rate-control target rides on the codec context's `bit_rate` fields.
fn vk_low_latency_options(_config: &EncodeConfig) -> Vec<(&'static str, String)> {
    vec![("async_depth", "1".to_string())]
}

/// Reject configurations this backend cannot drive before touching hardware.
fn validate(config: &EncodeConfig) -> Result<()> {
    if encoder_name(config.codec).is_none() {
        return Err(FluxError::EncoderInit(format!(
            "FFmpeg backend does not encode {:?} (only H.264/H.265)",
            config.codec
        )));
    }
    if config.dynamic_range == DynamicRange::Hdr10 && config.codec != VideoCodec::H265 {
        return Err(FluxError::EncoderInit(
            "HDR10 encode requires HEVC (Main10)".into(),
        ));
    }
    Ok(())
}

// ───────────────────────────── encoder ─────────────────────────────

/// A refcounted VA-API hardware device context (`AVBufferRef*`).
///
/// libavutil hardware device contexts are reference-counted and safe to share
/// across threads, so the raw pointer is `Send`; each session takes its own ref
/// and runs on the encode thread.
struct HwDevice(*mut ff::ffi::AVBufferRef);

// SAFETY: an `AVBufferRef` to an `AVHWDeviceContext` is internally refcounted;
// `av_buffer_ref`/`av_buffer_unref` are thread-safe and the context is only
// used to derive frame pools, never mutated through this pointer.
unsafe impl Send for HwDevice {}
// SAFETY: the only `&self` operation is `new_ref` (atomic `av_buffer_ref`), so
// sharing a reference across threads is sound.
unsafe impl Sync for HwDevice {}

impl HwDevice {
    /// Open a VA-API device, targeting a DRM render node explicitly.
    ///
    /// libva's default (null device) auto-detection relies on an X11 `DISPLAY`
    /// and fails on headless / Wayland sessions, and a machine may expose more
    /// than one render node (e.g. an iGPU + a discrete GPU). So try each
    /// candidate node in turn and finally fall back to the libva default. The
    /// candidate list is, in order: `$FLUX_VAAPI_DEVICE` if set, then
    /// `/dev/dri/renderD128..renderD135`, then null (libva default).
    fn open_vaapi() -> Result<Self> {
        Self::open_any(
            ff::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            vaapi_device_candidates(),
            "VA-API",
            "is a VA-API driver installed? (mesa-va-drivers / vainfo)",
        )
    }

    /// Open a Vulkan device for video encode. libavutil's default picks the
    /// first Vulkan device; `$FLUX_VULKAN_DEVICE` (a device index, e.g. "0")
    /// overrides on multi-GPU hosts. Used when no VA-API driver is available
    /// but the system Vulkan driver exposes the video-encode extensions.
    fn open_vulkan() -> Result<Self> {
        Self::open_any(
            ff::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VULKAN,
            vulkan_device_candidates(),
            "Vulkan",
            "is a Vulkan driver with video-encode support installed? \
             (vulkaninfo | grep VK_KHR_video_encode)",
        )
    }

    /// Try each candidate device for `device_type` in turn, falling back to the
    /// libavutil default (a `None` candidate). Returns a clear init error with
    /// `hint` when every candidate fails.
    fn open_any(
        device_type: ff::ffi::AVHWDeviceType,
        candidates: Vec<Option<String>>,
        label: &str,
        hint: &str,
    ) -> Result<Self> {
        let mut last_ret = 0;
        for device in candidates {
            match Self::open_device(device_type, device.as_deref()) {
                Ok(dev) => {
                    match device.as_deref() {
                        Some(path) => tracing::info!(device = %path, "opened {} device", label),
                        None => tracing::info!("opened {} device (default)", label),
                    }
                    return Ok(dev);
                }
                Err(ret) => {
                    tracing::debug!(
                        device = device.as_deref().unwrap_or("<default>"),
                        ret,
                        "{} device open failed, trying next candidate",
                        label
                    );
                    last_ret = ret;
                }
            }
        }
        Err(FluxError::EncoderInit(format!(
            "failed to create {label} device context \
             (av_hwdevice_ctx_create returned {last_ret}); {hint}"
        )))
    }

    /// Try to open one device by path/index (or the default when `device` is
    /// `None`). Returns the libav error code on failure.
    fn open_device(
        device_type: ff::ffi::AVHWDeviceType,
        device: Option<&str>,
    ) -> std::result::Result<Self, c_int> {
        let c_device = device.and_then(|d| std::ffi::CString::new(d).ok());
        let device_ptr = c_device
            .as_ref()
            .map_or(ptr::null(), |s| s.as_ptr());
        let mut ptr: *mut ff::ffi::AVBufferRef = ptr::null_mut();
        // SAFETY: out-pointer is valid; `device_ptr` is either null or a valid
        // NUL-terminated string that outlives this call.
        let ret = unsafe {
            ff::ffi::av_hwdevice_ctx_create(
                &mut ptr,
                device_type,
                device_ptr,
                ptr::null_mut(),
                0,
            )
        };
        if ret < 0 || ptr.is_null() {
            return Err(ret);
        }
        Ok(Self(ptr))
    }

    /// Take a new owned reference to the same underlying device context.
    fn new_ref(&self) -> Self {
        // SAFETY: `self.0` is a valid `AVBufferRef`; `av_buffer_ref` returns a
        // new ref to the same buffer.
        Self(unsafe { ff::ffi::av_buffer_ref(self.0) })
    }
}

impl Drop for HwDevice {
    fn drop(&mut self) {
        // SAFETY: `self.0` was obtained from `av_hwdevice_ctx_create`/`av_buffer_ref`.
        unsafe { ff::ffi::av_buffer_unref(&mut self.0) };
    }
}

/// VA-API device paths to try opening, in priority order. `None` is the libva
/// default (no explicit device), tried last as a fallback.
fn vaapi_device_candidates() -> Vec<Option<String>> {
    let mut candidates = Vec::new();
    if let Ok(dev) = std::env::var("FLUX_VAAPI_DEVICE") {
        if !dev.is_empty() {
            candidates.push(Some(dev));
        }
    }
    // DRM render nodes are numbered from 128. Probe the low range; a host
    // typically has one or two GPUs (renderD128/129).
    for node in 128..=135 {
        let path = format!("/dev/dri/renderD{node}");
        if std::path::Path::new(&path).exists() {
            candidates.push(Some(path));
        }
    }
    candidates.push(None);
    candidates
}

/// Vulkan device strings to try, in priority order. `None` is the libavutil
/// default (first Vulkan device), tried last. `$FLUX_VULKAN_DEVICE` (a Vulkan
/// device index such as "0") overrides when set, for multi-GPU hosts.
fn vulkan_device_candidates() -> Vec<Option<String>> {
    let mut candidates = Vec::new();
    if let Ok(dev) = std::env::var("FLUX_VULKAN_DEVICE") {
        if !dev.is_empty() {
            candidates.push(Some(dev));
        }
    }
    candidates.push(None);
    candidates
}

/// RAII owner of a VA-API hardware frame pool (`AVBufferRef*`).
///
/// Owning the pool in a guard frees it via `Drop` if encoder setup fails before
/// the [`EncoderState`] is constructed (otherwise a failed init would leak the
/// whole surface pool). On success, [`Self::into_raw`] transfers ownership to
/// `EncoderState`, whose own `Drop` then frees it.
struct HwFramesCtx(*mut ff::ffi::AVBufferRef);

impl HwFramesCtx {
    fn raw(&self) -> *mut ff::ffi::AVBufferRef {
        self.0
    }

    /// Relinquish ownership without freeing; caller becomes responsible.
    fn into_raw(self) -> *mut ff::ffi::AVBufferRef {
        let ptr = self.0;
        std::mem::forget(self);
        ptr
    }
}

impl Drop for HwFramesCtx {
    fn drop(&mut self) {
        // SAFETY: `self.0` was obtained from `av_hwframe_ctx_alloc`.
        unsafe { ff::ffi::av_buffer_unref(&mut self.0) };
    }
}

/// FFmpeg VA-API hardware encoder (Linux, AMD/Intel via Mesa/iHD).
pub struct FfmpegVaapiEncoder {
    capabilities: EncoderCapabilities,
    device: HwDevice,
}

impl FfmpegVaapiEncoder {
    pub fn new() -> Result<Self> {
        ff::init().map_err(|e| FluxError::EncoderInit(format!("ffmpeg init failed: {e}")))?;
        tracing::info!("Initializing FFmpeg VA-API encoder");

        let device = HwDevice::open_vaapi()?;

        let mut supported = Vec::new();
        if ff::encoder::find_by_name("h264_vaapi").is_some() {
            supported.push(VideoCodec::H264);
        }
        let h265 = ff::encoder::find_by_name("hevc_vaapi").is_some();
        if h265 {
            supported.push(VideoCodec::H265);
        }
        if supported.is_empty() {
            return Err(FluxError::EncoderInit(
                "FFmpeg build exposes no VA-API video encoder (need h264_vaapi/hevc_vaapi)".into(),
            ));
        }

        let capabilities = EncoderCapabilities {
            name: "ffmpeg-vaapi",
            supported_codecs: supported.clone(),
            // HDR10 (HEVC Main10) rides on the HEVC VA-API encoder.
            supports_hdr: h265,
            supports_yuv444: false,
            max_resolution: Resolution::new(7680, 4320),
            max_framerate: 240,
        };
        tracing::info!(codecs = ?capabilities.supported_codecs, "FFmpeg VA-API encoder ready");
        Ok(Self { capabilities, device })
    }
}

impl VideoEncoder for FfmpegVaapiEncoder {
    fn name(&self) -> &'static str {
        "ffmpeg-vaapi"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Ok(self.capabilities.clone())
    }

    fn validate_config(&self, config: &EncodeConfig) -> Result<()> {
        validate(config)?;
        if !self.capabilities.supported_codecs.contains(&config.codec) {
            return Err(FluxError::EncoderInit(format!(
                "FFmpeg build has no VA-API encoder for {:?}",
                config.codec
            )));
        }
        let max = self.capabilities.max_resolution;
        if config.resolution.width > max.width || config.resolution.height > max.height {
            return Err(FluxError::EncoderInit(format!(
                "resolution {} exceeds FFmpeg VA-API maximum {}",
                config.resolution, max
            )));
        }
        Ok(())
    }

    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        self.validate_config(&config)?;
        Ok(Box::new(FfmpegSession::spawn(
            config,
            Accel::Hardware(HwApi::Vaapi),
            Some(self.device.new_ref()),
        )?))
    }
}

/// FFmpeg Vulkan-video hardware encoder (cross-vendor; AMD/Intel/NVIDIA via the
/// `VK_KHR_video_encode_*` extensions). Drives the system Vulkan driver, so it
/// needs no VA-API driver — the offload path on hosts where Mesa's VA driver is
/// unavailable (e.g. RHEL/Rocky). Requires FFmpeg >= 7.0 (which first shipped
/// the `h264_vulkan`/`hevc_vulkan` encoders).
pub struct FfmpegVulkanEncoder {
    capabilities: EncoderCapabilities,
    device: HwDevice,
}

impl FfmpegVulkanEncoder {
    pub fn new() -> Result<Self> {
        ff::init().map_err(|e| FluxError::EncoderInit(format!("ffmpeg init failed: {e}")))?;
        tracing::info!("Initializing FFmpeg Vulkan video encoder");

        // Probe encoder availability before opening a device so an old FFmpeg
        // (< 7.0, no Vulkan encoder) fails fast with a clear message rather
        // than a confusing device error.
        let mut supported = Vec::new();
        if ff::encoder::find_by_name("h264_vulkan").is_some() {
            supported.push(VideoCodec::H264);
        }
        let h265 = ff::encoder::find_by_name("hevc_vulkan").is_some();
        if h265 {
            supported.push(VideoCodec::H265);
        }
        if supported.is_empty() {
            return Err(FluxError::EncoderInit(
                "FFmpeg build exposes no Vulkan video encoder (need h264_vulkan/hevc_vulkan; \
                 requires FFmpeg >= 7.0)"
                    .into(),
            ));
        }

        let device = HwDevice::open_vulkan()?;

        let capabilities = EncoderCapabilities {
            name: "ffmpeg-vulkan",
            supported_codecs: supported.clone(),
            // HDR10 (HEVC Main10) rides on the HEVC Vulkan encoder.
            supports_hdr: h265,
            supports_yuv444: false,
            max_resolution: Resolution::new(7680, 4320),
            max_framerate: 240,
        };
        tracing::info!(codecs = ?capabilities.supported_codecs, "FFmpeg Vulkan video encoder ready");
        Ok(Self { capabilities, device })
    }
}

impl VideoEncoder for FfmpegVulkanEncoder {
    fn name(&self) -> &'static str {
        "ffmpeg-vulkan"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Ok(self.capabilities.clone())
    }

    fn validate_config(&self, config: &EncodeConfig) -> Result<()> {
        validate(config)?;
        if !self.capabilities.supported_codecs.contains(&config.codec) {
            return Err(FluxError::EncoderInit(format!(
                "FFmpeg build has no Vulkan encoder for {:?}",
                config.codec
            )));
        }
        let max = self.capabilities.max_resolution;
        if config.resolution.width > max.width || config.resolution.height > max.height {
            return Err(FluxError::EncoderInit(format!(
                "resolution {} exceeds FFmpeg Vulkan maximum {}",
                config.resolution, max
            )));
        }
        Ok(())
    }

    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        self.validate_config(&config)?;
        Ok(Box::new(FfmpegSession::spawn(
            config,
            Accel::Hardware(HwApi::Vulkan),
            Some(self.device.new_ref()),
        )?))
    }
}

/// FFmpeg software (libx264/libx265) encoder — the CPU fallback used when no
/// VA-API driver is available. Reuses the same swscale + session machinery as
/// the hardware path, but encodes the CPU frame directly (no VA-API surfaces).
pub struct FfmpegSoftwareEncoder {
    capabilities: EncoderCapabilities,
}

impl FfmpegSoftwareEncoder {
    pub fn new() -> Result<Self> {
        ff::init().map_err(|e| FluxError::EncoderInit(format!("ffmpeg init failed: {e}")))?;
        tracing::info!("Initializing FFmpeg software (libx264/libx265) encoder");

        let mut supported = Vec::new();
        if sw_encoder_name(VideoCodec::H264).is_some() {
            supported.push(VideoCodec::H264);
        }
        if sw_encoder_name(VideoCodec::H265).is_some() {
            supported.push(VideoCodec::H265);
        }
        if supported.is_empty() {
            return Err(FluxError::EncoderInit(
                "FFmpeg build exposes no software H.264/H.265 encoder (need libx264/libx265)".into(),
            ));
        }

        let capabilities = EncoderCapabilities {
            name: "ffmpeg-software",
            supported_codecs: supported.clone(),
            supports_hdr: false,
            supports_yuv444: false,
            max_resolution: Resolution::new(7680, 4320),
            max_framerate: 240,
        };
        tracing::info!(codecs = ?capabilities.supported_codecs, "FFmpeg software encoder ready");
        Ok(Self { capabilities })
    }
}

impl VideoEncoder for FfmpegSoftwareEncoder {
    fn name(&self) -> &'static str {
        "ffmpeg-software"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        Ok(self.capabilities.clone())
    }

    fn validate_config(&self, config: &EncodeConfig) -> Result<()> {
        if !self.capabilities.supported_codecs.contains(&config.codec) {
            return Err(FluxError::EncoderInit(format!(
                "FFmpeg software backend has no encoder for {:?}",
                config.codec
            )));
        }
        if config.dynamic_range == DynamicRange::Hdr10 {
            return Err(FluxError::EncoderInit(
                "HDR10 is not supported by the FFmpeg software fallback".into(),
            ));
        }
        Ok(())
    }

    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        self.validate_config(&config)?;
        Ok(Box::new(FfmpegSession::spawn(config, Accel::Software, None)?))
    }
}

// ───────────────────────────── session ─────────────────────────────

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

/// Handle to an FFmpeg VA-API encode session running on its own thread.
pub struct FfmpegSession {
    tx: Sender<Cmd>,
    handle: Option<JoinHandle<()>>,
}

impl FfmpegSession {
    fn spawn(config: EncodeConfig, accel: Accel, device: Option<HwDevice>) -> Result<Self> {
        let (tx, rx) = unbounded::<Cmd>();
        let (ready_tx, ready_rx) = bounded::<Result<()>>(1);

        let handle = thread::Builder::new()
            .name("flux-ffmpeg-encode".into())
            .spawn(move || run_encode_thread(config, accel, device, rx, ready_tx))
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

impl EncodeSession for FfmpegSession {
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

impl Drop for FfmpegSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

// ─────────────────────────── encode thread ───────────────────────────

fn run_encode_thread(
    config: EncodeConfig,
    accel: Accel,
    device: Option<HwDevice>,
    rx: crossbeam_channel::Receiver<Cmd>,
    ready_tx: Sender<Result<()>>,
) {
    let mut state = match EncoderState::new(&config, accel, device) {
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
            Cmd::SetBitrate(kbps) => state.set_bitrate(kbps),
            Cmd::Stop => break,
        }
    }
}

/// All `!Send` FFmpeg state, owned by the encode thread.
struct EncoderState {
    encoder: ff::encoder::Video,
    /// VA-API hardware frame pool (`AVBufferRef*`) for the hardware path; kept
    /// alive to allocate hardware frames for each upload. `None` on the
    /// software path, which encodes CPU frames directly.
    frames_ctx: Option<*mut ff::ffi::AVBufferRef>,
    /// Owned device ref, kept alive for the lifetime of the frame pool
    /// (hardware path only).
    _device: Option<HwDevice>,
    /// swscale context, cached and (re)built lazily for the input format the
    /// capture path actually delivers (BGRA or RGBA).
    scaler: Option<(Pixel, ff::software::scaling::Context)>,
    sw_format: Pixel,
    width: u32,
    height: u32,
    force_idr: bool,
    pts: i64,
}

impl EncoderState {
    fn new(config: &EncodeConfig, accel: Accel, device: Option<HwDevice>) -> Result<Self> {
        let name = match accel {
            Accel::Hardware(api) => api.encoder_name(config.codec)
                .ok_or_else(|| FluxError::EncoderInit("unsupported codec".into()))?,
            Accel::Software => sw_encoder_name(config.codec).ok_or_else(|| {
                FluxError::EncoderInit(format!(
                    "no software FFmpeg encoder for {:?} (need libx264/libx265)",
                    config.codec
                ))
            })?,
        };
        let codec = ff::encoder::find_by_name(name)
            .ok_or_else(|| FluxError::EncoderInit(format!("FFmpeg encoder {name} not found")))?;

        let width = config.resolution.width;
        let height = config.resolution.height;
        // Hardware surfaces take NV12 (SDR) / P010 (HDR); the software encoders
        // (libx264/libopenh264) take planar YUV420P.
        let sw_format = match accel {
            Accel::Hardware(_) => surface_sw_format(config.dynamic_range),
            Accel::Software => Pixel::YUV420P,
        };

        let mut octx = ff::codec::context::Context::new_with_codec(codec);

        // Dimensions/rate-control are set before the hardware frame pool is
        // built so the encoder sees a fully-configured context at open time.
        // SAFETY: `octx` owns a freshly-allocated `AVCodecContext`.
        unsafe {
            let ctx = octx.as_mut_ptr();
            (*ctx).width = width as c_int;
            (*ctx).height = height as c_int;
            (*ctx).time_base = ff::ffi::AVRational {
                num: 1,
                den: config.framerate.max(1) as c_int,
            };
            (*ctx).framerate = ff::ffi::AVRational {
                num: config.framerate.max(1) as c_int,
                den: 1,
            };
            (*ctx).gop_size = effective_gop(config.gop_size) as c_int;
            (*ctx).max_b_frames = config.b_frames as c_int;
            (*ctx).refs = config.max_ref_frames.max(1) as c_int;
            let bits = (config.bitrate_kbps as i64) * 1000;
            (*ctx).bit_rate = bits;
            (*ctx).rc_max_rate = bits;
            // `rc_buffer_size` is a C `int`; clamp so huge bitrates don't wrap
            // to a negative buffer size.
            (*ctx).rc_buffer_size = bits.min(i32::MAX as i64) as c_int;
        }

        // Hardware: build a surface pool and feed the encoder hardware surfaces.
        // Software: the encoder ingests the CPU frame directly in its
        // `sw_format`. The pool is held in an RAII guard so it is freed if any
        // `?` below bails out before `EncoderState` (whose `Drop` then owns it)
        // is constructed.
        let frames_ctx = match accel {
            Accel::Hardware(api) => {
                let device_ref = device.as_ref().ok_or_else(|| {
                    FluxError::EncoderInit("hardware encode requires a hardware device".into())
                })?;
                // The encoder ingests hardware surfaces (`pix_fmt`) holding
                // `sw_format` pixels; the Vulkan encoder reads `sw_pix_fmt` at
                // init to pick its picture format, and it is harmless for VA-API.
                // SAFETY: `octx` owns a freshly-allocated `AVCodecContext`.
                unsafe {
                    (*octx.as_mut_ptr()).pix_fmt = api.hw_pix_fmt();
                    (*octx.as_mut_ptr()).sw_pix_fmt = pixel_to_ffi(sw_format);
                }
                // Both APIs build the pool the same way: leave the Vulkan image
                // usage at its default so `hwcontext_vulkan` auto-derives the
                // `VIDEO_ENCODE_SRC` usage the encode queue requires (see
                // `create_hw_frames_ctx`).
                let frames =
                    create_hw_frames_ctx(device_ref, api.hw_pix_fmt(), sw_format, width, height)?;
                // SAFETY: `octx` owns a valid context; bind the pool so the
                // encoder ingests hardware surfaces.
                unsafe {
                    (*octx.as_mut_ptr()).hw_frames_ctx = ff::ffi::av_buffer_ref(frames.raw());
                }
                // Init succeeded so far; transfer ownership to `EncoderState::drop`.
                Some(frames.into_raw())
            }
            Accel::Software => {
                // SAFETY: `octx` owns a freshly-allocated `AVCodecContext`.
                unsafe {
                    (*octx.as_mut_ptr()).pix_fmt = pixel_to_ffi(sw_format);
                }
                None
            }
        };

        let mut opts = ff::Dictionary::new();
        let option_list = match accel {
            Accel::Hardware(HwApi::Vaapi) => low_latency_options(config),
            Accel::Hardware(HwApi::Vulkan) => vk_low_latency_options(config),
            Accel::Software => sw_low_latency_options(name),
        };
        for (k, v) in option_list {
            opts.set(k, &v);
        }

        let video = octx
            .encoder()
            .video()
            .map_err(|e| FluxError::EncoderInit(format!("encoder setup failed: {e}")))?;
        let encoder = video
            .open_with(opts)
            .map_err(|e| FluxError::EncoderInit(format!("failed to open {name}: {e}")))?;

        Ok(Self {
            encoder,
            frames_ctx,
            _device: device,
            scaler: None,
            sw_format,
            width,
            height,
            force_idr: true,
            pts: 0,
        })
    }

    /// Return a swscale context converting `src_format` → `sw_format`, building
    /// (and caching) it on first use or whenever the input format changes.
    fn scaler_for(&mut self, src_format: Pixel) -> Result<&mut ff::software::scaling::Context> {
        let needs_rebuild = self
            .scaler
            .as_ref()
            .map(|(fmt, _)| *fmt != src_format)
            .unwrap_or(true);
        if needs_rebuild {
            let ctx = ff::software::scaling::Context::get(
                src_format,
                self.width,
                self.height,
                self.sw_format,
                self.width,
                self.height,
                ff::software::scaling::Flags::BILINEAR,
            )
            .map_err(|e| FluxError::EncoderInit(format!("swscale init failed: {e}")))?;
            self.scaler = Some((src_format, ctx));
        }
        Ok(&mut self.scaler.as_mut().expect("scaler just set").1)
    }

    fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>> {
        let src_format = source_pixel_format(frame.format).ok_or_else(|| FluxError::Encode {
            frame: frame.sequence,
            reason: format!(
                "FFmpeg VA-API backend needs packed RGB input; {:?} not supported yet",
                frame.format
            ),
        })?;
        if frame.resolution.width != self.width || frame.resolution.height != self.height {
            return Err(FluxError::Encode {
                frame: frame.sequence,
                reason: format!(
                    "frame resolution {} does not match encoder {}x{}",
                    frame.resolution, self.width, self.height
                ),
            });
        }
        if frame.data.is_empty() {
            return Err(FluxError::Encode {
                frame: frame.sequence,
                reason: "frame has no CPU pixel data to upload (DMA-BUF import not yet wired)".into(),
            });
        }

        // 1. Wrap the captured pixels in a source AVFrame and colour-convert to
        //    the surface software format (NV12/P010) with swscale.
        let mut src = ff::frame::Video::new(src_format, self.width, self.height);
        copy_packed_rgb(&mut src, frame)?;
        let mut sw = ff::frame::Video::new(self.sw_format, self.width, self.height);
        let seq = frame.sequence;
        self.scaler_for(src_format)?
            .run(&src, &mut sw)
            .map_err(|e| FluxError::Encode {
                frame: seq,
                reason: format!("swscale failed: {e}"),
            })?;

        // 2. Hardware: upload the software frame into a pooled VA-API surface.
        //    Software: encode the CPU frame directly.
        let mut enc_frame = if let Some(frames_ctx) = self.frames_ctx {
            let mut hw = self.alloc_hw_frame(frames_ctx, frame.sequence)?;
            // SAFETY: both frames are valid; transfer copies sw planes into the surface.
            let ret = unsafe { ff::ffi::av_hwframe_transfer_data(hw.as_mut_ptr(), sw.as_ptr(), 0) };
            if ret < 0 {
                return Err(self.enc_err(frame.sequence, format!("hwframe upload failed ({ret})")));
            }
            hw
        } else {
            sw
        };

        enc_frame.set_pts(Some(self.pts));
        if self.force_idr {
            // SAFETY: `enc_frame` owns a valid `AVFrame`; requesting an I picture forces an IDR.
            unsafe {
                (*enc_frame.as_mut_ptr()).pict_type = ff::ffi::AVPictureType::AV_PICTURE_TYPE_I;
            }
            self.force_idr = false;
        }
        self.pts += 1;

        // 3. Encode and drain any available packets.
        self.encoder
            .send_frame(&enc_frame)
            .map_err(|e| self.enc_err(frame.sequence, format!("send_frame failed: {e}")))?;
        self.drain(frame.sequence)
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        self.encoder
            .send_eof()
            .map_err(|e| self.enc_err(self.pts as u64, format!("send_eof failed: {e}")))?;
        self.drain(self.pts as u64)
    }

    /// Drain `receive_packet` until the encoder needs more input / hits EOF.
    fn drain(&mut self, frame: u64) -> Result<Vec<EncodedPacket>> {
        let mut out = Vec::new();
        loop {
            let mut packet = ff::codec::packet::Packet::empty();
            match self.encoder.receive_packet(&mut packet) {
                Ok(()) => {
                    if let Some(data) = packet.data() {
                        out.push(EncodedPacket {
                            frame_index: packet.pts().unwrap_or(self.pts) as u64,
                            pts: packet.pts().unwrap_or(self.pts) as u64,
                            is_keyframe: packet.is_key(),
                            data: data.to_vec(),
                        });
                    }
                }
                Err(FfError::Other { errno }) if errno == EAGAIN => break,
                Err(FfError::Eof) => break,
                Err(e) => return Err(self.enc_err(frame, format!("receive_packet failed: {e}"))),
            }
        }
        Ok(out)
    }

    /// Allocate a hardware frame backed by the VA-API surface pool.
    fn alloc_hw_frame(
        &self,
        frames_ctx: *mut ff::ffi::AVBufferRef,
        frame: u64,
    ) -> Result<ff::frame::Video> {
        let mut hw = ff::frame::Video::empty();
        // SAFETY: `frames_ctx` is a valid hwframe context; `hw` is a fresh frame.
        let ret = unsafe { ff::ffi::av_hwframe_get_buffer(frames_ctx, hw.as_mut_ptr(), 0) };
        if ret < 0 {
            return Err(self.enc_err(frame, format!("hwframe pool exhausted ({ret})")));
        }
        Ok(hw)
    }

    /// Best-effort runtime bitrate update. VA-API encoders configure their
    /// rate controller at `avcodec_open2`, so most drivers ignore changes to
    /// these fields on an open context; this updates them for drivers/codecs
    /// that do re-read them, and is a no-op otherwise.
    fn set_bitrate(&mut self, kbps: u32) {
        let bits = (kbps as i64) * 1000;
        // SAFETY: encoder owns a valid, opened `AVCodecContext`.
        unsafe {
            let ctx = self.encoder.as_mut_ptr();
            (*ctx).bit_rate = bits;
            (*ctx).rc_max_rate = bits;
        }
    }

    fn enc_err(&self, frame: u64, reason: String) -> FluxError {
        FluxError::Encode { frame, reason }
    }
}

impl Drop for EncoderState {
    fn drop(&mut self) {
        // SAFETY: `frames_ctx` (when present) was obtained from `av_hwframe_ctx_alloc`.
        if let Some(ref mut ctx) = self.frames_ctx {
            unsafe { ff::ffi::av_buffer_unref(ctx) };
        }
    }
}

// ─────────────────────────── ffi helpers ───────────────────────────

/// Allocate and initialize a hardware frame pool for `sw_format` on the given
/// hardware API's surface format (`AV_PIX_FMT_VAAPI` / `AV_PIX_FMT_VULKAN`),
/// returned in an RAII guard that frees it on drop.
///
/// `usage` is deliberately left at its default of `0`. For Vulkan this matters:
/// `hwcontext_vulkan`'s pool init only auto-derives the image usage flags — in
/// particular `VK_IMAGE_USAGE_VIDEO_ENCODE_SRC_BIT_KHR` (and profile-independent
/// image creation via `VK_KHR_video_maintenance1`) — when the caller has not
/// pinned a custom usage. Setting it manually would suppress that derivation and
/// the encoder would reject the surfaces. This mirrors what the `hwupload`
/// filter does on the FFmpeg CLI.
fn create_hw_frames_ctx(
    device: &HwDevice,
    hw_format: ff::ffi::AVPixelFormat,
    sw_format: Pixel,
    width: u32,
    height: u32,
) -> Result<HwFramesCtx> {
    // SAFETY: `device.0` is a valid device context; we configure the returned
    // frames context's public fields before initializing it.
    unsafe {
        let frames_ref = ff::ffi::av_hwframe_ctx_alloc(device.0);
        if frames_ref.is_null() {
            return Err(FluxError::EncoderInit("av_hwframe_ctx_alloc failed".into()));
        }
        let frames_ctx = (*frames_ref).data as *mut ff::ffi::AVHWFramesContext;
        (*frames_ctx).format = hw_format;
        (*frames_ctx).sw_format = pixel_to_ffi(sw_format);
        (*frames_ctx).width = width as c_int;
        (*frames_ctx).height = height as c_int;
        (*frames_ctx).initial_pool_size = HW_POOL_SIZE;
        let ret = ff::ffi::av_hwframe_ctx_init(frames_ref);
        if ret < 0 {
            let mut r = frames_ref;
            ff::ffi::av_buffer_unref(&mut r);
            return Err(FluxError::EncoderInit(format!("av_hwframe_ctx_init failed ({ret})")));
        }
        Ok(HwFramesCtx(frames_ref))
    }
}

/// Convert an `ffmpeg-next` `Pixel` to the raw libavutil `AVPixelFormat`.
fn pixel_to_ffi(p: Pixel) -> ff::ffi::AVPixelFormat {
    p.into()
}

/// Copy packed-RGB pixels from a [`CapturedFrame`] into a source AVFrame,
/// honouring the captured stride vs. the AVFrame's own line size.
fn copy_packed_rgb(dst: &mut ff::frame::Video, frame: &CapturedFrame) -> Result<()> {
    let w = frame.resolution.width as usize;
    let h = frame.resolution.height as usize;
    let row_bytes = w * 4;
    let src_stride = (frame.stride as usize).max(row_bytes);
    if frame.data.len() < src_stride * h {
        return Err(FluxError::Encode {
            frame: frame.sequence,
            reason: format!(
                "RGB buffer too small: {} bytes, need stride {src_stride} * height {h}",
                frame.data.len()
            ),
        });
    }
    let dst_stride = dst.stride(0);
    let dst_data = dst.data_mut(0);
    for row in 0..h {
        let s = row * src_stride;
        let d = row * dst_stride;
        dst_data[d..d + row_bytes].copy_from_slice(&frame.data[s..s + row_bytes]);
    }
    Ok(())
}

// ───────────────────────────── tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::types::RateControlMode;

    fn cfg(codec: VideoCodec) -> EncodeConfig {
        EncodeConfig {
            codec,
            ..EncodeConfig::default()
        }
    }

    #[test]
    fn encoder_name_maps_codecs() {
        assert_eq!(encoder_name(VideoCodec::H264), Some("h264_vaapi"));
        assert_eq!(encoder_name(VideoCodec::H265), Some("hevc_vaapi"));
        assert_eq!(encoder_name(VideoCodec::Av1), None);
    }

    #[test]
    fn hw_api_maps_encoder_names_per_codec() {
        assert_eq!(HwApi::Vaapi.encoder_name(VideoCodec::H264), Some("h264_vaapi"));
        assert_eq!(HwApi::Vaapi.encoder_name(VideoCodec::H265), Some("hevc_vaapi"));
        assert_eq!(HwApi::Vulkan.encoder_name(VideoCodec::H264), Some("h264_vulkan"));
        assert_eq!(HwApi::Vulkan.encoder_name(VideoCodec::H265), Some("hevc_vulkan"));
        assert_eq!(HwApi::Vaapi.encoder_name(VideoCodec::Av1), None);
        assert_eq!(HwApi::Vulkan.encoder_name(VideoCodec::Av1), None);
    }

    #[test]
    fn hw_api_maps_surface_pixel_format() {
        assert_eq!(
            HwApi::Vaapi.hw_pix_fmt(),
            ff::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI
        );
        assert_eq!(
            HwApi::Vulkan.hw_pix_fmt(),
            ff::ffi::AVPixelFormat::AV_PIX_FMT_VULKAN
        );
    }

    #[test]
    fn vulkan_low_latency_options_set_async_depth() {
        let opts = vk_low_latency_options(&cfg(VideoCodec::H264));
        assert!(opts.contains(&("async_depth", "1".to_string())));
    }

    #[test]
    fn source_format_only_accepts_packed_rgb() {
        assert_eq!(source_pixel_format(PixelFormat::Bgra8), Some(Pixel::BGRA));
        assert_eq!(source_pixel_format(PixelFormat::Rgba8), Some(Pixel::RGBA));
        assert_eq!(source_pixel_format(PixelFormat::Nv12), None);
        assert_eq!(source_pixel_format(PixelFormat::I420), None);
        assert_eq!(source_pixel_format(PixelFormat::P010), None);
    }

    #[test]
    fn surface_format_tracks_dynamic_range() {
        assert_eq!(surface_sw_format(DynamicRange::Sdr), Pixel::NV12);
        assert_eq!(surface_sw_format(DynamicRange::Hdr10), Pixel::P010LE);
    }

    #[test]
    fn infinite_gop_maps_to_large_finite_value() {
        assert_eq!(effective_gop(0), i32::MAX as u32);
        assert_eq!(effective_gop(60), 60);
    }

    #[test]
    fn low_latency_options_select_rate_control_and_async_depth() {
        let mut c = cfg(VideoCodec::H265);
        c.rate_control = RateControlMode::Cbr;
        let opts = low_latency_options(&c);
        assert!(opts.contains(&("rc_mode", "CBR".to_string())));
        assert!(opts.contains(&("async_depth", "1".to_string())));

        c.rate_control = RateControlMode::Vbr;
        assert!(low_latency_options(&c).contains(&("rc_mode", "VBR".to_string())));
    }

    #[test]
    fn validate_rejects_av1() {
        let err = validate(&cfg(VideoCodec::Av1));
        assert!(err.is_err());
    }

    #[test]
    fn validate_accepts_h264_and_h265() {
        assert!(validate(&cfg(VideoCodec::H264)).is_ok());
        assert!(validate(&cfg(VideoCodec::H265)).is_ok());
    }

    #[test]
    fn validate_requires_hevc_for_hdr10() {
        let mut c = cfg(VideoCodec::H264);
        c.dynamic_range = DynamicRange::Hdr10;
        assert!(validate(&c).is_err());

        let mut c = cfg(VideoCodec::H265);
        c.dynamic_range = DynamicRange::Hdr10;
        assert!(validate(&c).is_ok());
    }
}
