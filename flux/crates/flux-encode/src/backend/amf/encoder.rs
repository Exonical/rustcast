//! AMD AMF encoder — real implementation with DLL loading and COM vtable calls.
//!
//! Lifecycle:
//!   1. Load amfrt64.dll → AMFQueryVersion → AMFInit → AMFFactory
//!   2. Factory::CreateContext → AMFContext → InitDX11(device)
//!   3. Factory::CreateComponent(context, codec_id) → AMFComponent (encoder)
//!   4. Set properties (usage, bitrate, framerate, etc.) BEFORE Init()
//!   5. Component::Init(format, width, height)
//!   6. Per frame: AllocSurface/CreateSurfaceFromDX11Native → SubmitInput → QueryOutput
//!   7. Drain + Terminate on shutdown

use std::ptr;
use std::sync::Arc;

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, EncodedPacket, GpuFrameHandle};
use flux_core::types::{DynamicRange, RateControlMode, Resolution, VideoCodec};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::core::Interface;

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

use super::constants::*;
use super::ffi::*;

// ─── DLL handle wrapper ─────────────────────────────────────────────────────

/// Holds the dynamically loaded AMF DLL and resolved function pointers.
struct AmfLibrary {
    _handle: *mut std::ffi::c_void,
    init_fn: AMFInit_Fn,
    query_version_fn: AMFQueryVersion_Fn,
}

// AMF library handle is thread-safe (COM apartment model).
unsafe impl Send for AmfLibrary {}
unsafe impl Sync for AmfLibrary {}

impl AmfLibrary {
    fn load() -> Result<Self> {
        unsafe {
            // LoadLibraryA("amfrt64.dll")
            let dll_name = std::ffi::CString::new(AMF_DLL_NAME).unwrap();
            let handle = windows_sys::Win32::System::LibraryLoader::LoadLibraryA(
                dll_name.as_ptr() as *const u8,
            );

            if handle.is_null() {
                return Err(FluxError::EncoderInit(
                    "Failed to load amfrt64.dll — AMD driver may not be installed".into(),
                ));
            }

            // Resolve AMFQueryVersion
            let query_name = std::ffi::CString::new("AMFQueryVersion").unwrap();
            let query_ptr = windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                handle,
                query_name.as_ptr() as *const u8,
            );
            if query_ptr.is_none() {
                return Err(FluxError::EncoderInit(
                    "amfrt64.dll missing AMFQueryVersion export".into(),
                ));
            }
            let query_version_fn: AMFQueryVersion_Fn = std::mem::transmute(query_ptr.unwrap());

            // Resolve AMFInit
            let init_name = std::ffi::CString::new("AMFInit").unwrap();
            let init_ptr = windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                handle,
                init_name.as_ptr() as *const u8,
            );
            if init_ptr.is_none() {
                return Err(FluxError::EncoderInit(
                    "amfrt64.dll missing AMFInit export".into(),
                ));
            }
            let init_fn: AMFInit_Fn = std::mem::transmute(init_ptr.unwrap());

            Ok(Self {
                _handle: handle,
                init_fn,
                query_version_fn,
            })
        }
    }

    fn query_version(&self) -> Result<u64> {
        unsafe {
            let mut version: amf_uint64 = 0;
            let result = (self.query_version_fn)(&mut version);
            check_amf(result, "AMFQueryVersion")?;
            Ok(version)
        }
    }

    fn init_factory(&self, version: u64) -> Result<*mut AMFFactoryObj> {
        unsafe {
            let mut factory: *mut AMFFactoryObj = ptr::null_mut();
            let result = (self.init_fn)(version, &mut factory);
            check_amf(result, "AMFInit")?;
            if factory.is_null() {
                return Err(FluxError::EncoderInit("AMFInit returned null factory".into()));
            }
            Ok(factory)
        }
    }
}

// ─── Safe wrappers around AMF COM objects ───────────────────────────────────

/// RAII wrapper for AMFContext.
struct AmfContext {
    ptr: *mut AMFContextObj,
}

unsafe impl Send for AmfContext {}

impl AmfContext {
    fn new(factory: *mut AMFFactoryObj) -> Result<Self> {
        unsafe {
            let mut ctx: *mut AMFContextObj = ptr::null_mut();
            let result = ((*(*factory).vtbl).CreateContext)(factory, &mut ctx);
            check_amf(result, "CreateContext")?;
            if ctx.is_null() {
                return Err(FluxError::EncoderInit("CreateContext returned null".into()));
            }
            Ok(Self { ptr: ctx })
        }
    }

    fn init_dx11(&self, device: *mut std::ffi::c_void) -> Result<()> {
        unsafe {
            let result = ((*(*self.ptr).vtbl).InitDX11)(self.ptr, device, AMF_DX11_0);
            check_amf(result, "InitDX11")
        }
    }

