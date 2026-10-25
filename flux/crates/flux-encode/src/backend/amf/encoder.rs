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

use std::fmt;
use std::ptr;
use std::sync::Arc;
use std::time::Instant;

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, DXGI_SHARED_TEXTURE_RING_SIZE, EncodedPacket, GpuDeviceHandle, GpuFrameHandle};
use flux_core::types::{DynamicRange, RateControlMode, Resolution, VideoCodec};

use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::{IDXGIAdapter1, IDXGIDevice};
use windows::core::Interface;
use windows_sys::Win32::Foundation::HMODULE;
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

use crate::traits::{EncodeConfig, EncodeSession, EncoderCapabilities, VideoEncoder};

use super::constants::*;
use super::ffi::*;

// ─── DLL handle wrapper ─────────────────────────────────────────────────────

/// Holds the dynamically loaded AMF DLL and resolved function pointers.
///
/// The DLL is never unloaded (there is no `FreeLibrary`), so the export
/// pointers and every object the runtime hands out stay backed by mapped code
/// for the rest of the process.
struct AmfLibrary {
    _handle: HMODULE,
    init_fn: AMFInit_Fn,
    query_version_fn: AMFQueryVersion_Fn,
}

// SAFETY: the fields are a module handle that is never freed and two export
// addresses, none of which is tied to the loading thread. The exports are only
// called from `AmfEncoder::new`, before the library is shared.
unsafe impl Send for AmfLibrary {}
unsafe impl Sync for AmfLibrary {}

/// Untyped export address, as returned by `GetProcAddress`.
type RawExport = unsafe extern "system" fn() -> isize;

/// Resolve a named export of the loaded AMF DLL.
fn amf_export(handle: HMODULE, name: &str) -> Result<RawExport> {
    let c_name = std::ffi::CString::new(name).unwrap();
    // SAFETY: `handle` was returned by `LoadLibraryA` and is never freed;
    // `c_name` is NUL-terminated and outlives the call.
    let export = unsafe { GetProcAddress(handle, c_name.as_ptr() as *const u8) };
    export.ok_or_else(|| FluxError::EncoderInit(format!("amfrt64.dll missing {} export", name)))
}

impl AmfLibrary {
    fn load() -> Result<Self> {
        let dll_name = std::ffi::CString::new(AMF_DLL_NAME).unwrap();
        // SAFETY: `dll_name` is NUL-terminated and outlives the call.
        let handle = unsafe { LoadLibraryA(dll_name.as_ptr() as *const u8) };
        if handle.is_null() {
            return Err(FluxError::EncoderInit(
                "Failed to load amfrt64.dll — AMD driver may not be installed".into(),
            ));
        }

        let query_version = amf_export(handle, "AMFQueryVersion")?;
        let init = amf_export(handle, "AMFInit")?;
        // SAFETY: `AMFQueryVersion_Fn`/`AMFInit_Fn` mirror the export typedefs in
        // the SDK's core/Factory.h. They are declared `__cdecl` there, which on
        // x86_64 Windows is the single calling convention `extern "system"` names.
        let (query_version_fn, init_fn) = unsafe {
            (
                std::mem::transmute::<RawExport, AMFQueryVersion_Fn>(query_version),
                std::mem::transmute::<RawExport, AMFInit_Fn>(init),
            )
        };

        Ok(Self {
            _handle: handle,
            init_fn,
            query_version_fn,
        })
    }

    fn query_version(&self) -> Result<u64> {
        let mut version: amf_uint64 = 0;
        // SAFETY: `query_version_fn` is the resolved `AMFQueryVersion` export and
        // `version` is a valid out-param.
        let result = unsafe { (self.query_version_fn)(&mut version) };
        check_amf(result, "AMFQueryVersion")?;
        Ok(version)
    }

    fn init_factory(&self, version: u64) -> Result<AmfFactory> {
        let mut factory: *mut AMFFactoryObj = ptr::null_mut();
        // SAFETY: `init_fn` is the resolved `AMFInit` export and `factory` is a
        // valid out-param.
        let result = unsafe { (self.init_fn)(version, &mut factory) };
        check_amf(result, "AMFInit")?;
        if factory.is_null() {
            return Err(FluxError::EncoderInit("AMFInit returned null factory".into()));
        }
        Ok(AmfFactory { ptr: factory })
    }
}

// ─── Safe wrappers around AMF COM objects ───────────────────────────────────
//
// Each wrapper upholds one invariant: `ptr` is a non-null, live AMF object
// whose vtable matches the `ffi` struct it is read through. The runtime never
// changes an object's vtable pointer, so `vtbl()` is the only place that
// dereferences it; the remaining unsafe blocks only vouch for call arguments.
// AMF is COM-like but not COM (no apartments or CoInitialize), so objects are
// not bound to the thread that created them.

/// The runtime's `AMFFactory` singleton.
///
/// Invariant: `ptr` is the non-null factory returned by `AMFInit`. The factory
/// is not reference-counted and must never be released; it stays valid while
/// the DLL is loaded, which is the rest of the process.
#[derive(Clone, Copy)]
struct AmfFactory {
    ptr: *mut AMFFactoryObj,
}

// SAFETY: the factory is a process-wide singleton, not tied to the thread that
// called `AMFInit`. `Sync` (which makes `AmfEncoder` `Sync`) additionally relies
// on the runtime tolerating concurrent `CreateContext`/`CreateComponent` calls;
// the AMF API reference does not state factory thread-safety explicitly.
unsafe impl Send for AmfFactory {}
unsafe impl Sync for AmfFactory {}

impl AmfFactory {
    fn vtbl(&self) -> &AMFFactoryVtbl {
        // SAFETY: by the type invariant `ptr` is the live factory.
        unsafe { &*(*self.ptr).vtbl }
    }

    fn create_context(&self) -> Result<AmfContext> {
        let mut ctx: *mut AMFContextObj = ptr::null_mut();
        // SAFETY: `ptr` is the live factory and `ctx` is a valid out-param.
        let result = unsafe { (self.vtbl().CreateContext)(self.ptr, &mut ctx) };
        check_amf(result, "CreateContext")?;
        if ctx.is_null() {
            return Err(FluxError::EncoderInit("CreateContext returned null".into()));
        }
        Ok(AmfContext { ptr: ctx })
    }

