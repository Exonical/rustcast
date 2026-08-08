//! Flux virtual display: an Indirect Display Driver (IddCx / UMDF 2) in Rust.
//!
//! Enumerates an EDID-less virtual monitor whose preferred mode is set at
//! plug-in time by flux-server through `IOCTL_FLUXIDD_PLUG_IN`, letting a
//! headless machine expose a GPU-backed display at any resolution for DXGI
//! Desktop Duplication capture.
//!
//! Architecture mirrors Microsoft's IddCx sample driver:
//! `DriverEntry` → `evt_device_add` (register IddCx callbacks) → D0 entry
//! initializes the virtual adapter; the monitor arrives/departs on IOCTL
//! request; a swap-chain processor thread acquires and releases the frames
//! the OS renders for the monitor.

mod bindings;
mod ioctl;
mod swapchain;

use core::ffi::c_void;
use core::mem::size_of;
use core::ptr;
use std::sync::Mutex;

use wdk_sys::{
    call_unsafe_wdf_function_binding, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, ULONG,
    WDFDEVICE, WDFDEVICE_INIT, WDFDRIVER, WDFOBJECT, WDFREQUEST, WDF_DRIVER_CONFIG, WDF_NO_HANDLE,
    WDF_NO_OBJECT_ATTRIBUTES, WDF_OBJECT_ATTRIBUTES, WDF_PNPPOWER_EVENT_CALLBACKS,
    WDF_POWER_DEVICE_STATE,
};

use bindings as idd;

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC0000002u32 as NTSTATUS;
const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000000Du32 as NTSTATUS;
const STATUS_DEVICE_BUSY: NTSTATUS = 0x80000011u32 as NTSTATUS;
const STATUS_DEVICE_NOT_READY: NTSTATUS = 0xC00000A3u32 as NTSTATUS;
const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = 0xC0000010u32 as NTSTATUS;

/// Device interface used by flux-server to find and control the driver:
/// {5b1a4c37-6f5d-4a41-9c1d-8f2e4b6a7c01}
pub const GUID_DEVINTERFACE_FLUXIDD: wdk_sys::GUID = wdk_sys::GUID {
    Data1: 0x5b1a4c37,
    Data2: 0x6f5d,
    Data3: 0x4a41,
    Data4: [0x9c, 0x1d, 0x8f, 0x2e, 0x4b, 0x6a, 0x7c, 0x01],
};

/// Modes offered in addition to the preferred mode requested at plug-in.
const DEFAULT_MODES: &[(u32, u32, u32)] = &[
    (3840, 2160, 60),
    (2560, 1600, 60),
    (2560, 1440, 60),
    (1920, 1200, 60),
    (1920, 1080, 60),
    (1680, 1050, 60),
    (1600, 900, 60),
    (1366, 768, 60),
    (1280, 720, 60),
    (1024, 768, 60),
];

/// Single-adapter, single-monitor driver state. IddCx context plumbing in C
/// uses per-object WDF contexts; with exactly one virtual adapter and monitor
/// a process-global keeps the Rust side simple and safe.
struct DriverState {
    adapter: idd::IDDCX_ADAPTER,
    monitor: idd::IDDCX_MONITOR,
    monitor_plugged_in: bool,
    preferred: (u32, u32, u32),
    processor: Option<swapchain::SwapChainProcessor>,
}

// IDDCX handles are only touched from IddCx callbacks and the IOCTL queue,
// which IddCx serializes; the Mutex enforces exclusive access on top.
unsafe impl Send for DriverState {}

static STATE: Mutex<DriverState> = Mutex::new(DriverState {
    adapter: ptr::null_mut(),
    monitor: ptr::null_mut(),
    monitor_plugged_in: false,
    preferred: (1920, 1080, 60),
    processor: None,
});

// ─────────────────────────────────────────────────────────────────────────────
// Driver entry and device add
// ─────────────────────────────────────────────────────────────────────────────

