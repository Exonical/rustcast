//! Temporary CCD topology and mode configuration for the FluxIdd monitor.

#![cfg(target_os = "windows")]

use std::mem::{size_of, MaybeUninit};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
const SNAPSHOT_MAGIC: &[u8; 8] = b"FLUXPRIV";
const SNAPSHOT_VERSION: u32 = 1;

struct DisplayConfig {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

/// Topology change a `SetDisplayConfig` validate/apply pair belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigOp {
    Privacy,
    Restore,
    Mode,
}

impl std::fmt::Display for ConfigOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Privacy => "privacy",
            Self::Restore => "restore",
            Self::Mode => "mode",
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CcdError {
    /// `SDC_VALIDATE` refused the supplied topology; nothing was applied.
    #[error("CCD {op} validate rejected (status {status}): SetDisplayConfig")]
    ValidateRejected { op: ConfigOp, status: i32 },
    /// `SDC_APPLY` failed after validation; the topology may be partially applied.
    #[error("CCD {op} apply failed (status {status}): SetDisplayConfig")]
    ApplyFailed { op: ConfigOp, status: i32 },
    #[error("CCD {op} verification mismatch: {detail}")]
    VerifyMismatch { op: ConfigOp, detail: String },
    #[error("{0}")]
    Topology(String),
    #[error("{action}: {source}")]
    SnapshotIo {
        action: &'static str,
        source: std::io::Error,
    },
    #[error("privacy topology snapshot {0}")]
    SnapshotInvalid(String),
    #[error("LockWorkStation failed: {0}")]
    LockWorkstation(windows::core::Error),
    #[error("CCD configuration did not settle within {timeout:?}: {last}")]
    Timeout { timeout: Duration, last: Box<CcdError> },
    #[error("privacy state lock poisoned")]
    StatePoisoned,
    #[error("privacy controller is shutting down")]
    ShuttingDown,
}

impl CcdError {
    fn topology(message: impl Into<String>) -> Self {
        Self::Topology(message.into())
    }

    fn verify_mismatch(op: ConfigOp, detail: impl Into<String>) -> Self {
        Self::VerifyMismatch {
            op,
            detail: detail.into(),
        }
    }

    fn snapshot_io(action: &'static str) -> impl FnOnce(std::io::Error) -> Self {
        move |source| Self::SnapshotIo { action, source }
    }

    fn snapshot_invalid(message: impl Into<String>) -> Self {
        Self::SnapshotInvalid(message.into())
    }
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
    next_connection: u64,
    connections: HashMap<u64, u32>,
}

#[derive(Clone)]
pub struct PrivacyConnection {
    inner: Arc<PrivacyConnectionState>,
}

struct PrivacyConnectionState {
    controller: Weak<Mutex<PrivacyState>>,
    token: u64,
}

impl Drop for PrivacyConnectionState {
    fn drop(&mut self) {
        if let Some(inner) = self.controller.upgrade() {
            if let Err(error) = (PrivacyController { inner }).disconnect(self.token) {
                tracing::error!("Privacy mode connection cleanup failed: {error}");
            }
        }
    }
}

impl PrivacyController {
    pub fn new(target: DisplayInfo, snapshot_path: PathBuf, lock_on_disconnect: bool) -> Result<Self, CcdError> {
        Ok(Self {
            inner: Arc::new(Mutex::new(PrivacyState {
                target,
                snapshot_path,
                lock_on_disconnect,
                privacy_applied: false,
                next_connection: 1,
                connections: HashMap::new(),
            })),
        })
    }

    pub fn connection(&self) -> PrivacyConnection {
        let mut state = self.inner.lock().expect("privacy state lock poisoned");
        let token = state.next_connection;
        state.next_connection = state.next_connection.wrapping_add(1).max(1);
        state.connections.insert(token, 0);
        PrivacyConnection {
            inner: Arc::new(PrivacyConnectionState {
                controller: Arc::downgrade(&self.inner),
                token,
            }),
        }
    }

