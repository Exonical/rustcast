//! Raw FFI bindings for the AMD Advanced Media Framework (AMF) runtime.
//!
//! AMF uses a COM-like interface model. We dynamically load `amfrt64.dll` at
//! runtime and resolve two entry points:
//!   - `AMFQueryVersion` — returns the runtime version
//!   - `AMFInit` — returns an `AMFFactory` vtable pointer
//!
//! All AMF objects are reference-counted via `AMFInterface` (AddRef/Release).
//! We model vtables as `#[repr(C)]` structs of function pointers matching the
//! exact C++ vtable layout emitted by MSVC.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

// ─── Result type ────────────────────────────────────────────────────────────

/// AMF_RESULT — every AMF call returns this.
pub type AMF_RESULT = i32;

pub const AMF_OK: AMF_RESULT = 0;
pub const AMF_FAIL: AMF_RESULT = 1;
pub const AMF_EOF: AMF_RESULT = 6;
pub const AMF_REPEAT: AMF_RESULT = 5;
pub const AMF_INPUT_FULL: AMF_RESULT = 4;
pub const AMF_NOT_FOUND: AMF_RESULT = 7;
pub const AMF_NOT_SUPPORTED: AMF_RESULT = 3;
pub const AMF_NEED_MORE_INPUT: AMF_RESULT = 9;

// ─── Primitive types ────────────────────────────────────────────────────────

pub type amf_int32 = i32;
pub type amf_int64 = i64;
pub type amf_uint32 = u32;
pub type amf_uint64 = u64;
pub type amf_size = usize;
pub type amf_bool = bool;
pub type amf_pts = i64;
pub type amf_handle = *mut c_void;

// ─── GUID ───────────────────────────────────────────────────────────────────

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AMFGuid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

// ─── AMFSize / AMFRate / AMFRect ────────────────────────────────────────────

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AMFSize {
    pub width: amf_int32,
    pub height: amf_int32,
}