    fn create_component(&self, context: &AmfContext, codec_id: &str) -> Result<AmfComponent> {
        let codec_id_wide = to_wide(codec_id);
        let mut component: *mut AMFComponentObj = ptr::null_mut();
        // SAFETY: the factory and `context` are live, `codec_id_wide` is a
        // NUL-terminated UTF-16 string that outlives the call, and `component`
        // is a valid out-param.
        let result =
            unsafe { (self.vtbl().CreateComponent)(self.ptr, context.ptr, codec_id_wide.as_ptr(), &mut component) };
        check_amf(result, "CreateComponent")?;
        if component.is_null() {
            return Err(FluxError::EncoderInit("CreateComponent returned null".into()));
        }
        Ok(AmfComponent { ptr: component })
    }
}

/// Owned reference to an `AMFContext`.
///
/// Invariant: `ptr` is non-null and this wrapper owns the reference returned
/// by `CreateContext`; `Drop` terminates and releases it.
struct AmfContext {
    ptr: *mut AMFContextObj,
}

// SAFETY: the context is not bound to its creating thread (see above) and is
// only reached through its single owner, `AmfSession`.
unsafe impl Send for AmfContext {}

impl AmfContext {
    fn vtbl(&self) -> &AMFContextVtbl {
        // SAFETY: by the type invariant `ptr` is a live context.
        unsafe { &*(*self.ptr).vtbl }
    }

    /// Initialize for DX11 on `device`, or on an AMF-selected adapter if null.
    fn init_dx11(&self, device: *mut std::ffi::c_void) -> Result<()> {
        // SAFETY: `device` is null or a live ID3D11Device; `AmfSession` keeps the
        // capture device alive for as long as it owns this context.
        let result = unsafe { (self.vtbl().InitDX11)(self.ptr, device, AMF_DX11_0) };
        check_amf(result, "InitDX11")
    }

    fn get_dx11_device(&self) -> Result<ID3D11Device> {
        // SAFETY: `ptr` is a live context; the call takes no pointer arguments.
        let raw = unsafe { (self.vtbl().GetDX11Device)(self.ptr, AMF_DX11_0) };
        // SAFETY: a non-null result is the context's ID3D11Device. AMF does not
        // AddRef it, so it is borrowed and cloned (AddRef) rather than adopted.
        let device = unsafe { ID3D11Device::from_raw_borrowed(&raw) };
        device
            .cloned()
            .ok_or_else(|| FluxError::EncoderInit("GetDX11Device returned null".into()))
    }

    #[allow(dead_code)]
    fn alloc_surface(&self, format: AMF_SURFACE_FORMAT, width: i32, height: i32) -> Result<AmfData> {
        let mut surface: *mut AMFSurfaceObj = ptr::null_mut();
        // SAFETY: `ptr` is a live context and `surface` is a valid out-param.
        let result =
            unsafe { (self.vtbl().AllocSurface)(self.ptr, AMF_MEMORY_HOST, format, width, height, &mut surface) };
        check_amf(result, "AllocSurface")?;
        // SAFETY: on success `surface` is null or a new reference we now own.
        unsafe { AmfData::from_created(surface, "AllocSurface") }
    }

    /// Wrap a D3D11 texture as an AMF surface (zero-copy).
    ///
    /// # Safety
    ///
    /// `dx11_texture` must be a live `ID3D11Texture2D` on this context's device,
    /// kept alive and unmodified until AMF has finished encoding the surface
    /// (encoding continues after `SubmitInput` returns).
    unsafe fn create_surface_from_dx11(&self, dx11_texture: *mut std::ffi::c_void) -> Result<AmfData> {
        let mut surface: *mut AMFSurfaceObj = ptr::null_mut();
        // SAFETY: `ptr` is a live context, the caller vouches for `dx11_texture`,
        // and `surface` is a valid out-param.
        let result =
            unsafe { (self.vtbl().CreateSurfaceFromDX11Native)(self.ptr, dx11_texture, &mut surface, ptr::null_mut()) };
        check_amf(result, "CreateSurfaceFromDX11Native")?;
        // SAFETY: on success `surface` is null or a new reference we now own.
        unsafe { AmfData::from_created(surface, "CreateSurfaceFromDX11Native") }
    }

    /// Wrap existing host memory as an AMF surface (zero-copy from CPU buffer).
    ///
    /// # Safety
    ///
    /// `data` must point at `h_pitch * v_pitch` readable bytes laid out as
    /// `format`, kept alive and unmodified until AMF has finished encoding the
    /// surface (encoding continues after `SubmitInput` returns).
    unsafe fn create_surface_from_host_native(
        &self,
        format: AMF_SURFACE_FORMAT,
        width: i32,
        height: i32,
        h_pitch: i32,
        v_pitch: i32,
        data: *mut std::ffi::c_void,
    ) -> Result<AmfData> {
        let mut surface: *mut AMFSurfaceObj = ptr::null_mut();
        // SAFETY: `ptr` is a live context, the caller vouches for `data`, and
        // `surface` is a valid out-param.
        let result = unsafe {
            (self.vtbl().CreateSurfaceFromHostNative)(
                self.ptr,
                format,
                width,
                height,
                h_pitch,
                v_pitch,
                data,
                &mut surface,
                ptr::null_mut(),
            )
        };
        check_amf(result, "CreateSurfaceFromHostNative")?;
        // SAFETY: on success `surface` is null or a new reference we now own.
        unsafe { AmfData::from_created(surface, "CreateSurfaceFromHostNative") }
    }
}

impl Drop for AmfContext {
    fn drop(&mut self) {
        let vtbl = self.vtbl();
        // SAFETY: `ptr` is live and we own one reference; nothing uses the
        // context after this.
        unsafe {
            (vtbl.Terminate)(self.ptr);
            (vtbl.Release)(self.ptr);
        }
    }
}

/// Owned reference to an `AMFComponent` (encoder).
///
/// Invariant: `ptr` is non-null and this wrapper owns the reference returned
/// by `CreateComponent`; `Drop` terminates and releases it.
struct AmfComponent {
    ptr: *mut AMFComponentObj,
}

// SAFETY: the component is not bound to its creating thread (AMF documents all
// components as thread-safe) and is only reached through `AmfSession`.
unsafe impl Send for AmfComponent {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertyStatus {
    Supported,
    Unsupported,
    Accepted,
    Rejected,
    NotApplicable,
}

impl fmt::Display for PropertyStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::NotApplicable => "n/a",
        };
        f.write_str(label)
    }
}

impl AmfComponent {
    fn vtbl(&self) -> &AMFComponentVtbl {
        // SAFETY: by the type invariant `ptr` is a live component.
        unsafe { &*(*self.ptr).vtbl }
    }

