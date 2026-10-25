//! Platform display hooks: privacy mode and the virtual display handle.
//!
//! Windows re-exports the CCD/IddCx implementations; other platforms get
//! uninhabited stand-ins with the same API, so callers need no `#[cfg]`.
//! Privacy mode and virtual displays are rejected at startup off Windows,
//! so the stand-ins are never constructed.

#[cfg(target_os = "windows")]
pub(crate) use crate::ccd_display::{PrivacyConnection, PrivacyController, recover_snapshot};
#[cfg(target_os = "windows")]
pub(crate) use crate::virtual_display::VirtualDisplay as VirtualDisplayHandle;

#[cfg(not(target_os = "windows"))]
pub(crate) use stub::{PrivacyConnection, PrivacyController, VirtualDisplayHandle, recover_snapshot};

#[cfg(not(target_os = "windows"))]
mod stub {
    use std::path::{Path, PathBuf};

    use flux_capture::traits::DisplayInfo;

    /// Never constructed: [`PrivacyController::new`] always fails here.
    #[derive(Clone)]
    pub(crate) enum PrivacyController {}

    impl PrivacyController {
        pub(crate) fn new(
            _target: DisplayInfo,
            _snapshot_path: PathBuf,
            _lock_on_disconnect: bool,
        ) -> Result<Self, String> {
            Err("privacy mode is only supported on Windows".into())
        }

        pub(crate) fn connection(&self) -> PrivacyConnection {
            match *self {}
        }

        pub(crate) fn restore(&self) -> Result<(), String> {
            match *self {}
        }
    }

    /// Never constructed: only a [`PrivacyController`] hands these out.
    #[derive(Clone)]
    pub(crate) enum PrivacyConnection {}

    impl PrivacyConnection {
        pub(crate) fn update(&self, _count: u32) -> Result<(), String> {
            match *self {}
        }
    }

    /// Never constructed: virtual displays are only plugged in on Windows.
    pub(crate) enum VirtualDisplayHandle {}

    /// No privacy topology snapshot can exist off Windows.
    pub(crate) fn recover_snapshot(_snapshot_path: &Path) {}
}