impl AMFSize {
    pub fn new(w: i32, h: i32) -> Self {
        Self {
            width: w,
            height: h,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AMFRate {
    pub num: amf_uint32,
    pub den: amf_uint32,
}

impl AMFRate {
    pub fn new(num: u32, den: u32) -> Self {
        Self { num, den }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AMFRect {
    pub left: amf_int32,
    pub top: amf_int32,
    pub right: amf_int32,
    pub bottom: amf_int32,
}

// ─── Surface format ─────────────────────────────────────────────────────────

pub type AMF_SURFACE_FORMAT = amf_int32;

pub const AMF_SURFACE_UNKNOWN: AMF_SURFACE_FORMAT = 0;
pub const AMF_SURFACE_NV12: AMF_SURFACE_FORMAT = 1;
pub const AMF_SURFACE_YV12: AMF_SURFACE_FORMAT = 2;
pub const AMF_SURFACE_BGRA: AMF_SURFACE_FORMAT = 3;
pub const AMF_SURFACE_ARGB: AMF_SURFACE_FORMAT = 4;
pub const AMF_SURFACE_RGBA: AMF_SURFACE_FORMAT = 5;
pub const AMF_SURFACE_GRAY8: AMF_SURFACE_FORMAT = 6;
pub const AMF_SURFACE_YUV420P: AMF_SURFACE_FORMAT = 7;
pub const AMF_SURFACE_U8V8: AMF_SURFACE_FORMAT = 8;
pub const AMF_SURFACE_YUY2: AMF_SURFACE_FORMAT = 9;
pub const AMF_SURFACE_P010: AMF_SURFACE_FORMAT = 10;
pub const AMF_SURFACE_RGBA_F16: AMF_SURFACE_FORMAT = 11;
pub const AMF_SURFACE_UYVY: AMF_SURFACE_FORMAT = 12;
pub const AMF_SURFACE_R10G10B10A2: AMF_SURFACE_FORMAT = 13;
pub const AMF_SURFACE_Y210: AMF_SURFACE_FORMAT = 14;
pub const AMF_SURFACE_Y410: AMF_SURFACE_FORMAT = 15;

// ─── Memory type ────────────────────────────────────────────────────────────

pub type AMF_MEMORY_TYPE = amf_int32;

pub const AMF_MEMORY_UNKNOWN: AMF_MEMORY_TYPE = 0;
pub const AMF_MEMORY_HOST: AMF_MEMORY_TYPE = 1;
pub const AMF_MEMORY_DX9: AMF_MEMORY_TYPE = 2;
pub const AMF_MEMORY_DX11: AMF_MEMORY_TYPE = 3;
pub const AMF_MEMORY_OPENCL: AMF_MEMORY_TYPE = 4;
pub const AMF_MEMORY_OPENGL: AMF_MEMORY_TYPE = 5;
pub const AMF_MEMORY_XV: AMF_MEMORY_TYPE = 6;
pub const AMF_MEMORY_GRALLOC: AMF_MEMORY_TYPE = 7;
pub const AMF_MEMORY_COMPUTE_FOR_DX9: AMF_MEMORY_TYPE = 8;
pub const AMF_MEMORY_COMPUTE_FOR_DX11: AMF_MEMORY_TYPE = 9;
pub const AMF_MEMORY_VULKAN: AMF_MEMORY_TYPE = 10;
pub const AMF_MEMORY_DX12: AMF_MEMORY_TYPE = 11;

// ─── Variant type ───────────────────────────────────────────────────────────

pub type AMF_VARIANT_TYPE = amf_int32;

pub const AMF_VARIANT_EMPTY: AMF_VARIANT_TYPE = 0;
pub const AMF_VARIANT_BOOL: AMF_VARIANT_TYPE = 1;
pub const AMF_VARIANT_INT64: AMF_VARIANT_TYPE = 2;
pub const AMF_VARIANT_DOUBLE: AMF_VARIANT_TYPE = 3;
pub const AMF_VARIANT_RECT: AMF_VARIANT_TYPE = 4;
pub const AMF_VARIANT_SIZE: AMF_VARIANT_TYPE = 5;
pub const AMF_VARIANT_POINT: AMF_VARIANT_TYPE = 6;
pub const AMF_VARIANT_RATE: AMF_VARIANT_TYPE = 7;
pub const AMF_VARIANT_RATIO: AMF_VARIANT_TYPE = 8;
pub const AMF_VARIANT_COLOR: AMF_VARIANT_TYPE = 9;
pub const AMF_VARIANT_STRING: AMF_VARIANT_TYPE = 10;
pub const AMF_VARIANT_WSTRING: AMF_VARIANT_TYPE = 11;
pub const AMF_VARIANT_INTERFACE: AMF_VARIANT_TYPE = 12;

// ─── AMFVariantStruct ───────────────────────────────────────────────────────

/// The AMFVariant is a tagged union. On x64, the C layout is:
///   offset 0: type_ (i32, 4 bytes)
///   offset 4: _pad  (4 bytes, alignment padding for the 8-byte-aligned union)
///   offset 8: data  (16 bytes, union — largest member is AMFRect = 4×i32)
/// Total size: 24 bytes.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct AMFVariantStruct {
    pub type_: AMF_VARIANT_TYPE,
    _pad: u32,
    /// 16-byte union data, 8-byte aligned to match C ABI.
    pub data: [u64; 2],
}

impl AMFVariantStruct {
    pub fn from_bool(v: bool) -> Self {
        Self {
            type_: AMF_VARIANT_BOOL,
            _pad: 0,
            data: [v as u64, 0],
        }
    }

    pub fn from_int64(v: i64) -> Self {
        Self {
            type_: AMF_VARIANT_INT64,
            _pad: 0,
            data: [v as u64, 0],
        }
    }

    pub fn from_size(sz: AMFSize) -> Self {
        let mut s = Self {
            type_: AMF_VARIANT_SIZE,
            _pad: 0,
            data: [0u64; 2],
        };
        // Write width (i32) and height (i32) into first 8 bytes of union
        unsafe {
            let ptr = s.data.as_mut_ptr() as *mut amf_int32;
            *ptr = sz.width;
            *ptr.add(1) = sz.height;
        }
        s
    }

    pub fn from_rate(r: AMFRate) -> Self {
        let mut s = Self {
            type_: AMF_VARIANT_RATE,
            _pad: 0,
            data: [0u64; 2],
        };
        // Write num (u32) and den (u32) into first 8 bytes of union
        unsafe {
            let ptr = s.data.as_mut_ptr() as *mut amf_uint32;
            *ptr = r.num;
            *ptr.add(1) = r.den;
        }
        s
    }

    pub fn as_int64(&self) -> i64 {
        debug_assert!(self.type_ == AMF_VARIANT_INT64);
        self.data[0] as i64
    }

    pub fn as_bool(&self) -> bool {
        debug_assert!(self.type_ == AMF_VARIANT_BOOL);
        self.data[0] != 0
    }
}

// ─── Opaque interface pointers ──────────────────────────────────────────────

// All AMF objects are accessed through vtable pointers. We define the vtable
// layouts we actually call, using `#[repr(C)]` to match the MSVC COM ABI.

/// Opaque pointer to an AMFInterface (base class of all AMF objects).
pub type AMFInterface = *mut c_void;

// ─── AMFFactory vtable ──────────────────────────────────────────────────────

/// `IAMFFactory` vtable. This is the root entry point returned by AMFInit().
/// NOTE: AMFFactory does NOT inherit from AMFInterface — it is a standalone singleton.
/// Vtable layout from AMF SDK `Factory.h` (7 methods, NO Acquire/Release/QueryInterface):
#[repr(C)]
pub struct AMFFactoryVtbl {
    pub CreateContext: unsafe extern "system" fn(
        this: *mut AMFFactoryObj,
        ppContext: *mut *mut AMFContextObj,
    ) -> AMF_RESULT,
    pub CreateComponent: unsafe extern "system" fn(
        this: *mut AMFFactoryObj,
        pContext: *mut AMFContextObj,
        id: *const u16, // wide string
        ppComponent: *mut *mut AMFComponentObj,
    ) -> AMF_RESULT,
    pub SetCacheFolder: unsafe extern "system" fn(this: *mut AMFFactoryObj, path: *const u16) -> AMF_RESULT,
    pub GetCacheFolder: unsafe extern "system" fn(this: *mut AMFFactoryObj) -> *const u16,
    pub GetDebug: unsafe extern "system" fn(this: *mut AMFFactoryObj, ppDebug: *mut *mut c_void) -> AMF_RESULT,
    pub GetTrace: unsafe extern "system" fn(this: *mut AMFFactoryObj, ppTrace: *mut *mut c_void) -> AMF_RESULT,
    pub GetPrograms: unsafe extern "system" fn(this: *mut AMFFactoryObj, ppPrograms: *mut *mut c_void) -> AMF_RESULT,
}

#[repr(C)]
pub struct AMFFactoryObj {
    pub vtbl: *const AMFFactoryVtbl,
}

// ─── AMFContext vtable ──────────────────────────────────────────────────────

#[repr(C)]
pub struct AMFContextVtbl {
    // AMFInterface (3 methods)
    pub Acquire: unsafe extern "system" fn(this: *mut AMFContextObj) -> amf_int64,
    pub Release: unsafe extern "system" fn(this: *mut AMFContextObj) -> amf_int64,
    pub QueryInterface: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        iid: *const AMFGuid,
        ppObject: *mut *mut c_void,
    ) -> AMF_RESULT,

    // AMFPropertyStorage (inherited) — 10 methods
    pub SetProperty: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        name: *const u16,
        value: AMFVariantStruct,
    ) -> AMF_RESULT,
    pub GetProperty: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        name: *const u16,
        pValue: *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub HasProperty: unsafe extern "system" fn(this: *mut AMFContextObj, name: *const u16) -> amf_bool,
    pub GetPropertyCount: unsafe extern "system" fn(this: *mut AMFContextObj) -> amf_size,
    pub GetPropertyAt: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        index: amf_size,
        name: *mut u16,
        nameSize: amf_size,
        pValue: *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub Clear: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub AddTo: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        pOther: *mut c_void,
        overwrite: amf_bool,
        deep: amf_bool,
    ) -> AMF_RESULT,
    pub CopyTo: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        pOther: *mut c_void,
        deep: amf_bool,
    ) -> AMF_RESULT,
    pub AddObserver: unsafe extern "system" fn(this: *mut AMFContextObj, pObserver: *mut c_void),
    pub RemoveObserver: unsafe extern "system" fn(this: *mut AMFContextObj, pObserver: *mut c_void),

