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
    call_unsafe_wdf_function_binding, NTSTATUS, PCUNICODE_STRING, PDRIVER_OBJECT, ULONG, WDFDEVICE,
    WDFDEVICE_INIT, WDFDRIVER, WDF_DRIVER_CONFIG, WDF_NO_HANDLE, WDF_NO_OBJECT_ATTRIBUTES,
    WDF_PNPPOWER_EVENT_CALLBACKS, WDF_POWER_DEVICE_STATE,
};

use bindings as idd;

const STATUS_SUCCESS: NTSTATUS = 0;
const STATUS_NOT_IMPLEMENTED: NTSTATUS = 0xC0000002u32 as NTSTATUS;
const STATUS_INVALID_PARAMETER: NTSTATUS = 0xC000000Du32 as NTSTATUS;
const STATUS_BUFFER_TOO_SMALL: NTSTATUS = 0xC0000023u32 as NTSTATUS;
const STATUS_DEVICE_BUSY: NTSTATUS = 0x80000011u32 as NTSTATUS;
const STATUS_DEVICE_NOT_READY: NTSTATUS = 0xC00000A3u32 as NTSTATUS;
const STATUS_INVALID_DEVICE_REQUEST: NTSTATUS = 0xC0000010u32 as NTSTATUS;
const STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN: NTSTATUS = 0xC01E0003u32 as NTSTATUS;
const STATUS_INIT_NOT_ATTEMPTED: NTSTATUS = 0xFFFFFFFFu32 as NTSTATUS;

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
    adapter_ready: bool,
    d0_entry_ran: bool,
    adapter_init_async_status: NTSTATUS,
    adapter_init_status: NTSTATUS,
    adapter_init_finished_entry_count: u32,
    device_init_config_status: NTSTATUS,
    device_initialize_status: NTSTATUS,
    adapter_handle_nonnull: u32,
    adapter_config_size: u32,
    adapter_caps_size: u32,
    monitor: idd::IDDCX_MONITOR,
    monitor_plugged_in: bool,
    monitor_operation_in_progress: bool,
    preferred: (u32, u32, u32),
    processor: Option<swapchain::SwapChainProcessor>,
}

// IDDCX handles are only touched from IddCx callbacks and the IOCTL queue,
// which IddCx serializes; the Mutex enforces exclusive access on top.
unsafe impl Send for DriverState {}

