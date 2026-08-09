//! Temporary CCD topology and mode configuration for the FluxIdd monitor.

#![cfg(target_os = "windows")]

use std::mem::{size_of, MaybeUninit};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use flux_capture::traits::DisplayInfo;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{LUID, POINTL};
use windows::Win32::Graphics::Gdi::{DISPLAYCONFIG_PATH_ACTIVE, DISPLAYCONFIG_PATH_MODE_IDX_INVALID};

// Deliberately omit *_VIRTUAL_MODE_AWARE. CCD supports the legacy-compatible
// path/mode view when callers do not opt into virtual-mode unions and desktop
// image entries; this keeps modeInfoIdx a plain index.
const ACTIVE_QUERY_FLAGS: QUERY_DISPLAY_CONFIG_FLAGS = QDC_ONLY_ACTIVE_PATHS;
const ALL_PATHS_QUERY_FLAGS: QUERY_DISPLAY_CONFIG_FLAGS = QDC_ALL_PATHS;
const VALIDATE_FLAGS: SET_DISPLAY_CONFIG_FLAGS =
    SET_DISPLAY_CONFIG_FLAGS(SDC_VALIDATE.0 | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0);
const APPLY_FLAGS: SET_DISPLAY_CONFIG_FLAGS = SET_DISPLAY_CONFIG_FLAGS(SDC_APPLY.0 | SDC_USE_SUPPLIED_DISPLAY_CONFIG.0);
const QUERY_RETRIES: usize = 4;
const CONFIGURATION_TIMEOUT: Duration = Duration::from_secs(5);

struct DisplayConfig {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

#[derive(Clone)]
pub struct PrivacyController {
    inner: Arc<Mutex<PrivacyState>>,
}

struct PrivacyState {
    target: DisplayInfo,
    snapshot_path: PathBuf,
    lock_on_disconnect: bool,
    privacy_applied: bool,
}

impl PrivacyController {
    pub fn new(target: DisplayInfo, snapshot_path: PathBuf, lock_on_disconnect: bool) -> Result<Self, String> {
        Ok(Self {
            inner: Arc::new(Mutex::new(PrivacyState {
                target,
                snapshot_path,
                lock_on_disconnect,
                privacy_applied: false,
            })),
        })
    }

    pub fn update_viewer_count(&self, count: u32) -> Result<(), String> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "privacy state lock poisoned".to_string())?;
        if count > 0 {
            if state.privacy_applied {
                return Ok(());
            }
            apply_privacy(&state.target, &state.snapshot_path)?;
            state.privacy_applied = true;
            tracing::info!("Privacy mode enabled for {} remote viewer(s)", count);
        } else if state.privacy_applied {
            restore_snapshot(&state.snapshot_path)?;
            state.privacy_applied = false;
            tracing::info!("Privacy mode disabled; physical display topology restored");
            if state.lock_on_disconnect {
                // LockWorkStation is asynchronous; success only means that the
                // request was initiated, not that the secure desktop is active.
                lock_workstation()?;
                tracing::info!(
                    "Workstation lock requested after privacy topology restoration; \
                     reconnecting viewers will see the lock screen until sign-in"
                );
            }
        }
        Ok(())
    }

    pub fn restore(&self) -> Result<(), String> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| "privacy state lock poisoned".to_string())?;
        if !state.privacy_applied && !state.snapshot_path.exists() {
            return Ok(());
        }
        restore_snapshot(&state.snapshot_path)?;
        state.privacy_applied = false;
        tracing::info!("Restored physical display topology during shutdown");
        Ok(())
    }
}

impl Drop for PrivacyController {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) != 1 {
            return;
        }
        if let Ok(mut state) = self.inner.lock() {
            if state.privacy_applied || state.snapshot_path.exists() {
                if let Err(error) = restore_snapshot(&state.snapshot_path) {
                    tracing::error!("Failed to restore privacy topology during drop: {error}");
                } else {
                    state.privacy_applied = false;
                    tracing::info!("Restored physical display topology during drop");
                }
            }
        }
    }
}