    // AMFContext methods (inherits AMFPropertyStorage, NOT AMFPropertyStorageEx)
    pub Terminate: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub InitDX9: unsafe extern "system" fn(this: *mut AMFContextObj, pDX9Device: *mut c_void) -> AMF_RESULT,
    pub GetDX9Device: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        dxVersionRequired: AMF_DX_VERSION,
    ) -> *mut c_void,
    pub LockDX9: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub UnlockDX9: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub InitDX11: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        pDX11Device: *mut c_void,
        version: AMF_DX_VERSION,
    ) -> AMF_RESULT,
    pub GetDX11Device: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        version: AMF_DX_VERSION,
    ) -> *mut c_void,
    pub LockDX11: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub UnlockDX11: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub InitOpenCL: unsafe extern "system" fn(this: *mut AMFContextObj, pCommandQueue: *mut c_void) -> AMF_RESULT,
    pub GetOpenCLContext: unsafe extern "system" fn(this: *mut AMFContextObj) -> *mut c_void,
    pub GetOpenCLCommandQueue: unsafe extern "system" fn(this: *mut AMFContextObj) -> *mut c_void,
    pub GetOpenCLDeviceID: unsafe extern "system" fn(this: *mut AMFContextObj) -> *mut c_void,
    pub GetOpenCLComputeFactory: unsafe extern "system" fn(this: *mut AMFContextObj, ppFactory: *mut *mut c_void) -> AMF_RESULT,
    pub InitOpenCLEx: unsafe extern "system" fn(this: *mut AMFContextObj, pDevice: *mut c_void) -> AMF_RESULT,
    pub LockOpenCL: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub UnlockOpenCL: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub InitOpenGL: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        hOpenGLContext: *mut c_void,
        hWnd: *mut c_void,
        hDC: *mut c_void,
    ) -> AMF_RESULT,
    pub GetOpenGLContext: unsafe extern "system" fn(this: *mut AMFContextObj) -> *mut c_void,
    pub GetOpenGLDrawable: unsafe extern "system" fn(this: *mut AMFContextObj) -> *mut c_void,
    pub LockOpenGL: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub UnlockOpenGL: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub InitXV: unsafe extern "system" fn(this: *mut AMFContextObj, pXVDevice: *mut c_void) -> AMF_RESULT,
    pub GetXVDevice: unsafe extern "system" fn(this: *mut AMFContextObj) -> *mut c_void,
    pub LockXV: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub UnlockXV: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub InitGralloc: unsafe extern "system" fn(this: *mut AMFContextObj, pGrallocDevice: *mut c_void) -> AMF_RESULT,
    pub GetGrallocDevice: unsafe extern "system" fn(this: *mut AMFContextObj) -> *mut c_void,
    pub LockGralloc: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,
    pub UnlockGralloc: unsafe extern "system" fn(this: *mut AMFContextObj) -> AMF_RESULT,

    // Allocation
    pub AllocBuffer: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        type_: AMF_MEMORY_TYPE,
        size: amf_size,
        ppBuffer: *mut *mut AMFBufferObj,
    ) -> AMF_RESULT,
    pub AllocSurface: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        type_: AMF_MEMORY_TYPE,
        format: AMF_SURFACE_FORMAT,
        width: amf_int32,
        height: amf_int32,
        ppSurface: *mut *mut AMFSurfaceObj,
    ) -> AMF_RESULT,
    // AllocAudioBuffer omitted — not needed for video encoding
    pub AllocAudioBuffer: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        type_: AMF_MEMORY_TYPE,
        format: amf_int32,
        samples: amf_int32,
        sample_rate: amf_int32,
        channels: amf_int32,
        ppBuffer: *mut *mut c_void,
    ) -> AMF_RESULT,

    // Wrap existing objects (order from Context.h C vtable)
    pub CreateBufferFromHostNative: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        pHostBuffer: *mut c_void,
        size: amf_size,
        ppBuffer: *mut *mut AMFBufferObj,
        pObserver: *mut c_void,
    ) -> AMF_RESULT,
    pub CreateSurfaceFromHostNative: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        format: AMF_SURFACE_FORMAT,
        width: amf_int32,
        height: amf_int32,
        hPitch: amf_int32,
        vPitch: amf_int32,
        pData: *mut c_void,
        ppSurface: *mut *mut AMFSurfaceObj,
        pObserver: *mut c_void,
    ) -> AMF_RESULT,
    pub CreateSurfaceFromDX9Native: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        pDX9Surface: *mut c_void,
        ppSurface: *mut *mut AMFSurfaceObj,
        pObserver: *mut c_void,
    ) -> AMF_RESULT,
    pub CreateSurfaceFromDX11Native: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        pDX11Surface: *mut c_void,
        ppSurface: *mut *mut AMFSurfaceObj,
        pObserver: *mut c_void,
    ) -> AMF_RESULT,
    pub CreateSurfaceFromOpenGLNative: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        format: AMF_SURFACE_FORMAT,
        hGLTextureID: amf_handle,
        ppSurface: *mut *mut AMFSurfaceObj,
        pObserver: *mut c_void,
    ) -> AMF_RESULT,
    pub CreateSurfaceFromGrallocNative: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        handle: amf_handle,
        ppSurface: *mut *mut AMFSurfaceObj,
        pObserver: *mut c_void,
    ) -> AMF_RESULT,
    pub CreateSurfaceFromOpenCLNative: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        format: AMF_SURFACE_FORMAT,
        width: amf_int32,
        height: amf_int32,
        pClPlanes: *mut *mut c_void,
        ppSurface: *mut *mut AMFSurfaceObj,
        pObserver: *mut c_void,
    ) -> AMF_RESULT,
    pub CreateBufferFromOpenCLNative: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        pCLBuffer: *mut c_void,
        size: amf_size,
        ppBuffer: *mut *mut AMFBufferObj,
    ) -> AMF_RESULT,
    pub GetCompute: unsafe extern "system" fn(
        this: *mut AMFContextObj,
        eMemType: AMF_MEMORY_TYPE,
        ppCompute: *mut *mut c_void,
    ) -> AMF_RESULT,
}