    fn get_dx11_device(&self) -> Result<ID3D11Device> {
        unsafe {
            let ptr = ((*(*self.ptr).vtbl).GetDX11Device)(self.ptr, AMF_DX11_0);
            if ptr.is_null() {
                return Err(FluxError::EncoderInit("GetDX11Device returned null".into()));
            }
            // AMF returns a borrowed pointer (no AddRef).
            // We use from_raw (which assumes ownership) then clone (AddRef) then forget (avoid Release on original).
            let device: ID3D11Device = windows::core::Interface::from_raw(ptr);
            let owned = device.clone();
            std::mem::forget(device);
            Ok(owned)
        }
    }

    #[allow(dead_code)]
    fn alloc_surface(
        &self,
        format: AMF_SURFACE_FORMAT,
        width: i32,
        height: i32,
    ) -> Result<*mut AMFSurfaceObj> {
        unsafe {
            let mut surface: *mut AMFSurfaceObj = ptr::null_mut();
            let result = ((*(*self.ptr).vtbl).AllocSurface)(
                self.ptr,
                AMF_MEMORY_HOST,
                format,
                width,
                height,
                &mut surface,
            );
            check_amf(result, "AllocSurface")?;
            Ok(surface)
        }
    }

    fn create_surface_from_dx11(
        &self,
        dx11_texture: *mut std::ffi::c_void,
    ) -> Result<*mut AMFSurfaceObj> {
        unsafe {
            let mut surface: *mut AMFSurfaceObj = ptr::null_mut();
            let result = ((*(*self.ptr).vtbl).CreateSurfaceFromDX11Native)(
                self.ptr,
                dx11_texture,
                &mut surface,
                ptr::null_mut(),
            );
            check_amf(result, "CreateSurfaceFromDX11Native")?;
            Ok(surface)
        }
    }

    /// Wrap existing host memory as an AMF surface (zero-copy from CPU buffer).
    fn create_surface_from_host_native(
        &self,
        format: AMF_SURFACE_FORMAT,
        width: i32,
        height: i32,
        h_pitch: i32,
        v_pitch: i32,
        data: *mut std::ffi::c_void,
    ) -> Result<*mut AMFSurfaceObj> {
        unsafe {
            let mut surface: *mut AMFSurfaceObj = ptr::null_mut();
            let result = ((*(*self.ptr).vtbl).CreateSurfaceFromHostNative)(
                self.ptr,
                format,
                width,
                height,
                h_pitch,
                v_pitch,
                data,
                &mut surface,
                ptr::null_mut(),
            );
            check_amf(result, "CreateSurfaceFromHostNative")?;
            Ok(surface)
        }
    }

    fn as_ptr(&self) -> *mut AMFContextObj {
        self.ptr
    }
}

impl Drop for AmfContext {
    fn drop(&mut self) {
        unsafe {
            ((*(*self.ptr).vtbl).Terminate)(self.ptr);
            ((*(*self.ptr).vtbl).Release)(self.ptr);
        }
    }
}

/// RAII wrapper for AMFComponent (encoder).
struct AmfComponent {
    ptr: *mut AMFComponentObj,
}

unsafe impl Send for AmfComponent {}

impl AmfComponent {
    fn set_property_int64(&self, name: &str, value: i64) -> Result<()> {
        let wide_name = to_wide(name);
        let variant = AMFVariantStruct::from_int64(value);
        unsafe {
            let result = ((*(*self.ptr).vtbl).SetProperty)(self.ptr, wide_name.as_ptr(), variant);
            check_amf(result, &format!("SetProperty({})", name))
        }
    }

    fn set_property_bool(&self, name: &str, value: bool) -> Result<()> {
        let wide_name = to_wide(name);
        let variant = AMFVariantStruct::from_bool(value);
        unsafe {
            let result = ((*(*self.ptr).vtbl).SetProperty)(self.ptr, wide_name.as_ptr(), variant);
            check_amf(result, &format!("SetProperty({})", name))
        }
    }

    fn set_property_size(&self, name: &str, width: i32, height: i32) -> Result<()> {
        let wide_name = to_wide(name);
        let variant = AMFVariantStruct::from_size(AMFSize::new(width, height));
        unsafe {
            let result = ((*(*self.ptr).vtbl).SetProperty)(self.ptr, wide_name.as_ptr(), variant);
            check_amf(result, &format!("SetProperty({})", name))
        }
    }

    fn set_property_rate(&self, name: &str, num: u32, den: u32) -> Result<()> {
        let wide_name = to_wide(name);
        let variant = AMFVariantStruct::from_rate(AMFRate::new(num, den));
        unsafe {
            let result = ((*(*self.ptr).vtbl).SetProperty)(self.ptr, wide_name.as_ptr(), variant);
            check_amf(result, &format!("SetProperty({})", name))
        }
    }