    fn has_property(&self, name: &str) -> bool {
        let wide_name = to_wide(name);
        // SAFETY: `ptr` is a live component and `wide_name` is a NUL-terminated
        // UTF-16 string that outlives the call.
        unsafe { (self.vtbl().HasProperty)(self.ptr, wide_name.as_ptr()) }
    }

    fn property_support_status(&self, name: &str) -> PropertyStatus {
        if self.has_property(name) {
            tracing::info!("AMF: {} supported by runtime", name);
            PropertyStatus::Supported
        } else {
            tracing::info!("AMF: {} unsupported by runtime", name);
            PropertyStatus::Unsupported
        }
    }

    fn set_property(&self, name: &str, variant: AMFVariantStruct) -> Result<()> {
        let wide_name = to_wide(name);
        // SAFETY: `ptr` is a live component, `wide_name` is a NUL-terminated
        // UTF-16 string that outlives the call, and `variant` is passed by value.
        let result = unsafe { (self.vtbl().SetProperty)(self.ptr, wide_name.as_ptr(), variant) };
        check_amf(result, &format!("SetProperty({})", name))
    }

    fn set_property_int64(&self, name: &str, value: i64) -> Result<()> {
        self.set_property(name, AMFVariantStruct::from_int64(value))
    }
    fn set_optional_int64(&self, name: &str, value: i64) -> PropertyStatus {
        if !self.has_property(name) {
            tracing::info!("AMF: SetProperty({}) unsupported", name);
            return PropertyStatus::Unsupported;
        }
        match self.set_property_int64(name, value) {
            Ok(()) => {
                tracing::info!("AMF: SetProperty({}) = {} accepted", name, value);
                PropertyStatus::Accepted
            }
            Err(e) => {
                tracing::warn!("AMF: SetProperty({}) = {} rejected: {}", name, value, e);
                PropertyStatus::Rejected
            }
        }
    }

    fn set_optional_rate(&self, name: &str, num: u32, den: u32) -> PropertyStatus {
        if !self.has_property(name) {
            tracing::info!("AMF: SetProperty({}) unsupported", name);
            return PropertyStatus::Unsupported;
        }
        match self.set_property_rate(name, num, den) {
            Ok(()) => {
                tracing::info!("AMF: SetProperty({}) = {}/{} accepted", name, num, den);
                PropertyStatus::Accepted
            }
            Err(e) => {
                tracing::warn!(
                    "AMF: SetProperty({}) = {}/{} rejected: {}",
                    name,
                    num,
                    den,
                    e
                );
                PropertyStatus::Rejected
            }
        }
    }

    fn set_property_bool(&self, name: &str, value: bool) -> Result<()> {
        self.set_property(name, AMFVariantStruct::from_bool(value))
    }

    fn set_optional_bool(&self, name: &str, value: bool) -> PropertyStatus {
        if !self.has_property(name) {
            tracing::info!("AMF: SetProperty({}) unsupported", name);
            return PropertyStatus::Unsupported;
        }
        match self.set_property_bool(name, value) {
            Ok(()) => {
                tracing::info!("AMF: SetProperty({}) = {} accepted", name, value);
                PropertyStatus::Accepted
            }
            Err(e) => {
                tracing::warn!("AMF: SetProperty({}) = {} rejected: {}", name, value, e);
                PropertyStatus::Rejected
            }
        }
    }

    fn set_property_size(&self, name: &str, width: i32, height: i32) -> Result<()> {
        self.set_property(name, AMFVariantStruct::from_size(AMFSize::new(width, height)))
    }

    fn set_property_rate(&self, name: &str, num: u32, den: u32) -> Result<()> {
        self.set_property(name, AMFVariantStruct::from_rate(AMFRate::new(num, den)))
    }

    fn init(&self, format: AMF_SURFACE_FORMAT, width: i32, height: i32) -> Result<()> {
        // SAFETY: `ptr` is a live component; the arguments are plain values.
        let result = unsafe { (self.vtbl().Init)(self.ptr, format, width, height) };
        check_amf(result, "Component::Init")
    }

    fn submit_input(&self, data: &AmfData) -> AMF_RESULT {
        // SAFETY: `ptr` is a live component and `data` a live surface; AMF takes
        // its own reference to keep it past this call.
        unsafe { (self.vtbl().SubmitInput)(self.ptr, data.ptr) }
    }

    /// Poll one encoded output. Video encoder components output `AMFBuffer`s
    /// holding the bitstream.
    fn query_output(&self) -> (AMF_RESULT, Option<AmfBuffer>) {
        let mut data: *mut AMFDataObj = ptr::null_mut();
        // SAFETY: `ptr` is a live component and `data` is a valid out-param.
        let result = unsafe { (self.vtbl().QueryOutput)(self.ptr, &mut data) };
        // SAFETY: a non-null output is an `AMFBuffer` whose reference is
        // transferred to us.
        (result, unsafe { AmfBuffer::from_raw(data) })
    }

    fn drain(&self) -> AMF_RESULT {
        // SAFETY: `ptr` is a live component; the call takes no arguments.
        unsafe { (self.vtbl().Drain)(self.ptr) }
    }

    #[allow(dead_code)]
    fn flush(&self) -> AMF_RESULT {
        // SAFETY: `ptr` is a live component; the call takes no arguments.
        unsafe { (self.vtbl().Flush)(self.ptr) }
    }
}

impl Drop for AmfComponent {
    fn drop(&mut self) {
        let vtbl = self.vtbl();
        // SAFETY: `ptr` is live and we own one reference; nothing uses the
        // component after this.
        unsafe {
            (vtbl.Terminate)(self.ptr);
            (vtbl.Release)(self.ptr);
        }
    }
}

// ─── AMFData / AMFBuffer wrappers ───────────────────────────────────────────

/// Owned reference to an `AMFData` (an input surface or output buffer).
///
/// Invariant: `ptr` is non-null and this wrapper owns one reference, which
/// `Drop` releases.
struct AmfData {
    ptr: *mut AMFDataObj,
}

impl AmfData {
    /// Adopt the object a successful `call` wrote to its out-param.
    ///
    /// # Safety
    ///
    /// `ptr` must be null or a live `AMFData` whose reference is transferred to
    /// the returned wrapper.
    unsafe fn from_created(ptr: *mut AMFDataObj, call: &str) -> Result<Self> {
        if ptr.is_null() {
            return Err(FluxError::EncoderInit(format!("{} returned null", call)));
        }
        Ok(Self { ptr })
    }

    fn vtbl(&self) -> &AMFDataVtbl {
        // SAFETY: by the type invariant `ptr` is a live data object.
        unsafe { &*(*self.ptr).vtbl }
    }