#[repr(C)]
pub struct AMFContextObj {
    pub vtbl: *const AMFContextVtbl,
}

pub type AMF_DX_VERSION = amf_int32;
pub const AMF_DX11_0: AMF_DX_VERSION = 110;
pub const AMF_DX11_1: AMF_DX_VERSION = 111;

// ─── AMFComponent vtable (encoder) ──────────────────────────────────────────

#[repr(C)]
pub struct AMFComponentVtbl {
    // AMFInterface (3)
    pub Acquire: unsafe extern "system" fn(this: *mut AMFComponentObj) -> amf_int64,
    pub Release: unsafe extern "system" fn(this: *mut AMFComponentObj) -> amf_int64,
    pub QueryInterface: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        iid: *const AMFGuid,
        ppObject: *mut *mut c_void,
    ) -> AMF_RESULT,

    // AMFPropertyStorage (10)
    pub SetProperty: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        name: *const u16,
        value: AMFVariantStruct,
    ) -> AMF_RESULT,
    pub GetProperty: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        name: *const u16,
        pValue: *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub HasProperty: unsafe extern "system" fn(this: *mut AMFComponentObj, name: *const u16) -> amf_bool,
    pub GetPropertyCount: unsafe extern "system" fn(this: *mut AMFComponentObj) -> amf_size,
    pub GetPropertyAt: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        index: amf_size,
        name: *mut u16,
        nameSize: amf_size,
        pValue: *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub Clear: unsafe extern "system" fn(this: *mut AMFComponentObj) -> AMF_RESULT,
    pub AddTo: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        pOther: *mut c_void,
        overwrite: amf_bool,
        deep: amf_bool,
    ) -> AMF_RESULT,
    pub CopyTo: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        pOther: *mut c_void,
        deep: amf_bool,
    ) -> AMF_RESULT,
    pub AddObserver: unsafe extern "system" fn(this: *mut AMFComponentObj, pObserver: *mut c_void),
    pub RemoveObserver: unsafe extern "system" fn(this: *mut AMFComponentObj, pObserver: *mut c_void),

    // AMFPropertyStorageEx (4 methods — Component inherits PropertyStorageEx)
    pub GetPropertiesInfoCount: unsafe extern "system" fn(this: *mut AMFComponentObj) -> amf_size,
    pub GetPropertyInfoAt: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        index: amf_size,
        ppInfo: *mut *const c_void,
    ) -> AMF_RESULT,
    pub GetPropertyInfo: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        name: *const u16,
        ppInfo: *mut *const c_void,
    ) -> AMF_RESULT,
    pub ValidateProperty: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        name: *const u16,
        value: AMFVariantStruct,
        pOutValidated: *mut AMFVariantStruct,
    ) -> AMF_RESULT,

    // AMFComponent
    pub Init: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        format: AMF_SURFACE_FORMAT,
        width: amf_int32,
        height: amf_int32,
    ) -> AMF_RESULT,
    pub ReInit: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        width: amf_int32,
        height: amf_int32,
    ) -> AMF_RESULT,
    pub Terminate: unsafe extern "system" fn(this: *mut AMFComponentObj) -> AMF_RESULT,
    pub Drain: unsafe extern "system" fn(this: *mut AMFComponentObj) -> AMF_RESULT,
    pub Flush: unsafe extern "system" fn(this: *mut AMFComponentObj) -> AMF_RESULT,
    pub SubmitInput: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        pData: *mut AMFDataObj,
    ) -> AMF_RESULT,
    pub QueryOutput: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        ppData: *mut *mut AMFDataObj,
    ) -> AMF_RESULT,
    pub GetContext: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
    ) -> *mut AMFContextObj,
    pub SetOutputDataAllocatorCB: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        pAllocator: *mut c_void,
    ) -> AMF_RESULT,
    pub GetCaps: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        ppCaps: *mut *mut c_void,
    ) -> AMF_RESULT,
    pub Optimize: unsafe extern "system" fn(
        this: *mut AMFComponentObj,
        pAllocator: *mut c_void,
    ) -> AMF_RESULT,
}