    fn init(&self, format: AMF_SURFACE_FORMAT, width: i32, height: i32) -> Result<()> {
        unsafe {
            let result = ((*(*self.ptr).vtbl).Init)(self.ptr, format, width, height);
            check_amf(result, "Component::Init")
        }
    }

    fn submit_input(&self, data: *mut AMFDataObj) -> AMF_RESULT {
        unsafe { ((*(*self.ptr).vtbl).SubmitInput)(self.ptr, data) }
    }

    fn query_output(&self) -> (AMF_RESULT, *mut AMFDataObj) {
        unsafe {
            let mut data: *mut AMFDataObj = ptr::null_mut();
            let result = ((*(*self.ptr).vtbl).QueryOutput)(self.ptr, &mut data);
            (result, data)
        }
    }

    fn drain(&self) -> AMF_RESULT {
        unsafe { ((*(*self.ptr).vtbl).Drain)(self.ptr) }
    }

    #[allow(dead_code)]
    fn flush(&self) -> AMF_RESULT {
        unsafe { ((*(*self.ptr).vtbl).Flush)(self.ptr) }
    }
}

impl Drop for AmfComponent {
    fn drop(&mut self) {
        unsafe {
            ((*(*self.ptr).vtbl).Terminate)(self.ptr);
            ((*(*self.ptr).vtbl).Release)(self.ptr);
        }
    }
}

// ─── AMFData helper ─────────────────────────────────────────────────────────

fn amf_data_set_pts(data: *mut AMFDataObj, pts: i64) {
    unsafe {
        ((*(*data).vtbl).SetPts)(data, pts);
    }
}

fn amf_data_set_property_int64(data: *mut AMFDataObj, name: &str, value: i64) -> Result<()> {
    let wide_name = to_wide(name);
    let variant = AMFVariantStruct::from_int64(value);
    unsafe {
        let result = ((*(*data).vtbl).SetProperty)(data, wide_name.as_ptr(), variant);
        check_amf(result, &format!("Data::SetProperty({})", name))
    }
}

#[allow(dead_code)]
fn amf_data_set_property_bool(data: *mut AMFDataObj, name: &str, value: bool) -> Result<()> {
    let wide_name = to_wide(name);
    let variant = AMFVariantStruct::from_bool(value);
    unsafe {
        let result = ((*(*data).vtbl).SetProperty)(data, wide_name.as_ptr(), variant);
        check_amf(result, &format!("Data::SetProperty({})", name))
    }
}

fn amf_data_get_property_int64(data: *mut AMFDataObj, name: &str) -> Result<i64> {
    let wide_name = to_wide(name);
    let mut variant = AMFVariantStruct::from_int64(0);
    unsafe {
        let result = ((*(*data).vtbl).GetProperty)(data, wide_name.as_ptr(), &mut variant);
        check_amf(result, &format!("Data::GetProperty({})", name))?;
        Ok(variant.data[0] as i64)
    }
}

/// Get the size of an AMFBuffer output via raw vtable slot access.
/// GetSize is an AMFBuffer method (not AMFData), at vtable slot 26.
fn amf_buffer_get_size(buf: *mut AMFDataObj) -> usize {
    unsafe {
        let vtable = (*buf).vtbl as *const *const std::ffi::c_void;
        let get_size_fn: unsafe extern "system" fn(*mut AMFDataObj) -> usize =
            std::mem::transmute(*vtable.add(AMFBUFFER_VTABLE_SLOT_GET_SIZE));
        get_size_fn(buf)
    }
}

/// Get the native pointer of an AMFBuffer output via raw vtable slot access.
/// GetNative is an AMFBuffer method (not AMFData), at vtable slot 27.
fn amf_buffer_get_native(buf: *mut AMFDataObj) -> *mut std::ffi::c_void {
    unsafe {
        let vtable = (*buf).vtbl as *const *const std::ffi::c_void;
        let get_native_fn: unsafe extern "system" fn(*mut AMFDataObj) -> *mut std::ffi::c_void =
            std::mem::transmute(*vtable.add(AMFBUFFER_VTABLE_SLOT_GET_NATIVE));
        get_native_fn(buf)
    }
}

fn amf_data_release(data: *mut AMFDataObj) {
    if !data.is_null() {
        unsafe {
            ((*(*data).vtbl).Release)(data);
        }
    }
}

// ─── Public encoder ─────────────────────────────────────────────────────────

/// AMD AMF hardware encoder (Windows).
///
/// Dynamically loads `amfrt64.dll` and uses the AMF COM-like API to access
/// AMD VCE/VCN encoding hardware.
pub struct AmfEncoder {
    _library: Arc<AmfLibrary>,
    factory: *mut AMFFactoryObj,
    runtime_version: u64,
}

