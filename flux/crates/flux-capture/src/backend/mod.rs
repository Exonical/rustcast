#[cfg(target_os = "windows")]
pub mod dxgi;

#[cfg(target_os = "linux")]
pub mod pipewire;

#[cfg(target_os = "linux")]
pub mod drm;