pub fn recover_snapshot(snapshot_path: &Path) -> Result<(), String> {
    if !snapshot_path.exists() {
        return Ok(());
    }
    tracing::warn!(
        "Found an unclean privacy topology snapshot at {}; restoring it before startup",
        snapshot_path.display()
    );
    restore_snapshot(snapshot_path)
}

fn apply_privacy(target: &DisplayInfo, snapshot_path: &Path) -> Result<(), String> {
    let config = query_config(ACTIVE_QUERY_FLAGS)?;
    let adapter_luid = target
        .adapter_luid
        .ok_or_else(|| format!("DXGI output {} has no adapter LUID", target.name))?;
    let (virtual_index, _) = find_source_path(&config.paths, adapter_luid, &target.name)?;
    write_snapshot(snapshot_path, &config)?;

    let mut privacy_config = config;
    for (index, path) in privacy_config.paths.iter_mut().enumerate() {
        if index == virtual_index {
            path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
        } else {
            path.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
        }
    }
    apply_and_verify_privacy(&privacy_config, target, adapter_luid)
}

fn apply_and_verify_privacy(config: &DisplayConfig, target: &DisplayInfo, adapter_luid: u64) -> Result<(), String> {
    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), VALIDATE_FLAGS) };
    if status != 0 {
        return Err(format!(
            "CCD privacy validate rejected (status {}): SetDisplayConfig",
            status
        ));
    }
    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), APPLY_FLAGS) };
    if status != 0 {
        return Err(format!(
            "CCD privacy apply failed (status {}): SetDisplayConfig",
            status
        ));
    }

    let active = query_config(ACTIVE_QUERY_FLAGS)?;
    let (virtual_index, _) = find_source_path(&active.paths, adapter_luid, &target.name)?;
    if active.paths[virtual_index].flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
        return Err("CCD privacy verification mismatch: virtual path is inactive".into());
    }
    let all_paths = query_config(ALL_PATHS_QUERY_FLAGS)?;
    for path in all_paths.paths {
        if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        if source_name(path.sourceInfo.adapterId, path.sourceInfo.id)? != target.name {
            return Err("CCD privacy verification mismatch: a physical source remains active".into());
        }
    }
    Ok(())
}

fn restore_snapshot(snapshot_path: &Path) -> Result<(), String> {
    let config = read_snapshot(snapshot_path)?;
    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), VALIDATE_FLAGS) };
    if status != 0 {
        return Err(format!(
            "CCD restore validate rejected (status {}): SetDisplayConfig",
            status
        ));
    }
    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), APPLY_FLAGS) };
    if status != 0 {
        return Err(format!(
            "CCD restore apply failed (status {}): SetDisplayConfig",
            status
        ));
    }

    let active = query_config(ACTIVE_QUERY_FLAGS)?;
    for expected in &config.paths {
        if expected.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        if !active.paths.iter().any(|actual| {
            actual.flags & DISPLAYCONFIG_PATH_ACTIVE != 0
                && actual.sourceInfo.adapterId == expected.sourceInfo.adapterId
                && actual.sourceInfo.id == expected.sourceInfo.id
                && actual.targetInfo.adapterId == expected.targetInfo.adapterId
                && actual.targetInfo.id == expected.targetInfo.id
        }) {
            return Err("CCD restore verification mismatch: a saved active path is missing".into());
        }
    }
    std::fs::remove_file(snapshot_path).map_err(|error| format!("remove privacy topology snapshot: {error}"))?;
    Ok(())
}

fn write_snapshot(snapshot_path: &Path, config: &DisplayConfig) -> Result<(), String> {
    let mut bytes = Vec::new();
    append_snapshot_vec(&mut bytes, &config.paths);
    append_snapshot_vec(&mut bytes, &config.modes);
    if let Some(parent) = snapshot_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("create privacy snapshot directory: {error}"))?;
    }
    let temporary = snapshot_path.with_extension("tmp");
    std::fs::write(&temporary, bytes).map_err(|error| format!("write privacy topology snapshot: {error}"))?;
    std::fs::rename(&temporary, snapshot_path).map_err(|error| format!("commit privacy topology snapshot: {error}"))?;
    Ok(())
}