// Factory pointer is thread-safe per AMF spec.
unsafe impl Send for AmfEncoder {}
unsafe impl Sync for AmfEncoder {}

impl AmfEncoder {
    pub fn new() -> Result<Self> {
        let library = AmfLibrary::load()?;
        let version = library.query_version()?;

        tracing::info!(
            "AMF runtime version: {}.{}.{}.{}",
            amf_get_major(version),
            amf_get_minor(version),
            amf_get_subminor(version),
            amf_get_build(version),
        );

        if version < AMF_MIN_VERSION {
            tracing::warn!(
                "AMF version {}.{}.{} is below minimum recommended {}.{}.{}",
                amf_get_major(version),
                amf_get_minor(version),
                amf_get_subminor(version),
                amf_get_major(AMF_MIN_VERSION),
                amf_get_minor(AMF_MIN_VERSION),
                amf_get_subminor(AMF_MIN_VERSION),
            );
        }

        let factory = library.init_factory(version)?;
        tracing::info!("AMF factory initialized successfully");

        Ok(Self {
            _library: Arc::new(library),
            factory,
            runtime_version: version,
        })
    }

    fn supports_av1(&self) -> bool {
        // AV1 requires AMF >= 1.4.30 (RDNA 3 / VCN 4.0)
        self.runtime_version >= amf_make_full_version(1, 4, 30, 0)
    }

    fn supports_hdr(&self) -> bool {
        // HDR (HEVC Main10) requires AMF >= 1.4.23
        self.runtime_version >= amf_make_full_version(1, 4, 23, 0)
    }
}

impl VideoEncoder for AmfEncoder {
    fn name(&self) -> &'static str {
        "AMD AMF"
    }

    fn capabilities(&self) -> Result<EncoderCapabilities> {
        let mut codecs = vec![VideoCodec::H264, VideoCodec::H265];
        if self.supports_av1() {
            codecs.push(VideoCodec::Av1);
        }

        Ok(EncoderCapabilities {
            name: "AMD AMF",
            supported_codecs: codecs,
            supports_hdr: self.supports_hdr(),
            supports_yuv444: false, // VCN < 4.0
            max_resolution: Resolution::new(7680, 4320),
            max_framerate: 240,
        })
    }

    fn validate_config(&self, config: &EncodeConfig) -> Result<()> {
        if config.codec == VideoCodec::Av1 && !self.supports_av1() {
            return Err(FluxError::EncoderInit(
                "AV1 encoding requires AMF >= 1.4.30 (RDNA 3+)".into(),
            ));
        }
        if config.dynamic_range == DynamicRange::Hdr10 && !self.supports_hdr() {
            return Err(FluxError::EncoderInit(
                "HDR encoding requires AMF >= 1.4.23".into(),
            ));
        }
        Ok(())
    }

    fn create_session(&self, config: EncodeConfig) -> Result<Box<dyn EncodeSession>> {
        self.validate_config(&config)?;
        Ok(Box::new(AmfSession::new(self.factory, config)?))
    }
}

// ─── NALU helpers ───────────────────────────────────────────────────────────

/// Scan an Annex-B bitstream for IDR NALUs.
/// Returns true if any start-code-prefixed NALU is an IDR slice.
fn contains_idr_nalu(data: &[u8], codec: VideoCodec) -> bool {
    let mut i = 0;
    while i + 3 < data.len() {
        // Look for 3-byte (00 00 01) or 4-byte (00 00 00 01) start code
        let (sc_len, found) = if i + 3 < data.len() && data[i] == 0 && data[i + 1] == 0 {
            if data[i + 2] == 1 {
                (3, true)
            } else if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                (4, true)
            } else {
                (0, false)
            }
        } else {
            (0, false)
        };

        if found {
            let nalu_offset = i + sc_len;
            if nalu_offset < data.len() {
                let nalu_byte = data[nalu_offset];
                match codec {
                    VideoCodec::H264 => {
                        let nalu_type = nalu_byte & 0x1F;
                        if nalu_type == 5 {
                            return true; // IDR slice
                        }
                    }
                    VideoCodec::H265 => {
                        let nalu_type = (nalu_byte >> 1) & 0x3F;
                        if nalu_type >= 19 && nalu_type <= 20 {
                            return true; // IDR_W_RADL or IDR_N_LP
                        }
                    }
                    VideoCodec::Av1 => {
                        // AV1 uses OBU, keyframe detection is more complex.
                        // For now, rely on frame_index modulo as fallback.
                        return false;
                    }
                }
            }
            i = nalu_offset + 1;
        } else {
            i += 1;
        }
    }
    false
}

// ─── Encode session ─────────────────────────────────────────────────────────

