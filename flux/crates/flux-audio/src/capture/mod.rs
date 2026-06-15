#[cfg(target_os = "windows")]
pub mod wasapi;

#[cfg(target_os = "linux")]
pub mod pipewire;