    fn set_pts(&self, pts: i64) {
        // SAFETY: `ptr` is live; the argument is a plain value.
        unsafe { (self.vtbl().SetPts)(self.ptr, pts) }
    }

    fn set_duration(&self, duration: i64) {
        // SAFETY: `ptr` is live; the argument is a plain value.
        unsafe { (self.vtbl().SetDuration)(self.ptr, duration) }
    }

    fn set_property(&self, name: &str, variant: AMFVariantStruct) -> Result<()> {
        let wide_name = to_wide(name);
        // SAFETY: `ptr` is live, `wide_name` is a NUL-terminated UTF-16 string
        // that outlives the call, and `variant` is passed by value.
        let result = unsafe { (self.vtbl().SetProperty)(self.ptr, wide_name.as_ptr(), variant) };
        check_amf(result, &format!("Data::SetProperty({})", name))
    }

    fn set_property_int64(&self, name: &str, value: i64) -> Result<()> {
        self.set_property(name, AMFVariantStruct::from_int64(value))
    }

    #[allow(dead_code)]
    fn set_property_bool(&self, name: &str, value: bool) -> Result<()> {
        self.set_property(name, AMFVariantStruct::from_bool(value))
    }

    fn get_property_int64(&self, name: &str) -> Result<i64> {
        let wide_name = to_wide(name);
        let mut variant = AMFVariantStruct::from_int64(0);
        // SAFETY: `ptr` is live, `wide_name` is a NUL-terminated UTF-16 string
        // that outlives the call, and `variant` is a valid out-param.
        let result = unsafe { (self.vtbl().GetProperty)(self.ptr, wide_name.as_ptr(), &mut variant) };
        check_amf(result, &format!("Data::GetProperty({})", name))?;
        Ok(variant.data[0] as i64)
    }
}

impl Drop for AmfData {
    fn drop(&mut self) {
        // SAFETY: `ptr` is live and we own one reference; nothing uses it after.
        unsafe {
            (self.vtbl().Release)(self.ptr);
        }
    }
}

/// Owned reference to an `AMFBuffer`, the `AMFData` subtype encoders output.
struct AmfBuffer(AmfData);

impl AmfBuffer {
    /// # Safety
    ///
    /// `ptr` must be null or a live `AMFBuffer` whose reference is transferred
    /// to the returned wrapper.
    unsafe fn from_raw(ptr: *mut AMFDataObj) -> Option<Self> {
        (!ptr.is_null()).then_some(Self(AmfData { ptr }))
    }

    fn vtbl(&self) -> &AMFBufferVtbl {
        // SAFETY: the object is a live `AMFBuffer`, whose vtable is the
        // `AMFData` vtable followed by the buffer methods (`AMFBufferVtbl`).
        unsafe { &*((*self.0.ptr).vtbl as *const AMFBufferVtbl) }
    }

    /// Copy the buffer's bytes out.
    fn to_vec(&self) -> Vec<u8> {
        let this = self.0.ptr as *mut AMFBufferObj;
        // SAFETY: `this` is a live buffer; the calls take no other arguments.
        let size = unsafe { (self.vtbl().GetSize)(this) };
        // SAFETY: as above.
        let native = unsafe { (self.vtbl().GetNative)(this) };
        if native.is_null() || size == 0 {
            return Vec::new();
        }
        // SAFETY: `GetNative` points at `GetSize` bytes of host memory owned by
        // the buffer, which stays alive while `self` holds its reference.
        unsafe { std::slice::from_raw_parts(native as *const u8, size).to_vec() }
    }
}

impl std::ops::Deref for AmfBuffer {
    type Target = AmfData;

    fn deref(&self) -> &AmfData {
        &self.0
    }
}

const AMF_TIMEBASE_PER_SECOND: u64 = 10_000_000;
const H264_MAX_AU_TENTHS_OF_A_SECOND: u64 = 10;
/// Opened shared textures kept per session; one slot of headroom over the
/// capture ring so every in-rotation handle stays cached.
const MAX_CACHED_SHARED_TEXTURES: usize = DXGI_SHARED_TEXTURE_RING_SIZE + 1;
const _: () = assert!(MAX_CACHED_SHARED_TEXTURES >= DXGI_SHARED_TEXTURE_RING_SIZE);

/// Return the configured frame duration in AMF's 100 ns timebase.
fn frame_duration_100ns(framerate: u32) -> i64 {
    (AMF_TIMEBASE_PER_SECOND / u64::from(framerate.max(1))).max(1) as i64
}

/// Keep the VBV formula identical between session setup and bitrate updates.
fn vbv_buffer_size(bitrate_bps: i64, framerate: u32, frame_count: u64) -> i64 {
    let frames = frame_count.max(1);
    (bitrate_bps.max(0) as u64)
        .saturating_mul(frames)
        .checked_div(u64::from(framerate.max(1)))
        .unwrap_or(0)
        .max(1)
        .min(i64::MAX as u64) as i64
}

/// Cap one access unit at roughly one tenth of a second of target bitrate.
///
/// AMF's MaxAUSize property is expressed in bits. At 10 Mbps this is
/// 1,000,000 bits, or approximately 125 KiB. AMF support is queried at
/// runtime because MaxAUSize is not present on every driver.
fn max_au_size_bits(bitrate_bps: i64) -> i64 {
    (bitrate_bps.max(0) as u64)
        .checked_div(H264_MAX_AU_TENTHS_OF_A_SECOND)
        .unwrap_or(0)
        .max(1)
        .min(i64::MAX as u64) as i64
}

/// Convert capture timing into AMF's 100 ns PTS units.
///
/// Capture timestamps are monotonic and may include idle gaps, so use their
/// elapsed time when available. A frame-count-derived duration is retained as
/// the fallback for a missing or backwards timestamp.
fn pts_from_frame_timing(
    frame_index: u64,
    timestamp: Instant,
    first_timestamp: Option<Instant>,
    frame_duration: i64,
) -> (i64, Instant) {
    let origin = first_timestamp.unwrap_or(timestamp);
    let fallback = frame_index
        .saturating_sub(1)
        .saturating_mul(frame_duration.max(1) as u64)
        .min(i64::MAX as u64) as i64;
    let pts = timestamp
        .checked_duration_since(origin)
        .map(|elapsed| {
            (elapsed.as_nanos() / 100) // AMF uses 100 ns units.
                .min(i64::MAX as u128) as i64
        })
        .unwrap_or(fallback);
    (pts, origin)
}