    fn update_connection(&self, token: u64, count: u32) -> Result<(), CcdError> {
        let mut state = self.inner.lock().map_err(|_| CcdError::StatePoisoned)?;
        let was_viewed = state.connections.values().any(|viewers| *viewers > 0);
        state.connections.insert(token, count);
        let has_viewers = state.connections.values().any(|viewers| *viewers > 0);
        if !was_viewed && has_viewers && !state.privacy_applied {
            if let Err(error) = apply_privacy(&state.target, &state.snapshot_path) {
                match error {
                    // Validation rejects before anything is applied, so the
                    // snapshot is stale and the topology is untouched.
                    CcdError::ValidateRejected {
                        op: ConfigOp::Privacy, ..
                    } => {
                        let _ = std::fs::remove_file(&state.snapshot_path);
                        state.privacy_applied = false;
                        state.connections.insert(token, 0);
                    }
                    // An apply or verification error may mean that the
                    // temporary topology took effect. Keep the snapshot and
                    // force the disconnect path to attempt restoration.
                    _ if state.snapshot_path.exists() => state.privacy_applied = true,
                    _ => {
                        state.connections.insert(token, 0);
                    }
                }
                return Err(error);
            }
            state.privacy_applied = true;
            tracing::info!("Privacy mode enabled for {} remote viewer(s)", count);
        } else if was_viewed && !has_viewers && state.privacy_applied {
            restore_and_lock(&mut state)?;
        }
        Ok(())
    }

    fn disconnect(&self, token: u64) -> Result<(), CcdError> {
        let mut state = self.inner.lock().map_err(|_| CcdError::StatePoisoned)?;
        let was_viewed = state.connections.values().any(|viewers| *viewers > 0);
        state.connections.remove(&token);
        if was_viewed
            && !state.connections.values().any(|viewers| *viewers > 0)
            && state.privacy_applied
        {
            restore_and_lock(&mut state)?;
        }
        Ok(())
    }

    pub fn restore(&self) -> Result<(), CcdError> {
        let mut state = self.inner.lock().map_err(|_| CcdError::StatePoisoned)?;
        if !state.privacy_applied && !state.snapshot_path.exists() {
            return Ok(());
        }
        restore_snapshot(&state.snapshot_path)?;
        state.privacy_applied = false;
        state.connections.clear();
        tracing::info!("Restored physical display topology during shutdown");
        Ok(())
    }
}

impl PrivacyConnection {
    pub fn update(&self, count: u32) -> Result<(), CcdError> {
        let inner = self.inner.controller.upgrade().ok_or(CcdError::ShuttingDown)?;
        PrivacyController { inner }.update_connection(self.inner.token, count)
    }
}

fn restore_and_lock(state: &mut PrivacyState) -> Result<(), CcdError> {
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
    Ok(())
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

pub fn recover_snapshot(snapshot_path: &Path) {
    if !snapshot_path.exists() {
        return;
    }
    tracing::warn!(
        "Found an unclean privacy topology snapshot at {}; restoring it before startup",
        snapshot_path.display()
    );
    if let Err(error) = restore_snapshot(snapshot_path) {
        tracing::warn!(
            "Discarding unclean privacy topology snapshot after recovery failure: {error}"
        );
        if let Err(remove_error) = std::fs::remove_file(snapshot_path) {
            tracing::warn!(
                "Could not discard failed privacy topology snapshot {}: {remove_error}",
                snapshot_path.display()
            );
        }
    }
}

fn apply_privacy(target: &DisplayInfo, snapshot_path: &Path) -> Result<(), CcdError> {
    let config = query_config(ACTIVE_QUERY_FLAGS)?;
    let adapter_luid = target
        .adapter_luid
        .ok_or_else(|| CcdError::topology(format!("DXGI output {} has no adapter LUID", target.name)))?;
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

fn apply_and_verify_privacy(config: &DisplayConfig, target: &DisplayInfo, adapter_luid: u64) -> Result<(), CcdError> {
    set_display_config(config, ConfigOp::Privacy)?;

    let active = query_config(ACTIVE_QUERY_FLAGS)?;
    let (virtual_index, _) = find_source_path(&active.paths, adapter_luid, &target.name)?;
    if active.paths[virtual_index].flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
        return Err(CcdError::verify_mismatch(ConfigOp::Privacy, "virtual path is inactive"));
    }
    let all_paths = query_config(ALL_PATHS_QUERY_FLAGS)?;
    for path in all_paths.paths {
        if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        if source_name(path.sourceInfo.adapterId, path.sourceInfo.id)? != target.name {
            return Err(CcdError::verify_mismatch(
                ConfigOp::Privacy,
                "a physical source remains active",
            ));
        }
    }
    Ok(())
}

fn restore_snapshot(snapshot_path: &Path) -> Result<(), CcdError> {
    let config = read_snapshot(snapshot_path)?;
    set_display_config(&config, ConfigOp::Restore)?;

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
            return Err(CcdError::verify_mismatch(
                ConfigOp::Restore,
                "a saved active path is missing",
            ));
        }
    }
    std::fs::remove_file(snapshot_path).map_err(CcdError::snapshot_io("remove privacy topology snapshot"))?;
    Ok(())
}