fn read_snapshot(snapshot_path: &Path) -> Result<DisplayConfig, String> {
    let bytes = std::fs::read(snapshot_path).map_err(|error| format!("read privacy topology snapshot: {error}"))?;
    let mut offset = 0;
    let paths = read_snapshot_vec::<DISPLAYCONFIG_PATH_INFO>(&bytes, &mut offset)?;
    let modes = read_snapshot_vec::<DISPLAYCONFIG_MODE_INFO>(&bytes, &mut offset)?;
    if offset != bytes.len() {
        return Err("privacy topology snapshot contains trailing data".into());
    }
    Ok(DisplayConfig { paths, modes })
}

fn append_snapshot_vec<T: Copy>(bytes: &mut Vec<u8>, values: &[T]) {
    bytes.extend_from_slice(&(values.len() as u32).to_le_bytes());
    let raw = unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of::<T>() * values.len()) };
    bytes.extend_from_slice(raw);
}

fn read_snapshot_vec<T: Copy>(bytes: &[u8], offset: &mut usize) -> Result<Vec<T>, String> {
    let count = read_snapshot_u32(bytes, offset)? as usize;
    let size = size_of::<T>()
        .checked_mul(count)
        .ok_or_else(|| "privacy topology snapshot size overflow".to_string())?;
    let end = offset
        .checked_add(size)
        .ok_or_else(|| "privacy topology snapshot offset overflow".to_string())?;
    if end > bytes.len() {
        return Err("privacy topology snapshot is truncated".into());
    }
    let mut values = Vec::with_capacity(count);
    for chunk in bytes[*offset..end].chunks_exact(size_of::<T>()) {
        let mut value = MaybeUninit::<T>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(chunk.as_ptr(), value.as_mut_ptr().cast::<u8>(), size_of::<T>());
            values.push(value.assume_init());
        }
    }
    *offset = end;
    Ok(values)
}

fn read_snapshot_u32(bytes: &[u8], offset: &mut usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "privacy topology snapshot offset overflow".to_string())?;
    if end > bytes.len() {
        return Err("privacy topology snapshot is truncated".into());
    }
    let value = u32::from_le_bytes(bytes[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(value)
}

fn lock_workstation() -> Result<(), String> {
    unsafe {
        windows::Win32::System::Shutdown::LockWorkStation().map_err(|error| format!("LockWorkStation failed: {error}"))
    }
}

/// Apply a temporary Extend topology and the requested mode to the virtual
/// source. No persistence flag is used; the user's saved display database is
/// intentionally left unchanged.
pub fn configure_virtual_display(target: &DisplayInfo, width: u32, height: u32, refresh_hz: u32) -> Result<(), String> {
    let adapter_luid = target
        .adapter_luid
        .ok_or_else(|| format!("DXGI output {} has no adapter LUID", target.name))?;
    let deadline = Instant::now() + CONFIGURATION_TIMEOUT;
    let mut last_error = "CCD path never appeared".to_string();
    let mut resolved_path_logged = false;

    while Instant::now() < deadline {
        match configure_once(
            target,
            adapter_luid,
            width,
            height,
            refresh_hz,
            &mut resolved_path_logged,
        ) {
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
    resolved_path_logged: &mut bool,
) -> Result<(), String> {
    let mut config = query_config(ACTIVE_QUERY_FLAGS)?;
    let (virtual_index, ccd_adapter_luid) = find_source_path(&config.paths, adapter_luid, &target.name)?;
    if !*resolved_path_logged {
        tracing::info!(
            "Resolved virtual CCD source {}: CCD adapter LUID 0x{:016X}, DXGI adapter LUID 0x{:016X}",
            target.name,
            ccd_adapter_luid,
            adapter_luid
        );
        *resolved_path_logged = true;
    }

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
        return Err(format!("CCD validate rejected (status {}): SetDisplayConfig", status));
    }
    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), APPLY_FLAGS) };
    if status != 0 {
        return Err(format!("CCD apply failed (status {}): SetDisplayConfig", status));
    }

    verify_config(
        &target.name,
        adapter_luid,
        ccd_adapter_luid,
        width,
        height,
        refresh_hz,
        position,
    )
}