fn clamp_monotonic_pts(pts: i64, previous_pts: i64) -> i64 {
    pts.max(previous_pts)
}

#[derive(Debug, Clone, Copy)]
enum IdrTrigger {
    ForcedRequest,
    PeriodicSafetyNet,
    EncoderRebuild,
}

impl fmt::Display for IdrTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::ForcedRequest => "forced-request",
            Self::PeriodicSafetyNet => "periodic-safety-net",
            Self::EncoderRebuild => "encoder-rebuild",
        })
    }
}

// ─── Public encoder ─────────────────────────────────────────────────────────

/// AMD AMF hardware encoder (Windows).
///
/// Dynamically loads `amfrt64.dll` and uses the AMF COM-like API to access
/// AMD VCE/VCN encoding hardware.
pub struct AmfEncoder {
    _library: Arc<AmfLibrary>,
    factory: AmfFactory,
    runtime_version: u64,
}

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
            // AMD VCN caps H.264 (AVC) encoding at 4096x4096; larger frames
            // fail session creation with AMF_INVALID_ARG on FrameSize.
            max_resolution: Resolution::new(4096, 4096),
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
        self.create_session_with_device(config, None)
    }

    fn create_session_with_device(
        &self,
        config: EncodeConfig,
        capture_device: Option<GpuDeviceHandle>,
    ) -> Result<Box<dyn EncodeSession>> {
        self.validate_config(&config)?;
        Ok(Box::new(AmfSession::new(
            self.factory,
            self.runtime_version,
            config,
            capture_device,
        )?))
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
    _capture_device: Option<GpuDeviceHandle>,
    config: EncodeConfig,
    frame_index: u64,
    pending_idr_trigger: Option<IdrTrigger>,
    active_idr_trigger: Option<IdrTrigger>,
    first_capture_timestamp: Option<Instant>,
    frame_duration_100ns: i64,
    last_pts_amf: i64,
    surface_format: AMF_SURFACE_FORMAT,
    /// Force an IDR every this many frames (workaround: AMF ignores H264_IDR_PERIOD)
    idr_interval: u64,
    /// Cached shared textures opened via OpenSharedResource (keyed by handle value)
    cached_shared_textures: Vec<(u64, ID3D11Texture2D)>,
}

fn log_amf_encode_adapter(device: &ID3D11Device) {
    let dxgi_device: IDXGIDevice = match device.cast() {
        Ok(device) => device,
        Err(error) => {
            tracing::warn!("AMF encoder adapter lookup: IDXGIDevice cast failed: {}", error);
            return;
        }
    };
    // SAFETY: `dxgi_device` is a live COM interface; the call has no arguments.
    let adapter = match unsafe { dxgi_device.GetAdapter() } {
        Ok(adapter) => adapter,
        Err(error) => {
            tracing::warn!("AMF encoder adapter lookup: GetAdapter failed: {}", error);
            return;
        }
    };
    let adapter: IDXGIAdapter1 = match adapter.cast() {
        Ok(adapter) => adapter,
        Err(error) => {
            tracing::warn!("AMF encoder adapter lookup: IDXGIAdapter1 cast failed: {}", error);
            return;
        }
    };
    // SAFETY: `adapter` is a live COM interface; the call has no arguments.
    let desc = match unsafe { adapter.GetDesc1() } {
        Ok(desc) => desc,
        Err(error) => {
            tracing::warn!("AMF encoder adapter lookup: GetDesc1 failed: {}", error);
            return;
        }
    };
    let adapter_luid =
        u64::from(desc.AdapterLuid.LowPart) | ((desc.AdapterLuid.HighPart as i64 as u64) << 32);
    let adapter_name_len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
    let adapter_name = String::from_utf16_lossy(&desc.Description[..adapter_name_len]);

    tracing::info!(
        "AMF DXGI Adapter: {} (LUID=0x{:016X}, VendorID=0x{:04X}, DeviceID=0x{:04X}, VRAM={} MB)",
        adapter_name,
        adapter_luid,
        desc.VendorId,
        desc.DeviceId,
        desc.DedicatedVideoMemory / (1024 * 1024),
    );
}