/// # Safety
/// Called by the UMDF framework with valid driver object and registry path.
#[unsafe(export_name = "DriverEntry")]
pub unsafe extern "system" fn driver_entry(
    driver: PDRIVER_OBJECT,
    registry_path: PCUNICODE_STRING,
) -> NTSTATUS {
    let mut config = WDF_DRIVER_CONFIG {
        Size: size_of::<WDF_DRIVER_CONFIG>() as ULONG,
        EvtDriverDeviceAdd: Some(evt_device_add),
        ..Default::default()
    };

    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDriverCreate,
            driver as *mut _,
            registry_path,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut config,
            WDF_NO_HANDLE.cast(),
        )
    }
}

extern "C" fn evt_device_add(_driver: WDFDRIVER, device_init: *mut WDFDEVICE_INIT) -> NTSTATUS {
    let mut device_init = device_init;

    let mut pnp_callbacks = WDF_PNPPOWER_EVENT_CALLBACKS {
        Size: size_of::<WDF_PNPPOWER_EVENT_CALLBACKS>() as ULONG,
        EvtDeviceD0Entry: Some(evt_device_d0_entry),
        ..Default::default()
    };
    unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceInitSetPnpPowerEventCallbacks,
            device_init,
            &mut pnp_callbacks,
        );
    }

    let mut idd_config = idd::IDD_CX_CLIENT_CONFIG {
        Size: size_of::<idd::IDD_CX_CLIENT_CONFIG>() as u32,
        EvtIddCxDeviceIoControl: Some(ioctl::evt_io_device_control),
        EvtIddCxAdapterInitFinished: Some(evt_adapter_init_finished),
        EvtIddCxParseMonitorDescription: Some(evt_parse_monitor_description),
        EvtIddCxMonitorGetDefaultDescriptionModes: Some(evt_monitor_get_default_modes),
        EvtIddCxMonitorQueryTargetModes: Some(evt_monitor_query_target_modes),
        EvtIddCxAdapterCommitModes: Some(evt_adapter_commit_modes),
        EvtIddCxMonitorAssignSwapChain: Some(evt_monitor_assign_swapchain),
        EvtIddCxMonitorUnassignSwapChain: Some(evt_monitor_unassign_swapchain),
        ..Default::default()
    };

    let status = unsafe { idd::IddCxDeviceInitConfig(device_init as *mut _, &mut idd_config) };
    if status < 0 {
        return status;
    }

    let mut device: WDFDEVICE = ptr::null_mut();
    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreate,
            &mut device_init,
            WDF_NO_OBJECT_ATTRIBUTES,
            &mut device,
        )
    };
    if status < 0 {
        return status;
    }

    let status = unsafe {
        call_unsafe_wdf_function_binding!(
            WdfDeviceCreateDeviceInterface,
            device,
            &GUID_DEVINTERFACE_FLUXIDD,
            ptr::null_mut(),
        )
    };
    if status < 0 {
        return status;
    }

    unsafe { idd::IddCxDeviceInitialize(device as idd::WDFDEVICE) }
}

extern "C" fn evt_device_d0_entry(
    device: WDFDEVICE,
    _previous_state: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    init_adapter(device)
}

// ─────────────────────────────────────────────────────────────────────────────
// Adapter and monitor lifecycle
// ─────────────────────────────────────────────────────────────────────────────