/// An active AMF encoding session.
pub struct AmfSession {
    context: AmfContext,
    encoder: AmfComponent,
    d3d11_device: ID3D11Device,
    config: EncodeConfig,
    frame_index: u64,
    idr_requested: bool,
    surface_format: AMF_SURFACE_FORMAT,
    /// Force an IDR every this many frames (workaround: AMF ignores H264_IDR_PERIOD)
    idr_interval: u64,
    /// Cached texture opened via OpenSharedResource (keyed by handle value)
    cached_shared_texture: Option<(u64, ID3D11Texture2D)>,
}

impl AmfSession {
    fn new(factory: *mut AMFFactoryObj, config: EncodeConfig) -> Result<Self> {
        tracing::info!(
            "Creating AMF session: {} {}@{}fps {}kbps",
            config.codec,
            config.resolution,
            config.framerate,
            config.bitrate_kbps,
        );

        // 1. Create context and init DX11
        let context = AmfContext::new(factory)?;

        // Init DX11 with NULL device — AMF creates its own internal device.
        // In production, pass the DXGI capture device for zero-copy.
        context.init_dx11(ptr::null_mut())?;
        tracing::debug!("AMF context initialized with DX11");

        // Retrieve the internal D3D11 device to support OpenSharedResource
        let d3d11_device = context.get_dx11_device()?;

        // 2. Create encoder component
        let codec_id = match config.codec {
            VideoCodec::H264 => AMF_VIDEO_ENCODER_VCE_AVC,
            VideoCodec::H265 => AMF_VIDEO_ENCODER_HEVC,
            VideoCodec::Av1 => AMF_VIDEO_ENCODER_AV1,
        };
        let codec_id_wide = to_wide(codec_id);

        let encoder = unsafe {
            let mut component: *mut AMFComponentObj = ptr::null_mut();
            let result = ((*(*factory).vtbl).CreateComponent)(
                factory,
                context.as_ptr(),
                codec_id_wide.as_ptr(),
                &mut component,
            );
            check_amf(result, "CreateComponent")?;
            if component.is_null() {
                return Err(FluxError::EncoderInit("CreateComponent returned null".into()));
            }
            AmfComponent { ptr: component }
        };

        // 3. Determine surface format.
        // Use BGRA for SDR — matches DXGI capture output directly.
        // AMF hardware does the CSC (color space conversion) to NV12 on the GPU.
        // For HDR, use P010 (caller must convert).
        let surface_format = if config.dynamic_range == DynamicRange::Hdr10 {
            AMF_SURFACE_P010
        } else {
            AMF_SURFACE_BGRA
        };

        // 4. Set encoder properties based on codec
        let w = config.resolution.width as i32;
        let h = config.resolution.height as i32;
        let bitrate_bps = (config.bitrate_kbps as i64) * 1000;

        match config.codec {
            VideoCodec::H264 => {
                // Switch to LOW_LATENCY usage (2) - used by Sunshine as workaround for AMF issues
                encoder.set_property_int64(H264_USAGE, H264_USAGE_LOW_LATENCY)?;
                encoder.set_property_size(H264_FRAMESIZE, w, h)?;
                encoder.set_property_rate(H264_FRAMERATE, config.framerate, 1)?;
                encoder.set_property_int64(H264_PROFILE, H264_PROFILE_HIGH)?;

                // Rate control
                let rc = match config.rate_control {
                    RateControlMode::Cbr => H264_RC_CBR,
                    RateControlMode::Vbr => H264_RC_VBR_LATENCY,
                    RateControlMode::Cqp => H264_RC_CQP,
                };
                encoder.set_property_int64(H264_RATE_CONTROL_METHOD, rc)?;
                encoder.set_property_int64(H264_TARGET_BITRATE, bitrate_bps)?;
                encoder.set_property_int64(H264_PEAK_BITRATE, bitrate_bps * 3 / 2)?;

                // Single-frame VBV for lowest latency
                let vbv_size = bitrate_bps / config.framerate as i64;
                encoder.set_property_int64(H264_VBV_BUFFER_SIZE, vbv_size)?;

                // Streaming optimizations
                encoder.set_property_bool(H264_FILLER_DATA, false)?;
                // IDR_PERIOD must be 0 to allow per-frame ForcePictureType
                encoder.set_property_int64(H264_IDR_PERIOD, 0)?; 
                encoder.set_property_int64(H264_B_PIC_PATTERN, 0)?; 
                encoder.set_property_int64(H264_HEADER_INSERTION_MODE, HEADER_INSERTION_IDR)?;
                encoder.set_property_bool(H264_ENFORCE_HRD, true)?;
            }
            VideoCodec::H265 => {
                encoder.set_property_int64(HEVC_USAGE, HEVC_USAGE_ULTRA_LOW_LATENCY)?;
                encoder.set_property_size(HEVC_FRAMESIZE, w, h)?;
                encoder.set_property_rate(HEVC_FRAMERATE, config.framerate, 1)?;

                let profile = if config.dynamic_range == DynamicRange::Hdr10 {
                    HEVC_PROFILE_MAIN_10
                } else {
                    HEVC_PROFILE_MAIN
                };
                encoder.set_property_int64(HEVC_PROFILE, profile)?;

                // Tier (High for quality, Main for speed)
                encoder.set_property_int64(HEVC_TIER, HEVC_TIER_HIGH)?;

                // Rate control
                let rc = match config.rate_control {
                    RateControlMode::Cbr => HEVC_RC_CBR,
                    RateControlMode::Vbr => HEVC_RC_VBR_LATENCY,
                    RateControlMode::Cqp => HEVC_RC_CQP,
                };
                encoder.set_property_int64(HEVC_RATE_CONTROL_METHOD, rc)?;
                encoder.set_property_int64(HEVC_TARGET_BITRATE, bitrate_bps)?;
                encoder.set_property_int64(HEVC_PEAK_BITRATE, bitrate_bps * 3 / 2)?;

                let vbv_size = bitrate_bps / config.framerate as i64;
                encoder.set_property_int64(HEVC_VBV_BUFFER_SIZE, vbv_size)?;

                encoder.set_property_bool(HEVC_FILLER_DATA, false)?;
                
                // Picture control
                encoder.set_property_int64(HEVC_GOP_SIZE, config.framerate as i64)?;
                encoder.set_property_int64(HEVC_MAX_NUM_REFRAMES, config.max_ref_frames as i64)?;
                encoder.set_property_bool(HEVC_ENFORCE_HRD, true)?;
            }
            VideoCodec::Av1 => {
                encoder.set_property_int64(AV1_USAGE, AV1_USAGE_ULTRA_LOW_LATENCY)?;
                encoder.set_property_size(AV1_FRAMESIZE, w, h)?;
                encoder.set_property_rate(AV1_FRAMERATE, config.framerate, 1)?;
                encoder.set_property_int64(AV1_PROFILE, AV1_PROFILE_MAIN)?;
                
                // Rate control
                let rc = match config.rate_control {
                    RateControlMode::Cbr => AV1_RC_CBR,
                    RateControlMode::Vbr => AV1_RC_VBR_LATENCY,
                    RateControlMode::Cqp => AV1_RC_CQP,
                };
                encoder.set_property_int64(AV1_RATE_CONTROL_METHOD, rc)?;
                encoder.set_property_int64(AV1_TARGET_BITRATE, bitrate_bps)?;
                encoder.set_property_int64(AV1_PEAK_BITRATE, bitrate_bps * 3 / 2)?;
                
                encoder.set_property_int64(AV1_VBV_BUFFER_SIZE, bitrate_bps / config.framerate as i64)?;
                encoder.set_property_bool(AV1_FILLER_DATA, false)?;
                encoder.set_property_bool(AV1_ENFORCE_HRD, true)?;
            }
        }

        // 5. Initialize the encoder
        encoder.init(surface_format, w, h)?;
        tracing::info!("AMF encoder initialized: {} {:?}", config.codec, surface_format);

        // AMF's IDR_PERIOD property is unreliable on many driver versions.
        // Safety-net IDR every 10 minutes. Real IDR requests come from
        // new client connections via request_idr().
        let idr_interval = config.framerate as u64 * 600;

        Ok(Self {
            context,
            encoder,
            d3d11_device,
            config,
            frame_index: 0,
            idr_requested: false,
            surface_format,
            idr_interval,
            cached_shared_texture: None,
        })
    }

