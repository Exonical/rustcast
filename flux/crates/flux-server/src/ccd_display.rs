//! Temporary CCD topology and mode configuration for the FluxIdd monitor.

#![cfg(target_os = "windows")]

use std::mem::size_of;
use std::thread;
use std::time::{Duration, Instant};

use flux_capture::traits::DisplayInfo;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{LUID, POINTL};
use windows::Win32::Graphics::Gdi::{
    DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_MODE_IDX_INVALID, DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE,
};

const ACTIVE_QUERY_FLAGS: QUERY_DISPLAY_CONFIG_FLAGS = QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE;
const ALL_PATHS_QUERY_FLAGS: QUERY_DISPLAY_CONFIG_FLAGS = QDC_ALL_PATHS | QDC_VIRTUAL_MODE_AWARE;
const VALIDATE_FLAGS: SET_DISPLAY_CONFIG_FLAGS =
    SDC_VALIDATE | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE;
const APPLY_FLAGS: SET_DISPLAY_CONFIG_FLAGS = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE;
const QUERY_RETRIES: usize = 4;
const CONFIGURATION_TIMEOUT: Duration = Duration::from_secs(5);

struct DisplayConfig {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

/// Apply a temporary Extend topology and the requested mode to the virtual
/// source. No persistence flag is used; the user's saved display database is
/// intentionally left unchanged.
pub fn configure_virtual_display(target: &DisplayInfo, width: u32, height: u32, refresh_hz: u32) -> Result<(), String> {
    let adapter_luid = target
        .adapter_luid
        .ok_or_else(|| format!("DXGI output {} has no adapter LUID", target.name))?;
    let deadline = Instant::now() + CONFIGURATION_TIMEOUT;
    let mut last_error = "CCD path is not ready".to_string();

    while Instant::now() < deadline {
        match configure_once(target, adapter_luid, width, height, refresh_hz) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = error;
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    Err(format!(
        "CCD configuration did not settle within {:?}: {}",
        CONFIGURATION_TIMEOUT, last_error
    ))
}

fn configure_once(
    target: &DisplayInfo,
    adapter_luid: u64,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Result<(), String> {
    let mut config = query_config(ACTIVE_QUERY_FLAGS)?;
    let virtual_index = find_source_path(&config.paths, adapter_luid, &target.name)?;

    // If the active topology cloned this source, look for an alternate path
    // connecting the same target to an unused source. This makes the supplied
    // configuration an actual Extend topology instead of changing the shared
    // physical source's mode.
    if has_duplicate_active_source(&config.paths, virtual_index) {
        let active_path = config.paths[virtual_index];
        let all_paths = query_config(ALL_PATHS_QUERY_FLAGS)?;
        let replacement = all_paths
            .paths
            .iter()
            .find(|candidate| {
                candidate.targetInfo.adapterId == active_path.targetInfo.adapterId
                    && candidate.targetInfo.id == active_path.targetInfo.id
                    && candidate.targetInfo.targetAvailable.as_bool()
                    && !config.paths.iter().any(|active| {
                        active.sourceInfo.adapterId == candidate.sourceInfo.adapterId
                            && active.sourceInfo.id == candidate.sourceInfo.id
                    })
            })
            .copied()
            .ok_or_else(|| {
                format!(
                    "CCD target {} has no independent source path for Extend",
                    active_path.targetInfo.id
                )
            })?;
        config.paths[virtual_index] = copy_path_modes(replacement, &all_paths.modes, &mut config.modes)?;
    }

    let position = non_overlapping_position(&config, virtual_index)?;
    let path = &mut config.paths[virtual_index];
    path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    let source_index = source_mode_index(path)?;
    let target_index = target_mode_index(path)?;

    let source = config
        .modes
        .get_mut(source_index)
        .ok_or_else(|| format!("CCD source mode index {} is invalid", source_index))?;
    if source.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
        return Err(format!("CCD source mode index {} is not a source mode", source_index));
    }
    unsafe {
        let mode = &mut source.Anonymous.sourceMode;
        mode.width = width;
        mode.height = height;
        mode.position = position;
    }

    let target_mode = config
        .modes
        .get_mut(target_index)
        .ok_or_else(|| format!("CCD target mode index {} is invalid", target_index))?;
    if target_mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_TARGET {
        return Err(format!("CCD target mode index {} is not a target mode", target_index));
    }
    unsafe {
        let signal = &mut target_mode.Anonymous.targetMode.targetVideoSignalInfo;
        signal.activeSize.cx = width;
        signal.activeSize.cy = height;
        signal.totalSize.cx = width;
        signal.totalSize.cy = height;
        signal.vSyncFreq.Numerator = refresh_hz;
        signal.vSyncFreq.Denominator = 1;
        signal.pixelRate = u64::from(width) * u64::from(height) * u64::from(refresh_hz);
    }
    path.targetInfo.refreshRate = DISPLAYCONFIG_RATIONAL {
        Numerator: refresh_hz,
        Denominator: 1,
    };

    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), VALIDATE_FLAGS) };
    if status != 0 {
        return Err(format!("SetDisplayConfig validation failed: {}", status));
    }
    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), APPLY_FLAGS) };
    if status != 0 {
        return Err(format!("SetDisplayConfig apply failed: {}", status));
    }

    verify_config(&target.name, adapter_luid, width, height, refresh_hz, position)
}