fn init_adapter(device: WDFDEVICE) -> NTSTATUS {
    let mut firmware_version = idd::IDDCX_ENDPOINT_VERSION {
        Size: size_of::<idd::IDDCX_ENDPOINT_VERSION>() as u32,
        MajorVer: 1,
        ..Default::default()
    };

    let friendly_name: Vec<u16> = "Flux Virtual Display Adapter\0".encode_utf16().collect();
    let manufacturer: Vec<u16> = "Rustcast\0".encode_utf16().collect();
    let model: Vec<u16> = "FluxIdd\0".encode_utf16().collect();

    let mut caps = idd::IDDCX_ADAPTER_CAPS {
        Size: size_of::<idd::IDDCX_ADAPTER_CAPS>() as u32,
        MaxMonitorsSupported: 1,
        ..Default::default()
    };
    caps.EndPointDiagnostics.Size = size_of::<idd::IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32;
    caps.EndPointDiagnostics.GammaSupport = idd::IDDCX_FEATURE_IMPLEMENTATION_IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType = idd::IDDCX_TRANSMISSION_TYPE_IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    caps.EndPointDiagnostics.pEndPointFriendlyName = friendly_name.as_ptr();
    caps.EndPointDiagnostics.pEndPointManufacturerName = manufacturer.as_ptr();
    caps.EndPointDiagnostics.pEndPointModelName = model.as_ptr();
    caps.EndPointDiagnostics.pFirmwareVersion = &mut firmware_version;
    caps.EndPointDiagnostics.pHardwareVersion = &mut firmware_version;

    let mut init = idd::IDARG_IN_ADAPTER_INIT {
        WdfDevice: device as idd::WDFDEVICE,
        pCaps: &mut caps,
        ObjectAttributes: ptr::null_mut(),
    };

    let mut out = idd::IDARG_OUT_ADAPTER_INIT::default();
    let status = unsafe { idd::IddCxAdapterInitAsync(&mut init, &mut out) };
    if status >= 0 {
        STATE.lock().unwrap().adapter = out.AdapterObject;
    }
    status
}

pub(crate) fn plug_in_monitor(width: u32, height: u32, refresh_hz: u32) -> NTSTATUS {
    let mut state = STATE.lock().unwrap();
    if state.adapter.is_null() {
        return STATUS_DEVICE_NOT_READY;
    }
    if state.monitor_plugged_in {
        return STATUS_DEVICE_BUSY;
    }

    state.preferred = (
        if width == 0 { 1920 } else { width },
        if height == 0 { 1080 } else { height },
        if refresh_hz == 0 { 60 } else { refresh_hz },
    );

    let mut monitor_info = idd::IDDCX_MONITOR_INFO {
        Size: size_of::<idd::IDDCX_MONITOR_INFO>() as u32,
        MonitorType:
            idd::DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
        ConnectorIndex: 0,
        ..Default::default()
    };
    monitor_info.MonitorDescription.Size = size_of::<idd::IDDCX_MONITOR_DESCRIPTION>() as u32;
    monitor_info.MonitorDescription.Type =
        idd::IDDCX_MONITOR_DESCRIPTION_TYPE_IDDCX_MONITOR_DESCRIPTION_TYPE_EDID;
    // EDID-less monitor: the OS calls EvtIddCxMonitorGetDefaultDescriptionModes.
    monitor_info.MonitorDescription.DataSize = 0;
    monitor_info.MonitorDescription.pData = ptr::null_mut();

    // Stable container ID so Windows remembers per-monitor settings:
    // {9a8c2d4e-1b3f-4c5a-8d6e-7f0a1b2c3d4e}
    monitor_info.MonitorContainerId = idd::GUID {
        Data1: 0x9a8c2d4e,
        Data2: 0x1b3f,
        Data3: 0x4c5a,
        Data4: [0x8d, 0x6e, 0x7f, 0x0a, 0x1b, 0x2c, 0x3d, 0x4e],
    };

    let mut create = idd::IDARG_IN_MONITORCREATE {
        ObjectAttributes: ptr::null_mut(),
        pMonitorInfo: &mut monitor_info,
    };
    let mut create_out = idd::IDARG_OUT_MONITORCREATE::default();
    let status = unsafe { idd::IddCxMonitorCreate(state.adapter, &mut create, &mut create_out) };
    if status < 0 {
        return status;
    }

    let mut arrival_out = idd::IDARG_OUT_MONITORARRIVAL::default();
    let status = unsafe { idd::IddCxMonitorArrival(create_out.MonitorObject, &mut arrival_out) };
    if status < 0 {
        return status;
    }

    state.monitor = create_out.MonitorObject;
    state.monitor_plugged_in = true;
    STATUS_SUCCESS
}

