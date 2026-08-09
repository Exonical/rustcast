//! Windows virtual display control: talks to the FluxIdd indirect display
//! driver (drivers/flux-idd) to plug a GPU-backed virtual monitor in/out so a
//! headless host has a display to capture.
//!
//! The driver device is found through its interface GUID
//! `{5b1a4c37-6f5d-4a41-9c1d-8f2e4b6a7c01}`; the monitor is plugged in with
//! `IOCTL_FLUXIDD_PLUG_IN` (mode payload) and removed with
//! `IOCTL_FLUXIDD_PLUG_OUT`. `IOCTL_FLUXIDD_GET_STATUS` returns adapter
//! initialization state for diagnostics. A monitor acquired by the
//! [`VirtualDisplay`] handle is automatically plugged out when it is dropped.

#![cfg(target_os = "windows")]

use windows::core::GUID;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;

const GUID_DEVINTERFACE_FLUXIDD: GUID = GUID::from_u128(0x5b1a4c37_6f5d_4a41_9c1d_8f2e4b6a7c01);

// CTL_CODE(FILE_DEVICE_UNKNOWN=0x22, function, METHOD_BUFFERED=0, FILE_WRITE_DATA=2)
const fn ctl_code_write(function: u32) -> u32 {
    (0x22 << 16) | (2 << 14) | (function << 2)
}
const fn ctl_code_read(function: u32) -> u32 {
    (0x22 << 16) | (1 << 14) | (function << 2)
}
const IOCTL_FLUXIDD_PLUG_IN: u32 = ctl_code_write(0x900);
const IOCTL_FLUXIDD_PLUG_OUT: u32 = ctl_code_write(0x901);
const IOCTL_FLUXIDD_GET_STATUS: u32 = ctl_code_read(0x902);

#[repr(C)]
struct FluxIddMonitorMode {
    width: u32,
    height: u32,
    refresh_hz: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct FluxIddStatus {
    d0_entry_ran: u32,
    adapter_init_async_status: i32,
    adapter_init_status: i32,
    adapter_ready: u32,
    monitor_plugged_in: u32,
    monitor_operation_in_progress: u32,
    adapter_init_finished_entry_count: u32,
    device_init_config_status: i32,
    device_initialize_status: i32,
    adapter_handle_nonnull: u32,
    adapter_config_size: u32,
    adapter_caps_size: u32,
    iddcx_version_major: u32,
    iddcx_version_minor: u32,
    iddcx_minimum_version_required: u32,
}

/// An open handle to the FluxIdd driver with the virtual monitor plugged in.
/// Dropping it plugs out a monitor acquired by this handle.
pub struct VirtualDisplay {
    device: HANDLE,
    owns_monitor: bool,
}

// HANDLE is a process-wide kernel handle, safe to move across threads.
unsafe impl Send for VirtualDisplay {}

impl VirtualDisplay {
    /// Find the FluxIdd device, open it, and plug in the virtual monitor at
    /// the requested mode.
    pub fn plug_in(width: u32, height: u32, refresh_hz: u32) -> Result<Self, String> {
        let path = find_device_path().map_err(|e| {
            format!("FluxIdd driver not found — is the virtual display driver installed? ({e})")
        })?;

        let device = unsafe {
            CreateFileW(
                &windows::core::HSTRING::from(path.as_str()),
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| format!("open FluxIdd device: {e}"))?;

        let mode = FluxIddMonitorMode {
            width,
            height,
            refresh_hz,
        };
        if let Err(e) = issue_plug_in(device, mode) {
            match query_status(device) {
                Ok(status) => {
                    log_status("after plug-in failure", status);
                    if status.monitor_plugged_in != 0
                        && status.monitor_operation_in_progress == 0
                    {
                        tracing::warn!(
                            "FluxIdd monitor is already plugged in; adopting it before \
                             reconfiguring to {}x{}@{}Hz",
                            width,
                            height,
                            refresh_hz
                        );
                        return Ok(Self {
                            device,
                            owns_monitor: false,
                        });
                    }
                }
                Err(status_error) => {
                    tracing::error!("FluxIdd status query after plug-in failure failed: {status_error}")
                }
            }
            unsafe {
                let _ = CloseHandle(device);
            }
            return Err(format!(
                "IOCTL_FLUXIDD_PLUG_IN failed: {e} (device={path}, mode={width}x{height}@{refresh_hz}Hz, code=0x{IOCTL_FLUXIDD_PLUG_IN:08x})"
            ));
        }

        tracing::info!(
            "Virtual display plugged in: {}x{}@{}Hz",
            width,
            height,
            refresh_hz
        );
        Ok(Self {
            device,
            owns_monitor: true,
        })
    }

    /// Whether the monitor was already plugged in when this handle acquired it.
    pub fn was_adopted(&self) -> bool {
        !self.owns_monitor
    }

    /// Remove the current monitor while retaining the driver handle.
    ///
    /// This is used when an existing monitor was found: the status IOCTL does
    /// not expose its current mode, so the server re-plugs it at the requested
    /// mode before relying on the DXGI output identity.
    pub fn unplug(&mut self) -> Result<(), String> {
        issue_plug_out(self.device)?;
        self.owns_monitor = false;
        Ok(())
    }

    /// Plug a monitor into an already-open FluxIdd handle.
    pub fn plug_in_mode(
        &mut self,
        width: u32,
        height: u32,
        refresh_hz: u32,
    ) -> Result<(), String> {
        let mode = FluxIddMonitorMode {
            width,
            height,
            refresh_hz,
        };
        issue_plug_in(self.device, mode)?;
        self.owns_monitor = true;
        Ok(())
    }
}

fn issue_plug_in(device: HANDLE, mode: FluxIddMonitorMode) -> Result<(), String> {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            device,
            IOCTL_FLUXIDD_PLUG_IN,
            Some(&mode as *const _ as *const _),
            std::mem::size_of::<FluxIddMonitorMode>() as u32,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| format!("{e}"))
}

fn issue_plug_out(device: HANDLE) -> Result<(), String> {
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            device,
            IOCTL_FLUXIDD_PLUG_OUT,
            None,
            0,
            None,
            0,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| format!("IOCTL_FLUXIDD_PLUG_OUT failed: {e}"))
}

