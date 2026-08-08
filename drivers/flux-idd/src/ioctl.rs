//! IOCTL interface used by flux-server to plug the virtual monitor in/out.
//!
//! Codes must match `flux-capture`'s Windows IDD controller:
//! - `IOCTL_FLUXIDD_PLUG_IN`  = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900, METHOD_BUFFERED, FILE_WRITE_DATA)
//!   with input `FluxIddMonitorMode { width: u32, height: u32, refresh_hz: u32 }`
//! - `IOCTL_FLUXIDD_PLUG_OUT` = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x901, METHOD_BUFFERED, FILE_WRITE_DATA)

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;

use wdk_sys::{call_unsafe_wdf_function_binding, NTSTATUS, ULONG, WDFREQUEST};

use crate::bindings as idd;

// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, function, METHOD_BUFFERED=0, FILE_WRITE_DATA=2)
// = (0x22 << 16) | (2 << 14) | (function << 2) | 0
const fn ctl_code_write(function: u32) -> u32 {
    (0x22 << 16) | (2 << 14) | (function << 2)
}

pub const IOCTL_FLUXIDD_PLUG_IN: u32 = ctl_code_write(0x900);
pub const IOCTL_FLUXIDD_PLUG_OUT: u32 = ctl_code_write(0x901);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FluxIddMonitorMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

pub unsafe extern "C" fn evt_io_device_control(
    _device: idd::WDFDEVICE,
    request: idd::WDFREQUEST,
    _output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: ULONG,
) {
    let request = request as WDFREQUEST;
    let status: NTSTATUS = match io_control_code {
        IOCTL_FLUXIDD_PLUG_IN => handle_plug_in(request, input_buffer_length),
        IOCTL_FLUXIDD_PLUG_OUT => crate::plug_out_monitor(),
        _ => crate::ERR_INVALID_REQUEST,
    };

    unsafe {
        call_unsafe_wdf_function_binding!(WdfRequestComplete, request, status);
    }
}

fn handle_plug_in(request: WDFREQUEST, input_buffer_length: usize) -> NTSTATUS {
    if input_buffer_length < size_of::<FluxIddMonitorMode>() {
        return crate::ERR_INVALID_PARAM;
    }

    let mut buffer: *mut c_void = ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveInputBuffer,
            request,
            size_of::<FluxIddMonitorMode>(),
            &mut buffer,
            ptr::null_mut(),
        )
    };
    if status < 0 || buffer.is_null() {
        return if status < 0 {
            status
        } else {
            crate::ERR_INVALID_PARAM
        };
    }

    let mode = unsafe { *(buffer as *const FluxIddMonitorMode) };
    crate::plug_in_monitor(mode.width, mode.height, mode.refresh_hz)
}
