//! IOCTL interface used by flux-server to plug the virtual monitor in/out.
//!
//! Codes must match `flux-capture`'s Windows IDD controller:
//! - `IOCTL_FLUXIDD_PLUG_IN`  = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x900, METHOD_BUFFERED, FILE_WRITE_DATA)
//!   with input `FluxIddMonitorMode { width: u32, height: u32, refresh_hz: u32 }`
//! - `IOCTL_FLUXIDD_PLUG_OUT` = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x901, METHOD_BUFFERED, FILE_WRITE_DATA)
//! - `IOCTL_FLUXIDD_GET_STATUS` = CTL_CODE(FILE_DEVICE_UNKNOWN, 0x902, METHOD_BUFFERED, FILE_READ_DATA)

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

const fn ctl_code_read(function: u32) -> u32 {
    (0x22 << 16) | (1 << 14) | (function << 2)
}

pub const IOCTL_FLUXIDD_PLUG_IN: u32 = ctl_code_write(0x900);
pub const IOCTL_FLUXIDD_PLUG_OUT: u32 = ctl_code_write(0x901);
pub const IOCTL_FLUXIDD_GET_STATUS: u32 = ctl_code_read(0x902);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FluxIddMonitorMode {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FluxIddStatus {
    pub d0_entry_ran: u32,
    pub adapter_init_async_status: NTSTATUS,
    pub adapter_init_status: NTSTATUS,
    pub adapter_ready: u32,
    pub monitor_plugged_in: u32,
    pub monitor_operation_in_progress: u32,
}

pub unsafe extern "C" fn evt_io_device_control(
    _device: idd::WDFDEVICE,
    request: idd::WDFREQUEST,
    output_buffer_length: usize,
    input_buffer_length: usize,
    io_control_code: ULONG,
) {
    let request = request as WDFREQUEST;
    let (status, information) = match io_control_code {
        IOCTL_FLUXIDD_PLUG_IN => (handle_plug_in(request, input_buffer_length), 0),
        IOCTL_FLUXIDD_PLUG_OUT => (crate::plug_out_monitor(), 0),
        IOCTL_FLUXIDD_GET_STATUS => handle_get_status(request, output_buffer_length),
        _ => (crate::ERR_INVALID_REQUEST, 0),
    };

    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestCompleteWithInformation,
            request,
            status,
            information as u64,
        );
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

fn handle_get_status(request: WDFREQUEST, output_buffer_length: usize) -> (NTSTATUS, usize) {
    if output_buffer_length < size_of::<FluxIddStatus>() {
        return (crate::ERR_BUFFER_TOO_SMALL, 0);
    }

    let mut buffer: *mut c_void = ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfRequestRetrieveOutputBuffer,
            request,
            size_of::<FluxIddStatus>(),
            &mut buffer,
            ptr::null_mut(),
        )
    };
    if status < 0 || buffer.is_null() {
        return (
            if status < 0 {
                status
            } else {
                crate::ERR_INVALID_PARAM
            },
            0,
        );
    }

    unsafe {
        *(buffer as *mut FluxIddStatus) = crate::query_status();
    }
    (crate::STATUS_SUCCESS, size_of::<FluxIddStatus>())
}