    /// Open (or return cached) shared texture from a DXGI shared handle.
    fn open_shared_texture(&mut self, handle_val: u64) -> Result<&ID3D11Texture2D> {
        // If we already have a cached texture for this handle, reuse it
        if let Some((cached_handle, _)) = &self.cached_shared_texture {
            if *cached_handle == handle_val {
                return Ok(&self.cached_shared_texture.as_ref().unwrap().1);
            }
        }

        // Open the shared resource on AMF's internal D3D11 device
        unsafe {
            let handle = windows::Win32::Foundation::HANDLE(handle_val as *mut std::ffi::c_void);
            let mut texture: Option<ID3D11Texture2D> = None;
            self.d3d11_device.OpenSharedResource(handle, &mut texture)
                .map_err(|e| FluxError::Encode {
                    frame: self.frame_index,
                    reason: format!("OpenSharedResource failed: {}", e),
                })?;
            let texture = texture.ok_or_else(|| FluxError::Encode {
                frame: self.frame_index,
                reason: "OpenSharedResource returned null texture".into(),
            })?;
            self.cached_shared_texture = Some((handle_val, texture));
        }

        Ok(&self.cached_shared_texture.as_ref().unwrap().1)
    }

    /// Extract bitstream from an AMFBuffer output.
    fn extract_output(&self, data: *mut AMFDataObj) -> EncodedPacket {
        let size = amf_buffer_get_size(data);
        let native = amf_buffer_get_native(data);

        let bitstream = if !native.is_null() && size > 0 {
            unsafe { std::slice::from_raw_parts(native as *const u8, size).to_vec() }
        } else {
            Vec::new()
        };

        // Primary: read OutputDataType from the output buffer (authoritative, like OBS).
        // Fallback: scan for IDR NALU types in the bitstream.
        let output_type_prop = match self.config.codec {
            VideoCodec::H264 => H264_OUTPUT_DATA_TYPE,
            VideoCodec::H265 => HEVC_OUTPUT_DATA_TYPE,
            VideoCodec::Av1 => H264_OUTPUT_DATA_TYPE, // fallback, AV1 uses different prop
        };
        let is_keyframe = match amf_data_get_property_int64(data, output_type_prop) {
            Ok(t) => t == OUTPUT_DATA_TYPE_IDR,
            Err(_) => contains_idr_nalu(&bitstream, self.config.codec),
        };

        EncodedPacket {
            frame_index: self.frame_index,
            pts: self.frame_index,
            is_keyframe,
            data: bitstream,
        }
    }