fn query_config(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<DisplayConfig, String> {
    for _ in 0..QUERY_RETRIES {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        let status = unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
        if status.0 != 0 {
            return Err(format!(
                "CCD query failed (GetDisplayConfigBufferSizes status {})",
                status.0
            ));
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
            return Err(format!("CCD query failed (QueryDisplayConfig status {})", status.0));
        }
    }
    Err("CCD query failed: topology changed while querying".into())
}

fn find_source_path(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    dxgi_adapter_luid: u64,
    expected_name: &str,
) -> Result<(usize, u64), String> {
    let mut matches = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if source_name(path.sourceInfo.adapterId, path.sourceInfo.id)? == expected_name {
            matches.push((index, luid_value(path.sourceInfo.adapterId)));
        }
    }
    match matches.as_slice() {
        [] => Err(format!("CCD path never appeared for source {}", expected_name)),
        [match_] => Ok(*match_),
        matches => {
            let dxgi_matches: Vec<_> = matches
                .iter()
                .copied()
                .filter(|(_, luid)| *luid == dxgi_adapter_luid)
                .collect();
            match dxgi_matches.as_slice() {
                [match_] => Ok(*match_),
                _ => Err(format!(
                    "CCD source {} is ambiguous across {} paths (DXGI adapter LUID 0x{:016X})",
                    expected_name,
                    matches.len(),
                    dxgi_adapter_luid
                )),
            }
        }
    }
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
    let index = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
    if index == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
        Err("CCD path has no source mode".into())
    } else {
        Ok(index as usize)
    }
}

fn target_mode_index(path: &DISPLAYCONFIG_PATH_INFO) -> Result<usize, String> {
    let index = unsafe { path.targetInfo.Anonymous.modeInfoIdx };
    if index == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
        Err("CCD path has no target mode".into())
    } else {
        Ok(index as usize)
    }
}

fn set_source_mode_index(path: &mut DISPLAYCONFIG_PATH_INFO, index: u32) -> Result<(), String> {
    path.sourceInfo.Anonymous.modeInfoIdx = index;
    Ok(())
}

fn set_target_mode_index(path: &mut DISPLAYCONFIG_PATH_INFO, index: u32) -> Result<(), String> {
    path.targetInfo.Anonymous.modeInfoIdx = index;
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
    dxgi_adapter_luid: u64,
    expected_ccd_adapter_luid: u64,
    width: u32,
    height: u32,
    refresh_hz: u32,
    expected_position: POINTL,
) -> Result<(), String> {
    let config = query_config(ACTIVE_QUERY_FLAGS)?;
    let (index, ccd_adapter_luid) = find_source_path(&config.paths, dxgi_adapter_luid, expected_name)?;
    if ccd_adapter_luid != expected_ccd_adapter_luid {
        return Err(format!(
            "CCD verification resolved source {} on unexpected adapter LUID 0x{:016X} (expected 0x{:016X})",
            expected_name, ccd_adapter_luid, expected_ccd_adapter_luid
        ));
    }
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
    if source_position != expected_position {
        tracing::warn!(
            "CCD virtual source {} was placed at ({},{}), requested ({},{})",
            expected_name,
            source_position.x,
            source_position.y,
            expected_position.x,
            expected_position.y
        );
    }
    Ok(())
}

fn luid_value(luid: LUID) -> u64 {
    u64::from(luid.LowPart) | ((luid.HighPart as i64 as u64) << 32)
}