fn write_snapshot(snapshot_path: &Path, config: &DisplayConfig) -> Result<(), CcdError> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(SNAPSHOT_MAGIC);
    bytes.extend_from_slice(&SNAPSHOT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(size_of::<DISPLAYCONFIG_PATH_INFO>() as u32).to_le_bytes());
    bytes.extend_from_slice(&(size_of::<DISPLAYCONFIG_MODE_INFO>() as u32).to_le_bytes());
    bytes.extend_from_slice(&boot_session_id().to_le_bytes());
    append_snapshot_vec(&mut bytes, &config.paths);
    append_snapshot_vec(&mut bytes, &config.modes);
    if let Some(parent) = snapshot_path.parent() {
        std::fs::create_dir_all(parent).map_err(CcdError::snapshot_io("create privacy snapshot directory"))?;
    }
    let temporary = snapshot_path.with_extension("tmp");
    std::fs::write(&temporary, bytes).map_err(CcdError::snapshot_io("write privacy topology snapshot"))?;
    std::fs::rename(&temporary, snapshot_path).map_err(CcdError::snapshot_io("commit privacy topology snapshot"))?;
    Ok(())
}

fn read_snapshot(snapshot_path: &Path) -> Result<DisplayConfig, CcdError> {
    let bytes = std::fs::read(snapshot_path).map_err(CcdError::snapshot_io("read privacy topology snapshot"))?;
    let mut offset = 0;
    if bytes.len() < 28 || &bytes[..8] != SNAPSHOT_MAGIC {
        return Err(CcdError::snapshot_invalid("has an invalid magic value"));
    }
    offset += 8;
    let version = read_snapshot_u32(&bytes, &mut offset)?;
    if version != SNAPSHOT_VERSION {
        return Err(CcdError::snapshot_invalid(format!("version {version} is unsupported")));
    }
    let path_size = read_snapshot_u32(&bytes, &mut offset)?;
    let mode_size = read_snapshot_u32(&bytes, &mut offset)?;
    if path_size as usize != size_of::<DISPLAYCONFIG_PATH_INFO>()
        || mode_size as usize != size_of::<DISPLAYCONFIG_MODE_INFO>()
    {
        return Err(CcdError::snapshot_invalid(format!(
            "struct sizes do not match (path {path_size}, mode {mode_size})"
        )));
    }
    let boot_session = read_snapshot_u64(&bytes, &mut offset)?;
    let current_boot_session = boot_session_id();
    if boot_session != current_boot_session {
        return Err(CcdError::snapshot_invalid(format!(
            "belongs to boot session {boot_session}, current session is {current_boot_session}"
        )));
    }
    let paths = read_snapshot_vec::<DISPLAYCONFIG_PATH_INFO>(&bytes, &mut offset)?;
    let modes = read_snapshot_vec::<DISPLAYCONFIG_MODE_INFO>(&bytes, &mut offset)?;
    if offset != bytes.len() {
        return Err(CcdError::snapshot_invalid("contains trailing data"));
    }
    Ok(DisplayConfig { paths, modes })
}

fn append_snapshot_vec<T: Copy>(bytes: &mut Vec<u8>, values: &[T]) {
    bytes.extend_from_slice(&(values.len() as u32).to_le_bytes());
    let raw = unsafe { std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of::<T>() * values.len()) };
    bytes.extend_from_slice(raw);
}

fn read_snapshot_vec<T: Copy>(bytes: &[u8], offset: &mut usize) -> Result<Vec<T>, CcdError> {
    let count = read_snapshot_u32(bytes, offset)? as usize;
    let size = size_of::<T>()
        .checked_mul(count)
        .ok_or_else(|| CcdError::snapshot_invalid("size overflow"))?;
    let end = offset
        .checked_add(size)
        .ok_or_else(|| CcdError::snapshot_invalid("offset overflow"))?;
    if end > bytes.len() {
        return Err(CcdError::snapshot_invalid("is truncated"));
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

fn read_snapshot_u32(bytes: &[u8], offset: &mut usize) -> Result<u32, CcdError> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| CcdError::snapshot_invalid("offset overflow"))?;
    if end > bytes.len() {
        return Err(CcdError::snapshot_invalid("is truncated"));
    }
    let value = u32::from_le_bytes(bytes[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(value)
}

fn read_snapshot_u64(bytes: &[u8], offset: &mut usize) -> Result<u64, CcdError> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| CcdError::snapshot_invalid("offset overflow"))?;
    if end > bytes.len() {
        return Err(CcdError::snapshot_invalid("is truncated"));
    }
    let value = u64::from_le_bytes(bytes[*offset..end].try_into().unwrap());
    *offset = end;
    Ok(value)
}