impl AmfSession {
    fn new(
        factory: AmfFactory,
        runtime_version: u64,
        config: EncodeConfig,
        capture_device: Option<GpuDeviceHandle>,
    ) -> Result<Self> {
        tracing::info!(
            "Creating AMF session: {} {}@{}fps {}kbps",
            config.codec,
            config.resolution,
            config.framerate,
            config.bitrate_kbps,
        );

        // 1. Create context and init DX11
        let context = factory.create_context()?;

        let init_device = capture_device.as_ref().map(GpuDeviceHandle::d3d11_device);
        // Use the capture device when available so shared textures and AMF
        // encode work stay on the adapter that owns the captured output.
        context.init_dx11(init_device.as_ref().map_or(ptr::null_mut(), |device| {
            device.as_raw() as *mut std::ffi::c_void
        }))?;
        tracing::debug!(
            "AMF context initialized with {} DX11 device",
            if init_device.is_some() {
                "capture"
            } else {
                "AMF-selected"
            }
        );

        // Retrieve the internal D3D11 device to support OpenSharedResource
        let d3d11_device = context.get_dx11_device()?;
        log_amf_encode_adapter(&d3d11_device);

        // 2. Create encoder component
        let codec_id = match config.codec {
            VideoCodec::H264 => AMF_VIDEO_ENCODER_VCE_AVC,
            VideoCodec::H265 => AMF_VIDEO_ENCODER_HEVC,
            VideoCodec::Av1 => AMF_VIDEO_ENCODER_AV1,
        };
        let encoder = factory.create_component(&context, codec_id)?;

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
        let frame_duration = frame_duration_100ns(config.framerate);
        let hrd_status;
        let filler_status;
        let header_insertion_status;
        let max_au_status;
        let min_qp_status;
        let max_qp_status;
        let vbaq_status;
        let preanalysis_status;

        match config.codec {
            VideoCodec::H264 => {
                // Switch to LOW_LATENCY usage (2) - used by Sunshine as workaround for AMF issues
                encoder.set_property_int64(H264_USAGE, H264_USAGE_LOW_LATENCY)?;
                encoder.set_property_size(H264_FRAMESIZE, w, h)?;
                encoder.set_property_rate(H264_FRAMERATE, config.framerate, 1)?;
                encoder.set_property_int64(H264_PROFILE, H264_PROFILE_HIGH)?;
                // Quality costs little on VCN and is the cheapest fidelity
                // improvement for motion-heavy desktop/video content.
                encoder.set_property_int64(H264_QUALITY_PRESET, H264_QUALITY_QUALITY)?;
                if let Err(e) = encoder.set_property_bool(H264_LOWLATENCY_MODE, true) {
                    tracing::debug!("AMF: LowLatencyInternal not supported: {}", e);
                }

                // Rate control
                let rc = match config.rate_control {
                    RateControlMode::Cbr => H264_RC_CBR,
                    RateControlMode::Vbr => H264_RC_VBR_LATENCY,
                    RateControlMode::Cqp => H264_RC_CQP,
                };
                encoder.set_property_int64(H264_RATE_CONTROL_METHOD, rc)?;
                encoder.set_property_int64(H264_TARGET_BITRATE, bitrate_bps)?;
                encoder.set_property_int64(H264_PEAK_BITRATE, bitrate_bps * 3 / 2)?;

                // A three-frame VBV lets complex frames borrow bits from
                // neighbouring frames while adding only ~50 ms at 60 fps.
                let vbv_size = vbv_buffer_size(bitrate_bps, config.framerate, 3);
                encoder.set_property_int64(H264_VBV_BUFFER_SIZE, vbv_size)?;

                // Streaming optimizations
                filler_status = encoder.set_optional_bool(H264_FILLER_DATA, false);
                // IDR_PERIOD must be 0 to allow per-frame ForcePictureType
                encoder.set_property_int64(H264_IDR_PERIOD, 0)?;
                encoder.set_property_int64(H264_B_PIC_PATTERN, 0)?;
                header_insertion_status =
                    encoder.set_optional_int64(H264_HEADER_INSERTION_MODE, HEADER_INSERTION_IDR);
                hrd_status = encoder.set_optional_bool(H264_ENFORCE_HRD, true);
                // Bound the expensive case directly. These properties are
                // absent from some AMF runtimes, so rejection is non-fatal.
                max_au_status =
                    encoder.set_optional_int64(H264_MAX_AU_SIZE, max_au_size_bits(bitrate_bps));
                min_qp_status = encoder.property_support_status(H264_MIN_QP);
                max_qp_status = encoder.property_support_status(H264_MAX_QP);
                // Adaptive quantization: spend bits where the eye notices
                // (text, edges) for better perceived quality per bit. Not
                // supported on all drivers, so failures are non-fatal.
                vbaq_status = encoder.set_optional_bool(H264_VBAQ, true);
                preanalysis_status = encoder.set_optional_bool(H264_PREENCODE, true);
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
                encoder.set_property_int64(HEVC_QUALITY_PRESET, HEVC_QUALITY_BALANCED)?;

                // Rate control
                let rc = match config.rate_control {
                    RateControlMode::Cbr => HEVC_RC_CBR,
                    RateControlMode::Vbr => HEVC_RC_VBR_LATENCY,
                    RateControlMode::Cqp => HEVC_RC_CQP,
                };
                encoder.set_property_int64(HEVC_RATE_CONTROL_METHOD, rc)?;
                encoder.set_property_int64(HEVC_TARGET_BITRATE, bitrate_bps)?;
                encoder.set_property_int64(HEVC_PEAK_BITRATE, bitrate_bps * 3 / 2)?;

                let vbv_size = vbv_buffer_size(bitrate_bps, config.framerate, 1);
                encoder.set_property_int64(HEVC_VBV_BUFFER_SIZE, vbv_size)?;

                filler_status = encoder.set_optional_bool(HEVC_FILLER_DATA, false);
                header_insertion_status = PropertyStatus::NotApplicable;
                max_au_status = PropertyStatus::NotApplicable;
                min_qp_status = PropertyStatus::NotApplicable;
                max_qp_status = PropertyStatus::NotApplicable;
                
                // Picture control
                encoder.set_property_int64(HEVC_GOP_SIZE, config.framerate as i64)?;
                encoder.set_property_int64(HEVC_MAX_NUM_REFRAMES, config.max_ref_frames as i64)?;
                hrd_status = encoder.set_optional_bool(HEVC_ENFORCE_HRD, true);
                vbaq_status = encoder.set_optional_bool(HEVC_VBAQ, true);
                preanalysis_status = PropertyStatus::NotApplicable;
            }
            VideoCodec::Av1 => {
                encoder.set_property_int64(AV1_USAGE, AV1_USAGE_ULTRA_LOW_LATENCY)?;
                encoder.set_property_size(AV1_FRAMESIZE, w, h)?;
                encoder.set_property_rate(AV1_FRAMERATE, config.framerate, 1)?;
                encoder.set_property_int64(AV1_PROFILE, AV1_PROFILE_MAIN)?;
                encoder.set_property_int64(AV1_QUALITY_PRESET, AV1_QUALITY_BALANCED)?;
                
                // Rate control
                let rc = match config.rate_control {
                    RateControlMode::Cbr => AV1_RC_CBR,
                    RateControlMode::Vbr => AV1_RC_VBR_LATENCY,
                    RateControlMode::Cqp => AV1_RC_CQP,
                };
                encoder.set_property_int64(AV1_RATE_CONTROL_METHOD, rc)?;
                encoder.set_property_int64(AV1_TARGET_BITRATE, bitrate_bps)?;
                encoder.set_property_int64(AV1_PEAK_BITRATE, bitrate_bps * 3 / 2)?;
                
                encoder.set_property_int64(
                    AV1_VBV_BUFFER_SIZE,
                    vbv_buffer_size(bitrate_bps, config.framerate, 1),
                )?;
                filler_status = encoder.set_optional_bool(AV1_FILLER_DATA, false);
                header_insertion_status = PropertyStatus::NotApplicable;
                hrd_status = encoder.set_optional_bool(AV1_ENFORCE_HRD, true);
                max_au_status = PropertyStatus::NotApplicable;
                min_qp_status = PropertyStatus::NotApplicable;
                max_qp_status = PropertyStatus::NotApplicable;
                vbaq_status = PropertyStatus::NotApplicable;
                preanalysis_status = PropertyStatus::NotApplicable;
            }
        }

        // 5. Initialize the encoder
        encoder.init(surface_format, w, h)?;
        tracing::info!(
            "AMF effective config: backend=AMF runtime={}.{}.{}.{} codec={} resolution={} framerate={} target_bps={} peak_bps={} vbv_bits={} hrd={} filler={} header_insertion={} MaxAUSize={} MinQP={} MaxQP={} VBAQ={} preanalysis={}",
            amf_get_major(runtime_version),
            amf_get_minor(runtime_version),
            amf_get_subminor(runtime_version),
            amf_get_build(runtime_version),
            config.codec,
            config.resolution,
            config.framerate.max(1),
            bitrate_bps,
            bitrate_bps * 3 / 2,
            vbv_buffer_size(
                bitrate_bps,
                config.framerate,
                if config.codec == VideoCodec::H264 { 3 } else { 1 },
            ),
            hrd_status,
            filler_status,
            header_insertion_status,
            max_au_status,
            min_qp_status,
            max_qp_status,
            vbaq_status,
            preanalysis_status,
        );

        // AMF's IDR_PERIOD property is unreliable on many driver versions.
        // Safety-net IDR every 10 minutes. Real IDR requests come from
        // new client connections via request_idr().
        let idr_interval = u64::from(config.framerate.max(1)).saturating_mul(600);

        Ok(Self {
            context,
            encoder,
            d3d11_device,
            _capture_device: capture_device,
            config,
            frame_index: 0,
            pending_idr_trigger: None,
            active_idr_trigger: None,
            first_capture_timestamp: None,
            frame_duration_100ns: frame_duration,
            last_pts_amf: 0,
            surface_format,
            idr_interval,
            cached_shared_textures: Vec::new(),
        })
    }