#[repr(C)]
pub struct AMFComponentObj {
    pub vtbl: *const AMFComponentVtbl,
}

// ─── AMFData / AMFSurface / AMFBuffer vtables ───────────────────────────────

// AMFData is the base for AMFBuffer and AMFSurface.
// We use a common minimal vtable for property access and ref counting.

#[repr(C)]
pub struct AMFDataVtbl {
    // AMFInterface (3)
    pub Acquire: unsafe extern "system" fn(this: *mut AMFDataObj) -> amf_int64,
    pub Release: unsafe extern "system" fn(this: *mut AMFDataObj) -> amf_int64,
    pub QueryInterface: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        iid: *const AMFGuid,
        ppObject: *mut *mut c_void,
    ) -> AMF_RESULT,

    // AMFPropertyStorage (10)
    pub SetProperty: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        name: *const u16,
        value: AMFVariantStruct,
    ) -> AMF_RESULT,
    pub GetProperty: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        name: *const u16,
        pValue: *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub HasProperty: unsafe extern "system" fn(this: *mut AMFDataObj, name: *const u16) -> amf_bool,
    pub GetPropertyCount: unsafe extern "system" fn(this: *mut AMFDataObj) -> amf_size,
    pub GetPropertyAt: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        index: amf_size,
        name: *mut u16,
        nameSize: amf_size,
        pValue: *mut AMFVariantStruct,
    ) -> AMF_RESULT,
    pub Clear: unsafe extern "system" fn(this: *mut AMFDataObj) -> AMF_RESULT,
    pub AddTo: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        pOther: *mut c_void,
        overwrite: amf_bool,
        deep: amf_bool,
    ) -> AMF_RESULT,
    pub CopyTo: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        pOther: *mut c_void,
        deep: amf_bool,
    ) -> AMF_RESULT,
    pub AddObserver: unsafe extern "system" fn(this: *mut AMFDataObj, pObserver: *mut c_void),
    pub RemoveObserver: unsafe extern "system" fn(this: *mut AMFDataObj, pObserver: *mut c_void),

    // AMFData (inherits AMFPropertyStorage, NOT AMFPropertyStorageEx — no extra methods)
    pub GetMemoryType: unsafe extern "system" fn(this: *mut AMFDataObj) -> AMF_MEMORY_TYPE,
    pub Duplicate: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        type_: AMF_MEMORY_TYPE,
        ppNewData: *mut *mut AMFDataObj,
    ) -> AMF_RESULT,
    pub Convert: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        type_: AMF_MEMORY_TYPE,
    ) -> AMF_RESULT,
    pub Interop: unsafe extern "system" fn(
        this: *mut AMFDataObj,
        type_: AMF_MEMORY_TYPE,
    ) -> AMF_RESULT,
    pub GetDataType: unsafe extern "system" fn(this: *mut AMFDataObj) -> amf_int32,
    pub IsReusable: unsafe extern "system" fn(this: *mut AMFDataObj) -> amf_bool,
    pub SetPts: unsafe extern "system" fn(this: *mut AMFDataObj, pts: amf_pts),
    pub GetPts: unsafe extern "system" fn(this: *mut AMFDataObj) -> amf_pts,
    pub SetDuration: unsafe extern "system" fn(this: *mut AMFDataObj, duration: amf_pts),
    pub GetDuration: unsafe extern "system" fn(this: *mut AMFDataObj) -> amf_pts,
}