fn boot_session_id() -> u64 {
    let uptime_ms = unsafe {
        windows::Win32::System::SystemInformation::GetTickCount64()
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    now.saturating_sub(uptime_ms / 1000)
}

fn lock_workstation() -> Result<(), CcdError> {
    unsafe { windows::Win32::System::Shutdown::LockWorkStation().map_err(CcdError::LockWorkstation) }
}

/// Apply a temporary Extend topology and the requested mode to the virtual
/// source. No persistence flag is used; the user's saved display database is
/// intentionally left unchanged.
pub fn configure_virtual_display(
    target: &DisplayInfo,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Result<(), CcdError> {
    let adapter_luid = target
        .adapter_luid
        .ok_or_else(|| CcdError::topology(format!("DXGI output {} has no adapter LUID", target.name)))?;
    let deadline = Instant::now() + CONFIGURATION_TIMEOUT;
    let mut last_error = CcdError::topology("CCD path never appeared");
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

    Err(CcdError::Timeout {
        timeout: CONFIGURATION_TIMEOUT,
        last: Box::new(last_error),
    })
}

fn configure_once(
    target: &DisplayInfo,
    adapter_luid: u64,
    width: u32,
    height: u32,
    refresh_hz: u32,
    resolved_path_logged: &mut bool,
) -> Result<(), CcdError> {
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
                CcdError::topology(format!(
                    "CCD target {} has no independent source path for Extend",
                    active_path.targetInfo.id
                ))
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
        .ok_or_else(|| CcdError::topology(format!("CCD source mode index {} is invalid", source_index)))?;
    if source.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
        return Err(CcdError::topology(format!(
            "CCD source mode index {} is not a source mode",
            source_index
        )));
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
        .ok_or_else(|| CcdError::topology(format!("CCD target mode index {} is invalid", target_index)))?;
    if target_mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_TARGET {
        return Err(CcdError::topology(format!(
            "CCD target mode index {} is not a target mode",
            target_index
        )));
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

    set_display_config(&config, ConfigOp::Mode)?;

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

/// Validate `config` with `SDC_VALIDATE`, then apply it with `SDC_APPLY`.
fn set_display_config(config: &DisplayConfig, op: ConfigOp) -> Result<(), CcdError> {
    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), VALIDATE_FLAGS) };
    if status != 0 {
        return Err(CcdError::ValidateRejected { op, status });
    }
    let status = unsafe { SetDisplayConfig(Some(&config.paths), Some(&config.modes), APPLY_FLAGS) };
    if status != 0 {
        return Err(CcdError::ApplyFailed { op, status });
    }
    Ok(())
}

fn query_config(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<DisplayConfig, CcdError> {
    for _ in 0..QUERY_RETRIES {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        let status = unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
        if status.0 != 0 {
            return Err(CcdError::topology(format!(
                "CCD query failed (GetDisplayConfigBufferSizes status {})",
                status.0
            )));
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
            return Err(CcdError::topology(format!(
                "CCD query failed (QueryDisplayConfig status {})",
                status.0
            )));
        }
    }
    Err(CcdError::topology("CCD query failed: topology changed while querying"))
}

fn find_source_path(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    dxgi_adapter_luid: u64,
    expected_name: &str,
) -> Result<(usize, u64), CcdError> {
    let mut matches = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if source_name(path.sourceInfo.adapterId, path.sourceInfo.id)? == expected_name {
            matches.push((index, luid_value(path.sourceInfo.adapterId)));
        }
    }
    match matches.as_slice() {
        [] => Err(CcdError::topology(format!(
            "CCD path never appeared for source {}",
            expected_name
        ))),
        [match_] => Ok(*match_),
        matches => {
            let dxgi_matches: Vec<_> = matches
                .iter()
                .copied()
                .filter(|(_, luid)| *luid == dxgi_adapter_luid)
                .collect();
            match dxgi_matches.as_slice() {
                [match_] => Ok(*match_),
                _ => Err(CcdError::topology(format!(
                    "CCD source {} is ambiguous across {} paths (DXGI adapter LUID 0x{:016X})",
                    expected_name,
                    matches.len(),
                    dxgi_adapter_luid
                ))),
            }
        }
    }
}

fn source_name(adapter_id: LUID, source_id: u32) -> Result<String, CcdError> {
    let mut packet = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
    packet.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
        size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
        adapterId: adapter_id,
        id: source_id,
    };
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut packet as *mut _ as *mut DISPLAYCONFIG_DEVICE_INFO_HEADER) };
    if status != 0 {
        return Err(CcdError::topology(format!(
            "DisplayConfigGetDeviceInfo source {} failed: {}",
            source_id, status
        )));
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
) -> Result<DISPLAYCONFIG_PATH_INFO, CcdError> {
    let source_index = source_mode_index(&path)?;
    let target_index = target_mode_index(&path)?;
    let source = *source_modes
        .get(source_index)
        .ok_or_else(|| CcdError::topology(format!("CCD candidate source mode index {} is invalid", source_index)))?;
    let target = *source_modes
        .get(target_index)
        .ok_or_else(|| CcdError::topology(format!("CCD candidate target mode index {} is invalid", target_index)))?;
    if source.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE || target.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_TARGET
    {
        return Err(CcdError::topology("CCD candidate path has invalid source/target modes"));
    }
    let new_source_index = modes.len() as u32;
    modes.push(source);
    let new_target_index = modes.len() as u32;
    modes.push(target);
    set_source_mode_index(&mut path, new_source_index)?;
    set_target_mode_index(&mut path, new_target_index)?;
    Ok(path)
}