    /// Open (or return cached) shared texture from a DXGI shared handle.
    fn open_shared_texture(&mut self, handle_val: u64) -> Result<&ID3D11Texture2D> {
        if let Some(index) = self
            .cached_shared_textures
            .iter()
            .position(|(cached_handle, _)| *cached_handle == handle_val)
        {
            return Ok(&self.cached_shared_textures[index].1);
        }

        let handle = windows::Win32::Foundation::HANDLE(handle_val as *mut std::ffi::c_void);
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: `handle` is a DXGI shared handle from the capture side's
        // texture ring and `texture` is a valid out-param.
        unsafe { self.d3d11_device.OpenSharedResource(handle, &mut texture) }.map_err(|e| FluxError::Encode {
            frame: self.frame_index,
            reason: format!("OpenSharedResource failed: {}", e),
        })?;
        let texture = texture.ok_or_else(|| FluxError::Encode {
            frame: self.frame_index,
            reason: "OpenSharedResource returned null texture".into(),
        })?;
        self.cached_shared_textures.push((handle_val, texture));
        if self.cached_shared_textures.len() > MAX_CACHED_SHARED_TEXTURES {
            self.cached_shared_textures.remove(0);
        }

        let index = self
            .cached_shared_textures
            .iter()
            .position(|(cached_handle, _)| *cached_handle == handle_val)
            .expect("opened shared texture is cached");
        Ok(&self.cached_shared_textures[index].1)
    }

    /// Extract bitstream from an AMFBuffer output.
    fn extract_output(&mut self, data: &AmfBuffer) -> EncodedPacket {
        let bitstream = data.to_vec();

        // Primary: read OutputDataType from the output buffer (authoritative, like OBS).
        // Fallback: scan for IDR NALU types in the bitstream.
        let output_type_prop = match self.config.codec {
            VideoCodec::H264 => H264_OUTPUT_DATA_TYPE,
            VideoCodec::H265 => HEVC_OUTPUT_DATA_TYPE,
            VideoCodec::Av1 => H264_OUTPUT_DATA_TYPE, // fallback, AV1 uses different prop
        };
        let is_keyframe = match data.get_property_int64(output_type_prop) {
            Ok(t) => t == OUTPUT_DATA_TYPE_IDR,
            Err(_) => contains_idr_nalu(&bitstream, self.config.codec),
        };
        if is_keyframe {
            let trigger = self
                .active_idr_trigger
                .take()
                .map(|t| t.to_string())
                .unwrap_or_else(|| "unknown".into());
            tracing::info!(
                "AMF IDR produced: {} bytes trigger={}",
                bitstream.len(),
                trigger
            );
        }

        EncodedPacket {
            frame_index: self.frame_index,
            pts: self.frame_index,
            is_keyframe,
            data: bitstream,
        }
    }

    /// Poll encoder output, collecting all available packets.
    fn drain_output(&mut self) -> Vec<EncodedPacket> {
        let mut packets = Vec::new();
        while let (AMF_OK, Some(data)) = self.encoder.query_output() {
            packets.push(self.extract_output(&data));
        }
        packets
    }
}