fn query_config(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<DisplayConfig, String> {
    for _ in 0..QUERY_RETRIES {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        let status = unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
        if status.0 != 0 {
            return Err(format!("GetDisplayConfigBufferSizes failed: {}", status.0));
        }

        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let status = unsafe {
            QueryDisplayConfig(
                flags,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if status.0 == 0 {
            paths.truncate(path_count as usize);
            modes.truncate(mode_count as usize);
            return Ok(DisplayConfig { paths, modes });
        }
        // ERROR_INSUFFICIENT_BUFFER: the topology changed between the size
        // query and QueryDisplayConfig. Start over with fresh arrays.
        if status.0 != 122 {
            return Err(format!("QueryDisplayConfig failed: {}", status.0));
        }
    }
    Err("QueryDisplayConfig topology changed while querying".into())
}

fn find_source_path(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    adapter_luid: u64,
    expected_name: &str,
) -> Result<usize, String> {
    for (index, path) in paths.iter().enumerate() {
        if luid_value(path.sourceInfo.adapterId) != adapter_luid {
            continue;
        }
        if source_name(path.sourceInfo.adapterId, path.sourceInfo.id)? == expected_name {
            return Ok(index);
        }
    }
    Err(format!(
        "CCD source {} on adapter LUID 0x{:016X} is not ready",
        expected_name, adapter_luid
    ))
}

fn source_name(adapter_id: LUID, source_id: u32) -> Result<String, String> {
    let mut packet = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
    packet.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
        adapterId: adapter_id,
        id: source_id,
    };
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut packet as *mut _ as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER) };
    if status != 0 {
        return Err(format!(
            "DisplayConfigGetDeviceInfo source {} failed: {}",
            source_id, status
        ));
    }
    let length = packet
        .viewGdiDeviceName
        .iter()
        .position(|character| *character == 0)
        .unwrap_or(packet.viewGdiDeviceName.len());
    Ok(String::from_utf16_lossy(&packet.viewGdiDeviceName[..length]))
}

fn has_duplicate_active_source(paths: &[DISPLAYCONFIG_PATH_INFO], index: usize) -> bool {
    let path = &paths[index];
    paths.iter().enumerate().any(|(other, candidate)| {
        other != index
            && candidate.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
            && candidate.sourceInfo.adapterId == path.sourceInfo.adapterId
            && candidate.sourceInfo.id == path.sourceInfo.id
    })
}

fn copy_path_modes(
    mut path: DISPLAYCONFIG_PATH_INFO,
    source_modes: &[DISPLAYCONFIG_MODE_INFO],
    modes: &mut Vec<DISPLAYCONFIG_MODE_INFO>,
) -> Result<DISPLAYCONFIG_PATH_INFO, String> {
    let source_index = source_mode_index(&path)?;
    let target_index = target_mode_index(&path)?;
    let source = *source_modes
        .get(source_index)
        .ok_or_else(|| format!("CCD candidate source mode index {} is invalid", source_index))?;
    let target = *source_modes
        .get(target_index)
        .ok_or_else(|| format!("CCD candidate target mode index {} is invalid", target_index))?;
    if source.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE || target.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
    {
        return Err("CCD candidate path has invalid source/target modes".into());
    }
    let new_source_index = modes.len() as u32;
    modes.push(source);
    let new_target_index = modes.len() as u32;
    modes.push(target);
    set_source_mode_index(&mut path, new_source_index)?;
    set_target_mode_index(&mut path, new_target_index)?;
    Ok(path)
}

fn source_mode_index(path: &DISPLAYCONFIG_PATH_INFO) -> Result<usize, String> {
    let (index, virtual_mode) = unsafe {
        if path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 {
            (path.sourceInfo.Anonymous.Anonymous._bitfield & 0xffff, true)
        } else {
            (path.sourceInfo.Anonymous.modeInfoIdx, false)
        }
    };
    if index == DISPLAYCONFIG_PATH_MODE_IDX_INVALID || (virtual_mode && index == 0xffff) {
        Err("CCD path has no source mode".into())
    } else {
        Ok(index as usize)
    }
}