// ─── AMFBuffer vtable slot indices ───────────────────────────────────────────
// AMFBuffer extends AMFData. GetSize and GetNative are AMFBuffer-specific methods,
// NOT on AMFData. We access them via raw vtable slot offsets.
//
// Full AMFBuffer vtable layout (from Buffer.h C vtable):
//   0-2:   AMFInterface (Acquire, Release, QueryInterface)
//   3-12:  AMFPropertyStorage (10 methods)
//   13-22: AMFData (GetMemoryType, Duplicate, Convert, Interop, GetDataType,
//                   IsReusable, SetPts, GetPts, SetDuration, GetDuration)
//   23:    AMFBuffer::SetSize
//   24:    AMFBuffer::GetSize
//   25:    AMFBuffer::GetNative
//   26-27: AMFBuffer::AddObserver_Buffer, RemoveObserver_Buffer

pub const AMFBUFFER_VTABLE_SLOT_GET_SIZE: usize = 24;
pub const AMFBUFFER_VTABLE_SLOT_GET_NATIVE: usize = 25;

#[repr(C)]
pub struct AMFDataObj {
    pub vtbl: *const AMFDataVtbl,
}

// For our purposes AMFSurface and AMFBuffer are accessed via their AMFData
// base vtable. Upcast via reinterpret.
pub type AMFSurfaceObj = AMFDataObj;
pub type AMFBufferObj = AMFDataObj;