fn query_status(device: HANDLE) -> Result<FluxIddStatus, String> {
    let mut status = FluxIddStatus {
        d0_entry_ran: 0,
        adapter_init_async_status: 0,
        adapter_init_status: 0,
        adapter_ready: 0,
        monitor_plugged_in: 0,
        monitor_operation_in_progress: 0,
        adapter_init_finished_entry_count: 0,
        device_init_config_status: 0,
        device_initialize_status: 0,
        adapter_handle_nonnull: 0,
        adapter_config_size: 0,
        adapter_caps_size: 0,
        iddcx_version_major: 0,
        iddcx_version_minor: 0,
        iddcx_minimum_version_required: 0,
    };
    let mut returned = 0u32;
    unsafe {
        DeviceIoControl(
            device,
            IOCTL_FLUXIDD_GET_STATUS,
            None,
            0,
            Some(&mut status as *mut _ as *mut _),
            std::mem::size_of::<FluxIddStatus>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|e| format!("IOCTL_FLUXIDD_GET_STATUS failed: {e}"))?;
    if returned as usize != std::mem::size_of::<FluxIddStatus>() {
        return Err(format!(
            "IOCTL_FLUXIDD_GET_STATUS returned {returned} bytes, expected {}",
            std::mem::size_of::<FluxIddStatus>()
        ));
    }
    Ok(status)
}

fn log_status(context: &str, status: FluxIddStatus) {
    tracing::error!(
        "FluxIdd status {context}: d0_entry_ran={}, \
         adapter_init_async_status=0x{:08x}, adapter_init_status=0x{:08x}, \
         adapter_ready={}, monitor_plugged_in={}, \
         monitor_operation_in_progress={}, \
         adapter_init_finished_entry_count={}, \
         device_init_config_status=0x{:08x}, \
         device_initialize_status=0x{:08x}, \
         adapter_handle_nonnull={}, adapter_config_size={}, \
         adapter_caps_size={}, iddcx_version={}.{} min={}",
        status.d0_entry_ran,
        status.adapter_init_async_status as u32,
        status.adapter_init_status as u32,
        status.adapter_ready,
        status.monitor_plugged_in,
        status.monitor_operation_in_progress,
        status.adapter_init_finished_entry_count,
        status.device_init_config_status as u32,
        status.device_initialize_status as u32,
        status.adapter_handle_nonnull,
        status.adapter_config_size,
        status.iddcx_version_major,
        status.iddcx_version_minor,
        status.iddcx_minimum_version_required,
    );
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        if self.owns_monitor {
            let _ = issue_plug_out(self.device);
            tracing::info!("Virtual display plugged out");
        }
        unsafe {
            let _ = CloseHandle(self.device);
        }
    }
}

/// Resolve the device path for the first present FluxIdd interface.
fn find_device_path() -> Result<String, String> {
    unsafe {
        let mut len = 0u32;
        let cr = CM_Get_Device_Interface_List_SizeW(
            &mut len,
            &GUID_DEVINTERFACE_FLUXIDD,
            None,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        );
        if cr != CR_SUCCESS || len <= 1 {
            return Err(format!("no FluxIdd device interface present (CONFIGRET {cr:?})"));
        }

        let mut buffer = vec![0u16; len as usize];
        let cr = CM_Get_Device_Interface_ListW(
            &GUID_DEVINTERFACE_FLUXIDD,
            None,
            &mut buffer,
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        );
        if cr != CR_SUCCESS {
            return Err(format!("CM_Get_Device_Interface_ListW failed (CONFIGRET {cr:?})"));
        }

        // Buffer is a REG_MULTI_SZ; take the first entry.
        let first_end = buffer.iter().position(|&c| c == 0).unwrap_or(0);
        if first_end == 0 {
            return Err("no FluxIdd device interface present".to_string());
        }
        Ok(String::from_utf16_lossy(&buffer[..first_end]))
    }
}