    /// Poll encoder output, collecting all available packets.
    fn drain_output(&self) -> Vec<EncodedPacket> {
        let mut packets = Vec::new();
        loop {
            let (result, data) = self.encoder.query_output();
            if result == AMF_OK && !data.is_null() {
                packets.push(self.extract_output(data));
                amf_data_release(data);
            } else {
                break;
            }
        }
        packets
    }
}

impl EncodeSession for AmfSession {
    fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>> {
        self.frame_index += 1;

        // Periodic IDR: force keyframe every idr_interval frames
        if self.frame_index % self.idr_interval == 1 {
            self.idr_requested = true;
        }

        let is_idr = self.idr_requested;
        self.idr_requested = false;

        // Acquire an input surface
        let surface = if let Some(GpuFrameHandle::DxgiSharedTexture(ref dxgi)) = frame.gpu_handle {
            // Zero-copy path: use cached shared texture reference
            let texture = self.open_shared_texture(dxgi.handle)?;
            let tex_ptr = Interface::as_raw(texture);
            self.context.create_surface_from_dx11(tex_ptr as *mut std::ffi::c_void)?
        } else if !frame.data.is_empty() {
            // CPU path: wrap host memory as an AMF surface (no extra copy).
            // For BGRA: stride = width * 4, vPitch = height.
            let w = self.config.resolution.width as i32;
            let h = self.config.resolution.height as i32;
            let h_pitch = frame.stride as i32; // bytes per row
            let v_pitch = h;                   // number of rows

            self.context.create_surface_from_host_native(
                self.surface_format,
                w,
                h,
                h_pitch,
                v_pitch,
                frame.data.as_ptr() as *mut std::ffi::c_void,
            )?
        } else {
            return Err(FluxError::Encode {
                frame: self.frame_index,
                reason: "Frame has no data and no GPU handle".into(),
            });
        };

        // Set PTS
        let pts_amf = self.frame_index as i64 * 10_000; // 100ns units
        amf_data_set_pts(surface, pts_amf);

        // Force IDR if requested (IDR_PERIOD=0 is set in init for LOW_LATENCY mode)
        if is_idr {
            let force_type_prop = match self.config.codec {
                VideoCodec::H264 => H264_FORCE_PICTURE_TYPE,
                VideoCodec::H265 => HEVC_FORCE_PICTURE_TYPE,
                VideoCodec::Av1 => AV1_FORCE_PICTURE_TYPE,
            };
            // Set INSERT_SPS + INSERT_PPS so decoders can start from any IDR
            if self.config.codec == VideoCodec::H264 {
                let _ = amf_data_set_property_int64(surface, H264_INSERT_SPS, 1);
                let _ = amf_data_set_property_int64(surface, H264_INSERT_PPS, 1);
            }
            match amf_data_set_property_int64(surface, force_type_prop, PICTURE_TYPE_IDR) {
                Ok(()) => tracing::debug!("AMF: SetProperty({}) = IDR({}) on surface OK", force_type_prop, PICTURE_TYPE_IDR),
                Err(e) => tracing::warn!("AMF: SetProperty({}) failed: {}", force_type_prop, e),
            }
        }

        // Submit input — may return AMF_INPUT_FULL, in which case we poll output first
        let mut submit_result = self.encoder.submit_input(surface);
        let mut packets = Vec::new();

        if submit_result == AMF_INPUT_FULL {
            // Drain output before retrying
            packets.extend(self.drain_output());
            submit_result = self.encoder.submit_input(surface);
        }

        if submit_result != AMF_OK {
            amf_data_release(surface);
            return Err(FluxError::Encode {
                frame: self.frame_index,
                reason: format!("AMF SubmitInput failed: {}", submit_result),
            });
        }

        // Release the input surface — AMF has its own internal reference now.
        // Without this, every frame leaks an AMF surface object.
        amf_data_release(surface);

        // Poll output
        packets.extend(self.drain_output());

        Ok(packets)
    }

