//! Windows virtual display control: talks to the FluxIdd indirect display
//! driver (drivers/flux-idd) to plug a GPU-backed virtual monitor in/out so a
//! headless host has a display to capture.
//!
//! The driver device is found through its interface GUID
//! `{5b1a4c37-6f5d-4a41-9c1d-8f2e4b6a7c01}`; the monitor is plugged in with
//! `IOCTL_FLUXIDD_PLUG_IN` (mode payload) and removed with
//! `IOCTL_FLUXIDD_PLUG_OUT`. The monitor is automatically plugged out when
//! the [`VirtualDisplay`] handle is dropped.

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
const IOCTL_FLUXIDD_PLUG_IN: u32 = ctl_code_write(0x900);
const IOCTL_FLUXIDD_PLUG_OUT: u32 = ctl_code_write(0x901);

#[repr(C)]
struct FluxIddMonitorMode {
    width: u32,
    height: u32,
    refresh_hz: u32,
}

/// An open handle to the FluxIdd driver with the virtual monitor plugged in.
/// Dropping it plugs the monitor back out.
pub struct VirtualDisplay {
    device: HANDLE,
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
        let mut returned = 0u32;
        let result = unsafe {
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
        };
        if let Err(e) = result {
            unsafe {
                let _ = CloseHandle(device);
            }
            return Err(format!("IOCTL_FLUXIDD_PLUG_IN failed: {e}"));
        }

        tracing::info!(
            "Virtual display plugged in: {}x{}@{}Hz",
            width,
            height,
            refresh_hz
        );
        Ok(Self { device })
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        unsafe {
            let mut returned = 0u32;
            let _ = DeviceIoControl(
                self.device,
                IOCTL_FLUXIDD_PLUG_OUT,
                None,
                0,
                None,
                0,
                Some(&mut returned),
                None,
            );
            let _ = CloseHandle(self.device);
        }
        tracing::info!("Virtual display plugged out");
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
