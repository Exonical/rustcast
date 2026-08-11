//! Raw IddCx bindings generated at build time from the WDK headers.
//!
//! IddCx's API surface dispatches through the `IddFunctions` table exported
//! by IddCxStub.lib; the wrappers below fetch each entry point from the
//! table (by its IDDFUNCENUM index) and call it with `IddDriverGlobals`,
//! mirroring the FORCEINLINE dispatch stubs in IddCx.h.

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]
#![allow(dead_code)]
#![allow(clippy::all)]

include!(concat!(env!("OUT_DIR"), "/iddcx_bindings.rs"));
include!(concat!(env!("OUT_DIR"), "/iddcx_version.rs"));

/// IddCxStub.lib resolves this from the client driver; it declares the
/// minimum IddCx version the driver requires (IDDCX_MINIMUM_VERSION_REQUIRED).
#[unsafe(no_mangle)]
pub static IddMinimumVersionRequired: u32 = FLUX_IDDCX_MINIMUM_VERSION_REQUIRED;

macro_rules! iddcx_call {
    ($name:ident: $pfn:ident @ $idx:ident ( $($arg:ident : $ty:ty),* $(,)? )) => {
        pub unsafe fn $name($($arg: $ty),*) -> NTSTATUS {
            unsafe {
                let f = (*(&raw const IddFunctions))
                    .as_ptr()
                    .add($idx as usize)
                    .cast::<$pfn>()
                    .read()
                    .expect(concat!(stringify!($name), " missing from IddCx function table"));
                f(IddDriverGlobals, $($arg),*)
            }
        }
    };
}

macro_rules! iddcx_function_available {
    ($pfn:ident @ $idx:ident) => {{
        unsafe {
            (*(&raw const IddFunctions))
                .as_ptr()
                .add($idx as usize)
                .cast::<$pfn>()
                .read()
                .is_some()
        }
    }};
}

iddcx_call!(IddCxDeviceInitConfig: PFN_IDDCXDEVICEINITCONFIG @ _IDDFUNCENUM_IddCxDeviceInitConfigTableIndex(
    device_init: *mut WDFDEVICE_INIT,
    config: *const IDD_CX_CLIENT_CONFIG,
));
iddcx_call!(IddCxDeviceInitialize: PFN_IDDCXDEVICEINITIALIZE @ _IDDFUNCENUM_IddCxDeviceInitializeTableIndex(
    device: WDFDEVICE,
));
iddcx_call!(IddCxAdapterInitAsync: PFN_IDDCXADAPTERINITASYNC @ _IDDFUNCENUM_IddCxAdapterInitAsyncTableIndex(
    in_args: *const IDARG_IN_ADAPTER_INIT,
    out_args: *mut IDARG_OUT_ADAPTER_INIT,
));
iddcx_call!(IddCxAdapterSetRenderAdapter: PFN_IDDCXADAPTERSETRENDERADAPTER @ _IDDFUNCENUM_IddCxAdapterSetRenderAdapterTableIndex(
    adapter: IDDCX_ADAPTER,
    in_args: *const IDARG_IN_ADAPTERSETRENDERADAPTER,
));

/// Rust equivalent of the IddCx `IDD_IS_FUNCTION_AVAILABLE` macro.
///
/// The macro is defined in the WDK header and is not emitted by bindgen. It
/// checks the function table entry so a 1.4-built driver can run on older
/// IddCx versions without dereferencing an unavailable entry point.
pub unsafe fn idd_cx_adapter_set_render_adapter_available() -> bool {
    iddcx_function_available!(
        PFN_IDDCXADAPTERSETRENDERADAPTER @
        _IDDFUNCENUM_IddCxAdapterSetRenderAdapterTableIndex
    )
}
iddcx_call!(IddCxMonitorCreate: PFN_IDDCXMONITORCREATE @ _IDDFUNCENUM_IddCxMonitorCreateTableIndex(
    adapter: IDDCX_ADAPTER,
    in_args: *const IDARG_IN_MONITORCREATE,
    out_args: *mut IDARG_OUT_MONITORCREATE,
));
iddcx_call!(IddCxMonitorArrival: PFN_IDDCXMONITORARRIVAL @ _IDDFUNCENUM_IddCxMonitorArrivalTableIndex(
    monitor: IDDCX_MONITOR,
    out_args: *mut IDARG_OUT_MONITORARRIVAL,
));
iddcx_call!(IddCxMonitorDeparture: PFN_IDDCXMONITORDEPARTURE @ _IDDFUNCENUM_IddCxMonitorDepartureTableIndex(
    monitor: IDDCX_MONITOR,
));
iddcx_call!(IddCxSwapChainSetDevice: PFN_IDDCXSWAPCHAINSETDEVICE @ _IDDFUNCENUM_IddCxSwapChainSetDeviceTableIndex(
    swapchain: IDDCX_SWAPCHAIN,
    in_args: *const IDARG_IN_SWAPCHAINSETDEVICE,
));
iddcx_call!(IddCxSwapChainReleaseAndAcquireBuffer: PFN_IDDCXSWAPCHAINRELEASEANDACQUIREBUFFER @ _IDDFUNCENUM_IddCxSwapChainReleaseAndAcquireBufferTableIndex(
    swapchain: IDDCX_SWAPCHAIN,
    out_args: *mut IDARG_OUT_RELEASEANDACQUIREBUFFER,
));
iddcx_call!(IddCxSwapChainFinishedProcessingFrame: PFN_IDDCXSWAPCHAINFINISHEDPROCESSINGFRAME @ _IDDFUNCENUM_IddCxSwapChainFinishedProcessingFrameTableIndex(
    swapchain: IDDCX_SWAPCHAIN,
));