fn source_mode_index(path: &DISPLAYCONFIG_PATH_INFO) -> Result<usize, CcdError> {
    let index = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
    if index == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
        Err(CcdError::topology("CCD path has no source mode"))
    } else {
        Ok(index as usize)
    }
}

fn target_mode_index(path: &DISPLAYCONFIG_PATH_INFO) -> Result<usize, CcdError> {
    let index = unsafe { path.targetInfo.Anonymous.modeInfoIdx };
    if index == DISPLAYCONFIG_PATH_MODE_IDX_INVALID {
        Err(CcdError::topology("CCD path has no target mode"))
    } else {
        Ok(index as usize)
    }
}

fn set_source_mode_index(path: &mut DISPLAYCONFIG_PATH_INFO, index: u32) -> Result<(), CcdError> {
    path.sourceInfo.Anonymous.modeInfoIdx = index;
    Ok(())
}

fn set_target_mode_index(path: &mut DISPLAYCONFIG_PATH_INFO, index: u32) -> Result<(), CcdError> {
    path.targetInfo.Anonymous.modeInfoIdx = index;
    Ok(())
}

fn non_overlapping_position(config: &DisplayConfig, virtual_index: usize) -> Result<POINTL, CcdError> {
    let mut rightmost = 0i32;
    for (index, path) in config.paths.iter().enumerate() {
        if index == virtual_index || path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
            continue;
        }
        let source_index = source_mode_index(path)?;
        let mode = config
            .modes
            .get(source_index)
            .ok_or_else(|| CcdError::topology(format!("CCD source mode index {} is invalid", source_index)))?;
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
) -> Result<(), CcdError> {
    let config = query_config(ACTIVE_QUERY_FLAGS)?;
    let (index, ccd_adapter_luid) = find_source_path(&config.paths, dxgi_adapter_luid, expected_name)?;
    if ccd_adapter_luid != expected_ccd_adapter_luid {
        return Err(CcdError::verify_mismatch(
            ConfigOp::Mode,
            format!(
                "source {} resolved on unexpected adapter LUID 0x{:016X} (expected 0x{:016X})",
                expected_name, ccd_adapter_luid, expected_ccd_adapter_luid
            ),
        ));
    }
    let path = &config.paths[index];
    if path.flags & DISPLAYCONFIG_PATH_ACTIVE == 0 {
        return Err(CcdError::verify_mismatch(
            ConfigOp::Mode,
            "virtual path is not active after apply",
        ));
    }
    if has_duplicate_active_source(&config.paths, index) {
        return Err(CcdError::verify_mismatch(
            ConfigOp::Mode,
            "virtual source remains cloned after apply",
        ));
    }
    let source_index = source_mode_index(path)?;
    let target_index = target_mode_index(path)?;
    let source_mode = config
        .modes
        .get(source_index)
        .ok_or_else(|| CcdError::topology(format!("CCD source mode index {} is invalid", source_index)))?;
    let target_mode = config
        .modes
        .get(target_index)
        .ok_or_else(|| CcdError::topology(format!("CCD target mode index {} is invalid", target_index)))?;
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
        return Err(CcdError::verify_mismatch(
            ConfigOp::Mode,
            format!(
                "source={}x{} at ({},{}), target={}x{}@{}Hz",
                source_width,
                source_height,
                source_position.x,
                source_position.y,
                target_width,
                target_height,
                target_refresh
            ),
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