pub(crate) fn plug_out_monitor() -> NTSTATUS {
    let mut state = STATE.lock().unwrap();
    if !state.monitor_plugged_in || state.monitor.is_null() {
        return STATUS_DEVICE_NOT_READY;
    }

    let status = unsafe { idd::IddCxMonitorDeparture(state.monitor) };
    if status >= 0 {
        state.monitor = ptr::null_mut();
        state.monitor_plugged_in = false;
        state.processor = None;
    }
    status
}

// ─────────────────────────────────────────────────────────────────────────────
// Mode lists
// ─────────────────────────────────────────────────────────────────────────────

fn fill_signal_info(
    mode: &mut idd::DISPLAYCONFIG_VIDEO_SIGNAL_INFO,
    width: u32,
    height: u32,
    vsync: u32,
    monitor_mode: bool,
) {
    mode.totalSize.cx = width as i32;
    mode.totalSize.cy = height as i32;
    mode.activeSize.cx = width as i32;
    mode.activeSize.cy = height as i32;

    // vSyncFreqDivider lives in the AdditionalSignalInfo bitfield union.
    unsafe {
        let additional = &mut mode.__bindgen_anon_1.AdditionalSignalInfo;
        additional.set_vSyncFreqDivider(if monitor_mode { 0 } else { 1 });
        additional.set_videoStandard(255);
    }

    mode.vSyncFreq.Numerator = vsync;
    mode.vSyncFreq.Denominator = 1;
    mode.hSyncFreq.Numerator = vsync * height;
    mode.hSyncFreq.Denominator = 1;

    mode.scanLineOrdering =
        idd::DISPLAYCONFIG_SCANLINE_ORDERING_DISPLAYCONFIG_SCANLINE_ORDERING_PROGRESSIVE;
    mode.pixelRate = u64::from(vsync) * u64::from(width) * u64::from(height);
}

fn monitor_mode(width: u32, height: u32, vsync: u32, origin: idd::IDDCX_MONITOR_MODE_ORIGIN) -> idd::IDDCX_MONITOR_MODE {
    let mut mode = idd::IDDCX_MONITOR_MODE {
        Size: size_of::<idd::IDDCX_MONITOR_MODE>() as u32,
        Origin: origin,
        ..Default::default()
    };
    fill_signal_info(&mut mode.MonitorVideoSignalInfo, width, height, vsync, true);
    mode
}

fn target_mode(width: u32, height: u32, vsync: u32) -> idd::IDDCX_TARGET_MODE {
    let mut mode = idd::IDDCX_TARGET_MODE {
        Size: size_of::<idd::IDDCX_TARGET_MODE>() as u32,
        ..Default::default()
    };
    fill_signal_info(
        &mut mode.TargetVideoSignalInfo.targetVideoSignalInfo,
        width,
        height,
        vsync,
        false,
    );
    mode
}

/// Preferred mode first, then the defaults (skipping duplicates).
fn mode_list() -> Vec<(u32, u32, u32)> {
    let preferred = STATE.lock().unwrap().preferred;
    let mut list = vec![preferred];
    for &(w, h, hz) in DEFAULT_MODES {
        if w == preferred.0 && h == preferred.1 {
            continue;
        }
        list.push((w, h, hz));
    }
    list
}

// ─────────────────────────────────────────────────────────────────────────────
// IddCx callbacks
// ─────────────────────────────────────────────────────────────────────────────

extern "C" fn evt_adapter_init_finished(
    _adapter: idd::IDDCX_ADAPTER,
    _in_args: *const idd::IDARG_IN_ADAPTER_INIT_FINISHED,
) -> NTSTATUS {
    // Monitor is plugged in on IOCTL request, not at adapter init.
    STATUS_SUCCESS
}

extern "C" fn evt_adapter_commit_modes(
    _adapter: idd::IDDCX_ADAPTER,
    _in_args: *const idd::IDARG_IN_COMMITMODES,
) -> NTSTATUS {
    STATUS_SUCCESS
}

extern "C" fn evt_parse_monitor_description(
    _in_args: *const idd::IDARG_IN_PARSEMONITORDESCRIPTION,
    out_args: *mut idd::IDARG_OUT_PARSEMONITORDESCRIPTION,
) -> NTSTATUS {
    // EDID-less monitor: no description to parse.
    unsafe {
        (*out_args).MonitorModeBufferOutputCount = 0;
    }
    STATUS_NOT_IMPLEMENTED
}