fn target_mode_index(path: &DISPLAYCONFIG_PATH_INFO) -> Result<usize, String> {
    let (index, virtual_mode) = unsafe {
        if path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 {
            ((path.targetInfo.Anonymous.Anonymous._bitfield >> 16) & 0xffff, true)
        } else {
            (path.targetInfo.Anonymous.modeInfoIdx, false)
        }
    };
    if index == DISPLAYCONFIG_PATH_MODE_IDX_INVALID || (virtual_mode && index == 0xffff) {
        Err("CCD path has no target mode".into())
    } else {
        Ok(index as usize)
    }
}

fn set_source_mode_index(path: &mut DISPLAYCONFIG_PATH_INFO, index: u32) -> Result<(), String> {
    if index > 0xffff {
        return Err("CCD source mode index exceeds virtual-mode field".into());
    }
    unsafe {
        if path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 {
            let bits = path.sourceInfo.Anonymous.Anonymous._bitfield;
            path.sourceInfo.Anonymous.Anonymous._bitfield = (bits & 0xffff0000) | index;
        } else {
            path.sourceInfo.Anonymous.modeInfoIdx = index;
        }
    }
    Ok(())
}

fn set_target_mode_index(path: &mut DISPLAYCONFIG_PATH_INFO, index: u32) -> Result<(), String> {
    if index > 0xffff {
        return Err("CCD target mode index exceeds virtual-mode field".into());
    }
    unsafe {
        if path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE != 0 {
            let bits = path.targetInfo.Anonymous.Anonymous._bitfield;
            path.targetInfo.Anonymous.Anonymous._bitfield = (bits & 0x0000ffff) | (index << 16);
        } else {
            path.targetInfo.Anonymous.modeInfoIdx = index;
        }
    }
    Ok(())
}

fn non_overlapping_position(config: &DisplayConfig, virtual_index: usize) -> Result<POINTL, String> {
    let mut rightmost = 0i32;
    for (index, path) in config.paths.iter().enumerate() {
        if index == virtual_index || path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        let source_index = source_mode_index(path)?;
        let mode = config
            .modes
            .get(source_index)
            .ok_or_else(|| format!("CCD source mode index {} is invalid", source_index))?;
        let source = unsafe { &mode.Anonymous.sourceMode };
        rightmost = rightmost.max(source.position.x.saturating_add(source.width as i32));
    }
    Ok(POINTL { x: rightmost, y: 0 })
}

fn verify_config(
    expected_name: &str,
    adapter_luid: u64,
    width: u32,
    height: u32,
    refresh_hz: u32,
    expected_position: POINTL,
) -> Result<(), String> {
    let config = query_config(ACTIVE_QUERY_FLAGS)?;
    let index = find_source_path(&config.paths, adapter_luid, expected_name)?;
    let path = &config.paths[index];
    if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
        return Err("CCD virtual path is not active after apply".into());
    }
    if has_duplicate_active_source(&config.paths, index) {
        return Err("CCD virtual source remains cloned after apply".into());
    }
    let source_index = source_mode_index(path)?;
    let target_index = target_mode_index(path)?;
    let source_mode = config
        .modes
        .get(source_index)
        .ok_or_else(|| format!("CCD source mode index {} is invalid", source_index))?;
    let target_mode = config
        .modes
        .get(target_index)
        .ok_or_else(|| format!("CCD target mode index {} is invalid", target_index))?;
    let (source_width, source_height, source_position) = unsafe {
        let source = &source_mode.Anonymous.sourceMode;
        (source.width, source.height, source.position)
    };
    let (target_width, target_height, target_refresh) = unsafe {
        let signal = &target_mode.Anonymous.targetMode.targetVideoSignalInfo;
        (
            signal.activeSize.cx,
            signal.activeSize.cy,
            if signal.vSyncFreq.Denominator == 0 {
                0
            } else {
                signal.vSyncFreq.Numerator / signal.vSyncFreq.Denominator
            },
        )
    };
    if source_width != width
        || source_height != height
        || target_width != width
        || target_height != height
        || target_refresh != refresh_hz
        || source_position != expected_position
    {
        return Err(format!(
            "CCD verification mismatch: source={}x{} at ({},{}), target={}x{}@{}Hz",
            source_width,
            source_height,
            source_position.x,
            source_position.y,
            target_width,
            target_height,
            target_refresh
        ));
    }
    Ok(())
}

fn luid_value(luid: LUID) -> u64 {
    u64::from(luid.LowPart) | ((luid.HighPart as i64 as u64) << 32)
}