impl EncodeSession for AmfSession {
    fn encode(&mut self, frame: Arc<CapturedFrame>) -> Result<Vec<EncodedPacket>> {
        let frame = &*frame;
        self.frame_index += 1;

        // Periodic IDR: force keyframe every idr_interval frames
        if self.frame_index % self.idr_interval == 1 {
            if self.pending_idr_trigger.is_none() {
                self.pending_idr_trigger = Some(IdrTrigger::PeriodicSafetyNet);
            }
        }

        self.active_idr_trigger = self.pending_idr_trigger.take();
        let is_idr = self.active_idr_trigger.is_some();

        // Acquire an input surface
        let surface = if let Some(GpuFrameHandle::DxgiSharedTexture(ref dxgi)) = frame.gpu_handle {
            // Zero-copy path: use cached shared texture reference
            let texture = self.open_shared_texture(dxgi.handle)?;
            let tex_ptr = Interface::as_raw(texture);
            // SAFETY: `texture` is a live ID3D11Texture2D opened on the context's
            // device and stays open in `cached_shared_textures`. That the capture
            // side does not rewrite it before AMF finishes reading relies on the
            // `DXGI_SHARED_TEXTURE_RING_SIZE` ring, not on anything checked here.
            unsafe { self.context.create_surface_from_dx11(tex_ptr) }?
        } else if !frame.data.is_empty() {
            // CPU path: wrap host memory as an AMF surface (no extra copy).
            // For BGRA: stride = width * 4, vPitch = height.
            let w = self.config.resolution.width as i32;
            let h = self.config.resolution.height as i32;
            let h_pitch = frame.stride as i32; // bytes per row
            let v_pitch = h;                   // number of rows

            // SAFETY: `frame.data` holds `stride * height` bytes of the session's
            // surface format and AMF only reads it. AMF wraps rather than copies
            // the memory, so this relies on the encoder having consumed the
            // surface before the caller recycles `frame` after `encode` returns.
            unsafe {
                self.context.create_surface_from_host_native(
                    self.surface_format,
                    w,
                    h,
                    h_pitch,
                    v_pitch,
                    frame.data.as_ptr() as *mut std::ffi::c_void,
                )
            }?
        } else {
            return Err(FluxError::Encode {
                frame: self.frame_index,
                reason: "Frame has no data and no GPU handle".into(),
            });
        };

        // Set capture-timed PTS and duration. AMF uses 100 ns units.
        let (raw_pts_amf, first_timestamp) = pts_from_frame_timing(
            self.frame_index,
            frame.timestamp,
            self.first_capture_timestamp,
            self.frame_duration_100ns,
        );
        self.first_capture_timestamp = Some(first_timestamp);
        let pts_amf = clamp_monotonic_pts(raw_pts_amf, self.last_pts_amf);
        self.last_pts_amf = pts_amf;
        surface.set_pts(pts_amf);
        surface.set_duration(self.frame_duration_100ns);

        // Force IDR if requested (IDR_PERIOD=0 is set in init for LOW_LATENCY mode)
        if is_idr {
            let force_type_prop = match self.config.codec {
                VideoCodec::H264 => H264_FORCE_PICTURE_TYPE,
                VideoCodec::H265 => HEVC_FORCE_PICTURE_TYPE,
                VideoCodec::Av1 => AV1_FORCE_PICTURE_TYPE,
            };
            // Set INSERT_SPS + INSERT_PPS so decoders can start from any IDR
            if self.config.codec == VideoCodec::H264 {
                let _ = surface.set_property_int64(H264_INSERT_SPS, 1);
                let _ = surface.set_property_int64(H264_INSERT_PPS, 1);
            }
            match surface.set_property_int64(force_type_prop, PICTURE_TYPE_IDR) {
                Ok(()) => tracing::debug!("AMF: SetProperty({}) = IDR({}) on surface OK", force_type_prop, PICTURE_TYPE_IDR),
                Err(e) => tracing::warn!("AMF: SetProperty({}) failed: {}", force_type_prop, e),
            }
        }

        // Submit input — may return AMF_INPUT_FULL, in which case we poll output first
        let mut submit_result = self.encoder.submit_input(&surface);
        let mut packets = Vec::new();

        if submit_result == AMF_INPUT_FULL {
            // Drain output before retrying
            packets.extend(self.drain_output());
            submit_result = self.encoder.submit_input(&surface);
        }

        if submit_result != AMF_OK {
            return Err(FluxError::Encode {
                frame: self.frame_index,
                reason: format!("AMF SubmitInput failed: {}", submit_result),
            });
        }

        // Release the input surface — AMF has its own internal reference now.
        // Without this, every frame leaks an AMF surface object.
        drop(surface);

        // Poll output
        packets.extend(self.drain_output());

        Ok(packets)
    }

    fn request_idr(&mut self) {
        tracing::debug!("AMF: IDR frame requested for next encode");
        self.pending_idr_trigger = Some(IdrTrigger::ForcedRequest);
    }

    fn request_idr_for_rebuild(&mut self) {
        tracing::debug!("AMF: IDR frame requested for encoder rebuild");
        self.pending_idr_trigger = Some(IdrTrigger::EncoderRebuild);
    }

    fn set_framerate(&mut self, framerate: u32) -> Result<()> {
        let framerate = framerate.max(1);
        let property = match self.config.codec {
            VideoCodec::H264 => H264_FRAMERATE,
            VideoCodec::H265 => HEVC_FRAMERATE,
            VideoCodec::Av1 => AV1_FRAMERATE,
        };
        let status = self.encoder.set_optional_rate(property, framerate, 1);
        tracing::info!(
            "AMF: runtime framerate {} -> {} ({})",
            property,
            framerate,
            status
        );
        self.config.framerate = framerate;
        self.frame_duration_100ns = frame_duration_100ns(framerate);
        Ok(())
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        tracing::debug!("AMF: flushing encoder");

        // Signal end-of-stream
        let drain_result = self.encoder.drain();
        if drain_result != AMF_OK && drain_result != AMF_EOF {
            tracing::warn!("AMF Drain returned {}", drain_result);
        }

        let packets = self.drain_output();

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
                self.encoder.set_property_int64(
                    H264_VBV_BUFFER_SIZE,
                    vbv_buffer_size(bitrate_bps, self.config.framerate, 3),
                )?;
                self.encoder
                    .set_optional_int64(H264_MAX_AU_SIZE, max_au_size_bits(bitrate_bps));
            }
            VideoCodec::H265 => {
                self.encoder.set_property_int64(HEVC_TARGET_BITRATE, bitrate_bps)?;
                self.encoder.set_property_int64(HEVC_PEAK_BITRATE, peak_bps)?;
                self.encoder.set_property_int64(
                    HEVC_VBV_BUFFER_SIZE,
                    vbv_buffer_size(bitrate_bps, self.config.framerate, 1),
                )?;
            }
            VideoCodec::Av1 => {
                self.encoder.set_property_int64(AV1_TARGET_BITRATE, bitrate_bps)?;
                self.encoder.set_property_int64(AV1_PEAK_BITRATE, peak_bps)?;
                self.encoder.set_property_int64(
                    AV1_VBV_BUFFER_SIZE,
                    vbv_buffer_size(bitrate_bps, self.config.framerate, 1),
                )?;
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
    use std::time::Duration;

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

    #[test]
    fn max_au_size_is_one_tenth_second_of_target_bitrate() {
        assert_eq!(max_au_size_bits(10_000_000), 1_000_000);
        assert_eq!(max_au_size_bits(6_827_000), 682_700);
        assert_eq!(max_au_size_bits(0), 1);
    }

    #[test]
    fn vbv_buffer_recomputes_from_bitrate_and_frame_rate() {
        assert_eq!(vbv_buffer_size(10_000_000, 60, 3), 500_000);
        assert_eq!(vbv_buffer_size(6_827_000, 60, 3), 341_350);
        assert_eq!(vbv_buffer_size(10_000_000, 0, 1), 10_000_000);
    }

    #[test]
    fn pts_uses_capture_timing_and_falls_back_to_frame_duration() {
        let origin = Instant::now();
        let frame_duration = frame_duration_100ns(60);

        let (first_pts, first_origin) =
            pts_from_frame_timing(1, origin, None, frame_duration);
        assert_eq!(first_pts, 0);
        assert_eq!(first_origin, origin);

        let (timed_pts, _) = pts_from_frame_timing(
            2,
            origin + Duration::from_millis(100),
            Some(origin),
            frame_duration,
        );
        assert_eq!(timed_pts, 1_000_000);

        let (fallback_pts, _) = pts_from_frame_timing(
            3,
            origin - Duration::from_millis(1),
            Some(origin),
            frame_duration,
        );
        assert_eq!(fallback_pts, frame_duration * 2);
        assert_eq!(clamp_monotonic_pts(1, frame_duration), frame_duration);
    }
}