extern "C" fn evt_monitor_get_default_modes(
    _monitor: idd::IDDCX_MONITOR,
    in_args: *const idd::IDARG_IN_GETDEFAULTDESCRIPTIONMODES,
    out_args: *mut idd::IDARG_OUT_GETDEFAULTDESCRIPTIONMODES,
) -> NTSTATUS {
    let modes = mode_list();
    unsafe {
        let in_args = &*in_args;
        let out_args = &mut *out_args;
        if in_args.DefaultMonitorModeBufferInputCount == 0 {
            out_args.DefaultMonitorModeBufferOutputCount = modes.len() as u32;
        } else {
            let count = modes.len().min(in_args.DefaultMonitorModeBufferInputCount as usize);
            for (i, &(w, h, hz)) in modes.iter().take(count).enumerate() {
                let origin = if i == 0 {
                    idd::IDDCX_MONITOR_MODE_ORIGIN_IDDCX_MONITOR_MODE_ORIGIN_MONITORDESCRIPTOR
                } else {
                    idd::IDDCX_MONITOR_MODE_ORIGIN_IDDCX_MONITOR_MODE_ORIGIN_DRIVER
                };
                *in_args.pDefaultMonitorModes.add(i) = monitor_mode(w, h, hz, origin);
            }
            out_args.DefaultMonitorModeBufferOutputCount = count as u32;
            out_args.PreferredMonitorModeIdx = 0;
        }
    }
    STATUS_SUCCESS
}

extern "C" fn evt_monitor_query_target_modes(
    _monitor: idd::IDDCX_MONITOR,
    in_args: *const idd::IDARG_IN_QUERYTARGETMODES,
    out_args: *mut idd::IDARG_OUT_QUERYTARGETMODES,
) -> NTSTATUS {
    let modes = mode_list();
    unsafe {
        let in_args = &*in_args;
        let out_args = &mut *out_args;
        if in_args.TargetModeBufferInputCount == 0 {
            out_args.TargetModeBufferOutputCount = modes.len() as u32;
        } else {
            let count = modes.len().min(in_args.TargetModeBufferInputCount as usize);
            for (i, &(w, h, hz)) in modes.iter().take(count).enumerate() {
                *in_args.pTargetModes.add(i) = target_mode(w, h, hz);
            }
            out_args.TargetModeBufferOutputCount = count as u32;
        }
    }
    STATUS_SUCCESS
}

extern "C" fn evt_monitor_assign_swapchain(
    _monitor: idd::IDDCX_MONITOR,
    in_args: *const idd::IDARG_IN_SETSWAPCHAIN,
) -> NTSTATUS {
    let in_args = unsafe { &*in_args };
    let mut state = STATE.lock().unwrap();
    state.processor = None; // stop any previous processor first

    match swapchain::SwapChainProcessor::start(
        in_args.hSwapChain,
        in_args.RenderAdapterLuid,
        in_args.hNextSurfaceAvailable as *mut c_void,
    ) {
        Ok(processor) => {
            state.processor = Some(processor);
            STATUS_SUCCESS
        }
        Err(_) => {
            // Give the swap-chain back to the OS to retry (possibly with a
            // different render adapter).
            unsafe {
                call_unsafe_wdf_function_binding!(
                    WdfObjectDelete,
                    in_args.hSwapChain as WDFOBJECT,
                );
            }
            STATUS_SUCCESS
        }
    }
}

extern "C" fn evt_monitor_unassign_swapchain(_monitor: idd::IDDCX_MONITOR) -> NTSTATUS {
    STATE.lock().unwrap().processor = None;
    STATUS_SUCCESS
}

// Re-exported for ioctl.rs
pub(crate) use {STATUS_INVALID_DEVICE_REQUEST as ERR_INVALID_REQUEST, STATUS_INVALID_PARAMETER as ERR_INVALID_PARAM};
pub(crate) type Wdfrequest = WDFREQUEST;