    fn request_idr(&mut self) {
        tracing::debug!("AMF: IDR frame requested for next encode");
        self.idr_requested = true;
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        tracing::debug!("AMF: flushing encoder");

        // Signal end-of-stream
        let drain_result = self.encoder.drain();
        if drain_result != AMF_OK && drain_result != AMF_EOF {
            tracing::warn!("AMF Drain returned {}", drain_result);
        }

        // Collect remaining output
        let mut packets = Vec::new();
        loop {
            let (result, data) = self.encoder.query_output();
            if result == AMF_OK && !data.is_null() {
                packets.push(self.extract_output(data));
                amf_data_release(data);
            } else {
                break;
            }
        }

        tracing::debug!("AMF flush: {} remaining packets", packets.len());
        Ok(packets)
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        let bitrate_bps = (bitrate_kbps as i64) * 1000;
        let peak_bps = bitrate_bps * 3 / 2;

        match self.config.codec {
            VideoCodec::H264 => {
                self.encoder.set_property_int64(H264_TARGET_BITRATE, bitrate_bps)?;
                self.encoder.set_property_int64(H264_PEAK_BITRATE, peak_bps)?;
            }
            VideoCodec::H265 => {
                self.encoder.set_property_int64(HEVC_TARGET_BITRATE, bitrate_bps)?;
                self.encoder.set_property_int64(HEVC_PEAK_BITRATE, peak_bps)?;
            }
            VideoCodec::Av1 => {
                self.encoder.set_property_int64(AV1_TARGET_BITRATE, bitrate_bps)?;
                self.encoder.set_property_int64(AV1_PEAK_BITRATE, peak_bps)?;
            }
        }

        tracing::info!("AMF: bitrate updated to {} kbps", bitrate_kbps);
        self.config.bitrate_kbps = bitrate_kbps;
        Ok(())
    }
}

#[allow(dead_code)]
fn surface_format_name(f: AMF_SURFACE_FORMAT) -> &'static str {
    match f {
        AMF_SURFACE_NV12 => "NV12",
        AMF_SURFACE_P010 => "P010",
        AMF_SURFACE_BGRA => "BGRA",
        AMF_SURFACE_RGBA => "RGBA",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::VideoEncoder;

    #[test]
    fn test_amf_availability() {
        // Should not panic — either succeeds or returns a clean error
        let result = AmfEncoder::new();
        match result {
            Ok(encoder) => {
                let caps = encoder.capabilities().expect("capabilities should succeed");
                println!("AMF available! Supported codecs: {:?}", caps.supported_codecs);
                assert!(!caps.supported_codecs.is_empty(), "should report at least one codec");
                println!(
                    "Max resolution: {}x{}, max fps: {}, HDR: {}",
                    caps.max_resolution.width, caps.max_resolution.height,
                    caps.max_framerate, caps.supports_hdr
                );
                assert!(caps.max_resolution.width > 0);
                assert!(caps.max_resolution.height > 0);
            }
            Err(e) => {
                println!("AMF not available (expected if no AMD GPU): {}", e);
            }
        }
    }

    #[test]
    fn test_amf_library_load() {
        let result = AmfLibrary::load();
        match result {
            Ok(lib) => {
                let version = lib.query_version().expect("query_version should succeed");
                println!(
                    "AMF version: {}.{}.{}.{}",
                    amf_get_major(version),
                    amf_get_minor(version),
                    amf_get_subminor(version),
                    amf_get_build(version),
                );
                assert!(amf_get_major(version) >= 1);
            }
            Err(e) => {
                println!("AMF DLL not found (expected without AMD driver): {}", e);
            }
        }
    }

    #[test]
    fn test_surface_format_name() {
        assert_eq!(surface_format_name(AMF_SURFACE_NV12), "NV12");
        assert_eq!(surface_format_name(AMF_SURFACE_P010), "P010");
        assert_eq!(surface_format_name(AMF_SURFACE_BGRA), "BGRA");
        assert_eq!(surface_format_name(AMF_SURFACE_RGBA), "RGBA");
        assert_eq!(surface_format_name(9999), "unknown");
    }
}