static STATE: Mutex<DriverState> = Mutex::new(DriverState {
    adapter: ptr::null_mut(),
    adapter_ready: false,
    d0_entry_ran: false,
    adapter_init_async_status: STATUS_INIT_NOT_ATTEMPTED,
    adapter_init_status: STATUS_INIT_NOT_ATTEMPTED,
    adapter_init_finished_entry_count: 0,
    device_init_config_status: STATUS_INIT_NOT_ATTEMPTED,
    device_initialize_status: STATUS_INIT_NOT_ATTEMPTED,
    adapter_handle_nonnull: 0,
    adapter_config_size: 0,
    adapter_caps_size: 0,
    monitor: ptr::null_mut(),
    monitor_plugged_in: false,
    monitor_operation_in_progress: false,
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

    // Match the Microsoft sample: register callbacks before IddCx config so IddCx can chain them.
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
    STATE.lock().unwrap().device_init_config_status = status;
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

    let status = unsafe { idd::IddCxDeviceInitialize(device as idd::WDFDEVICE) };
    STATE.lock().unwrap().device_initialize_status = status;
    status
}

extern "C" fn evt_device_d0_entry(
    device: WDFDEVICE,
    _previous_state: WDF_POWER_DEVICE_STATE,
) -> NTSTATUS {
    STATE.lock().unwrap().d0_entry_ran = true;
    // Keep device start successful so the status IOCTL remains available when
    // adapter initialization fails; init_adapter records its status above.
    let _ = init_adapter(device);
    STATUS_SUCCESS
}

// ─────────────────────────────────────────────────────────────────────────────
// Adapter and monitor lifecycle
// ─────────────────────────────────────────────────────────────────────────────

fn init_adapter(device: WDFDEVICE) -> NTSTATUS {
    // Leaked: adapter init is asynchronous, so IddCx may read the diagnostic
    // strings/version after this function returns. One-time allocation.
    let firmware_version: &'static mut idd::IDDCX_ENDPOINT_VERSION =
        Box::leak(Box::new(idd::IDDCX_ENDPOINT_VERSION {
            Size: size_of::<idd::IDDCX_ENDPOINT_VERSION>() as u32,
            MajorVer: 1,
            ..Default::default()
        }));

    let friendly_name: &'static [u16] =
        Vec::leak("Flux Virtual Display Adapter\0".encode_utf16().collect());
    let manufacturer: &'static [u16] = Vec::leak("Rustcast\0".encode_utf16().collect());
    let model: &'static [u16] = Vec::leak("FluxIdd\0".encode_utf16().collect());

    let mut caps = idd::IDDCX_ADAPTER_CAPS {
        Size: size_of::<idd::IDDCX_ADAPTER_CAPS>() as u32,
        MaxMonitorsSupported: 1,
        ..Default::default()
    };
    caps.EndPointDiagnostics.Size = size_of::<idd::IDDCX_ENDPOINT_DIAGNOSTIC_INFO>() as u32;
    caps.EndPointDiagnostics.GammaSupport =
        idd::IDDCX_FEATURE_IMPLEMENTATION_IDDCX_FEATURE_IMPLEMENTATION_NONE;
    caps.EndPointDiagnostics.TransmissionType =
        idd::IDDCX_TRANSMISSION_TYPE_IDDCX_TRANSMISSION_TYPE_WIRED_OTHER;
    caps.EndPointDiagnostics.pEndPointFriendlyName = friendly_name.as_ptr();
    caps.EndPointDiagnostics.pEndPointManufacturerName = manufacturer.as_ptr();
    caps.EndPointDiagnostics.pEndPointModelName = model.as_ptr();
    caps.EndPointDiagnostics.pFirmwareVersion = firmware_version;
    caps.EndPointDiagnostics.pHardwareVersion = firmware_version;
    {
        let mut state = STATE.lock().unwrap();
        state.adapter_config_size = size_of::<idd::IDD_CX_CLIENT_CONFIG>() as u32;
        state.adapter_caps_size = caps.Size;
    }

    // Mirrors WDF_OBJECT_ATTRIBUTES_INIT: a zeroed struct has invalid
    // ExecutionLevel/SynchronizationScope and IddCx rejects it with
    // STATUS_WDF_OBJECT_ATTRIBUTES_INVALID.
    let mut object_attributes = idd::_WDF_OBJECT_ATTRIBUTES {
        Size: size_of::<idd::_WDF_OBJECT_ATTRIBUTES>() as ULONG,
        ExecutionLevel: idd::_WDF_EXECUTION_LEVEL_WdfExecutionLevelInheritFromParent,
        SynchronizationScope:
            idd::_WDF_SYNCHRONIZATION_SCOPE_WdfSynchronizationScopeInheritFromParent,
        ..Default::default()
    };
    let mut init = idd::IDARG_IN_ADAPTER_INIT {
        WdfDevice: device as idd::WDFDEVICE,
        pCaps: &mut caps,
        ObjectAttributes: &mut object_attributes,
    };

    let mut out = idd::IDARG_OUT_ADAPTER_INIT::default();
    let status = unsafe { idd::IddCxAdapterInitAsync(&mut init, &mut out) };
    let mut state = STATE.lock().unwrap();
    state.adapter_init_async_status = status;
    state.adapter_handle_nonnull = (!out.AdapterObject.is_null()) as u32;
    if status >= 0 {
        state.adapter = out.AdapterObject;
    }
    status
}

fn prefer_discrete_render_adapter(adapter: idd::IDDCX_ADAPTER) {
    let available = unsafe { idd::idd_cx_adapter_set_render_adapter_available() };
    if !available {
        wdk::println!(
            "[FluxIdd] Render adapter preference skipped: IddCxAdapterSetRenderAdapter unavailable"
        );
        return;
    }

    let candidates = match swapchain::choose_render_adapter() {
        Ok(candidates) => candidates,
        Err(error) => {
            wdk::println!(
                "[FluxIdd] Render adapter preference skipped: DXGI enumeration failed: {}",
                error
            );
            return;
        }
    };
    if candidates.len() <= 1 {
        wdk::println!(
            "[FluxIdd] Render adapter preference skipped: found {} usable hardware adapter(s); need at least 2",
            candidates.len()
        );
        return;
    }

    let preferred = &candidates[0];
    swapchain::log_render_adapter(preferred);
    let preferred_luid = idd::LUID {
        LowPart: preferred.luid.LowPart,
        HighPart: preferred.luid.HighPart,
    };
    let args = idd::IDARG_IN_ADAPTERSETRENDERADAPTER {
        PreferredRenderAdapter: preferred_luid,
    };
    wdk::println!(
        "[FluxIdd] Render adapter preference attempted: {} (LUID=0x{:016X})",
        preferred.name,
        preferred.luid_value()
    );
    // IddCxAdapterSetRenderAdapter has a VOID ABI, so the API provides no
    // status to inspect. Returning from the call is the only driver-visible
    // indication that the request was accepted; Windows reports the actual
    // choice later through RenderAdapterLuid.
    unsafe { idd::IddCxAdapterSetRenderAdapter(adapter, &args) };
    wdk::println!(
        "[FluxIdd] Render adapter preference call returned: requested {} (LUID=0x{:016X}); awaiting Windows assignment",
        preferred.name,
        preferred.luid_value()
    );
}

pub(crate) fn plug_in_monitor(width: u32, height: u32, refresh_hz: u32) -> NTSTATUS {
    let (adapter, old_preferred) = {
        let mut state = STATE.lock().unwrap();
        if !state.adapter_ready || state.adapter.is_null() {
            if state.adapter_init_status != STATUS_INIT_NOT_ATTEMPTED
                && state.adapter_init_status < 0
            {
                return state.adapter_init_status;
            }
            if state.adapter_init_async_status != STATUS_INIT_NOT_ATTEMPTED
                && state.adapter_init_async_status < 0
            {
                return state.adapter_init_async_status;
            }
            return STATUS_DEVICE_NOT_READY;
        }
        if state.monitor_plugged_in || state.monitor_operation_in_progress {
            return STATUS_DEVICE_BUSY;
        }

        state.monitor_operation_in_progress = true;
        let old_preferred = state.preferred;
        state.preferred = (
            if width == 0 { 1920 } else { width },
            if height == 0 { 1080 } else { height },
            if refresh_hz == 0 { 60 } else { refresh_hz },
        );
        (state.adapter, old_preferred)
    };

    let mut monitor_info = idd::IDDCX_MONITOR_INFO {
        Size: size_of::<idd::IDDCX_MONITOR_INFO>() as u32,
        MonitorType:
            idd::DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY_DISPLAYCONFIG_OUTPUT_TECHNOLOGY_HDMI,
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
    let status = unsafe { idd::IddCxMonitorCreate(adapter, &mut create, &mut create_out) };
    if status < 0 {
        let mut state = STATE.lock().unwrap();
        state.preferred = old_preferred;
        state.monitor_operation_in_progress = false;
        return status;
    }

    let mut arrival_out = idd::IDARG_OUT_MONITORARRIVAL::default();
    let status = unsafe { idd::IddCxMonitorArrival(create_out.MonitorObject, &mut arrival_out) };
    if status < 0 {
        let processor = {
            let mut state = STATE.lock().unwrap();
            state.preferred = old_preferred;
            state.monitor_operation_in_progress = false;
            state.processor.take()
        };
        drop(processor);
        return status;
    }

    let mut state = STATE.lock().unwrap();
    state.monitor = create_out.MonitorObject;
    state.monitor_plugged_in = true;
    state.monitor_operation_in_progress = false;
    STATUS_SUCCESS
}

pub(crate) fn plug_out_monitor() -> NTSTATUS {
    let (monitor, processor) = {
        let mut state = STATE.lock().unwrap();
        if !state.monitor_plugged_in || state.monitor.is_null() {
            return STATUS_DEVICE_NOT_READY;
        }
        if state.monitor_operation_in_progress {
            return STATUS_DEVICE_BUSY;
        }
        state.monitor_operation_in_progress = true;
        (state.monitor, state.processor.take())
    };

    drop(processor);
    let status = unsafe { idd::IddCxMonitorDeparture(monitor) };

    let mut state = STATE.lock().unwrap();
    state.monitor_operation_in_progress = false;
    if status >= 0 {
        state.monitor = ptr::null_mut();
        state.monitor_plugged_in = false;
    } else {
        state.monitor = monitor;
        state.monitor_plugged_in = true;
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
    mode.totalSize.cx = width;
    mode.totalSize.cy = height;
    mode.activeSize.cx = width;
    mode.activeSize.cy = height;

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

fn monitor_mode(
    width: u32,
    height: u32,
    vsync: u32,
    origin: idd::IDDCX_MONITOR_MODE_ORIGIN,
) -> idd::IDDCX_MONITOR_MODE {
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
    adapter: idd::IDDCX_ADAPTER,
    in_args: *const idd::IDARG_IN_ADAPTER_INIT_FINISHED,
) -> NTSTATUS {
    {
        let mut state = STATE.lock().unwrap();
        state.adapter_init_finished_entry_count =
            state.adapter_init_finished_entry_count.saturating_add(1);
    }
    let init_status = unsafe {
        if in_args.is_null() {
            STATUS_INVALID_PARAMETER
        } else {
            (*in_args).AdapterInitStatus
        }
    };
    let mut state = STATE.lock().unwrap();
    state.adapter_init_status = init_status;
    state.adapter_ready = init_status >= 0;
    if init_status >= 0 {
        state.adapter = adapter;
    }
    let adapter_ready = state.adapter_ready;
    drop(state);
    if adapter_ready {
        // Adapter init is complete here and monitors are only added later by
        // the plug-in IOCTL. This call performs no wait, so it does not
        // reintroduce the callback deadlock fixed in the adapter lifecycle.
        prefer_discrete_render_adapter(adapter);
    }
    // Monitor is plugged in on IOCTL request, not at adapter init.
    STATUS_SUCCESS
}

pub(crate) fn query_status() -> ioctl::FluxIddStatus {
    let state = STATE.lock().unwrap();
    ioctl::FluxIddStatus {
        d0_entry_ran: state.d0_entry_ran as u32,
        adapter_init_async_status: state.adapter_init_async_status,
        adapter_init_status: state.adapter_init_status,
        adapter_ready: state.adapter_ready as u32,
        monitor_plugged_in: state.monitor_plugged_in as u32,
        monitor_operation_in_progress: state.monitor_operation_in_progress as u32,
        adapter_init_finished_entry_count: state.adapter_init_finished_entry_count,
        device_init_config_status: state.device_init_config_status,
        device_initialize_status: state.device_initialize_status,
        adapter_handle_nonnull: state.adapter_handle_nonnull,
        adapter_config_size: state.adapter_config_size,
        adapter_caps_size: state.adapter_caps_size,
        iddcx_version_major: idd::FLUX_IDDCX_VERSION_MAJOR,
        iddcx_version_minor: idd::FLUX_IDDCX_VERSION_MINOR,
        iddcx_minimum_version_required: idd::FLUX_IDDCX_MINIMUM_VERSION_REQUIRED,
    }
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
            let count = modes
                .len()
                .min(in_args.DefaultMonitorModeBufferInputCount as usize);
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
    let render_adapter_luid = u64::from(in_args.RenderAdapterLuid.LowPart)
        | ((in_args.RenderAdapterLuid.HighPart as i64 as u64) << 32);
    wdk::println!(
        "[FluxIdd] Swap-chain assigned RenderAdapterLuid=0x{:016X}",
        render_adapter_luid
    );
    let previous_processor = {
        let mut state = STATE.lock().unwrap();
        state.processor.take()
    };
    drop(previous_processor);

    match swapchain::SwapChainProcessor::start(
        in_args.hSwapChain,
        in_args.RenderAdapterLuid,
        in_args.hNextSurfaceAvailable as *mut c_void,
    ) {
        Ok(processor) => {
            let mut state = STATE.lock().unwrap();
            state.processor = Some(processor);
            STATUS_SUCCESS
        }
        Err(_) => STATUS_GRAPHICS_INDIRECT_DISPLAY_ABANDON_SWAPCHAIN,
    }
}

extern "C" fn evt_monitor_unassign_swapchain(_monitor: idd::IDDCX_MONITOR) -> NTSTATUS {
    let processor = {
        let mut state = STATE.lock().unwrap();
        state.processor.take()
    };
    drop(processor);
    STATUS_SUCCESS
}

// Re-exported for ioctl.rs
pub(crate) use {
    STATUS_BUFFER_TOO_SMALL as ERR_BUFFER_TOO_SMALL,
    STATUS_INVALID_DEVICE_REQUEST as ERR_INVALID_REQUEST,
    STATUS_INVALID_PARAMETER as ERR_INVALID_PARAM,
};