// ─── DLL loading ────────────────────────────────────────────────────────────

/// DLL name for 64-bit AMF runtime.
pub const AMF_DLL_NAME: &str = "amfrt64.dll";

/// Function name exports from the DLL.
pub const AMF_INIT_FUNCTION_NAME: &[u8] = b"AMFInit\0";
pub const AMF_QUERY_VERSION_FUNCTION_NAME: &[u8] = b"AMFQueryVersion\0";

/// Minimum AMF version we support (1.4.30 — AV1 ultra low latency).
pub const AMF_MIN_VERSION: amf_uint64 = amf_make_full_version(1, 4, 30, 0);

/// AMF version encoding helpers.
pub const fn amf_make_full_version(major: u64, minor: u64, sub: u64, build: u64) -> u64 {
    (major << 48) | (minor << 32) | (sub << 16) | build
}

pub const fn amf_get_major(version: u64) -> u64 {
    (version >> 48) & 0xFFFF
}

pub const fn amf_get_minor(version: u64) -> u64 {
    (version >> 32) & 0xFFFF
}

pub const fn amf_get_subminor(version: u64) -> u64 {
    (version >> 16) & 0xFFFF
}

pub const fn amf_get_build(version: u64) -> u64 {
    version & 0xFFFF
}

/// Function pointer types for the DLL exports.
pub type AMFInit_Fn = unsafe extern "system" fn(
    version: amf_uint64,
    ppFactory: *mut *mut AMFFactoryObj,
) -> AMF_RESULT;

pub type AMFQueryVersion_Fn = unsafe extern "system" fn(
    pVersion: *mut amf_uint64,
) -> AMF_RESULT;

/// Helper to convert a Rust &str to a null-terminated wide string (UTF-16).
pub fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Check an AMF_RESULT and convert to a Rust Result.
pub fn check_amf(result: AMF_RESULT, context: &str) -> flux_core::error::Result<()> {
    if result == AMF_OK {
        Ok(())
    } else {
        Err(flux_core::error::FluxError::EncoderInit(format!(
            "AMF {} failed with code {}",
            context, result
        )))
    }
}
