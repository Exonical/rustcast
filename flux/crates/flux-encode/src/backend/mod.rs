pub mod nvenc;
pub mod vulkan;
pub mod software;

#[cfg(target_os = "windows")]
pub mod amf {
    pub mod ffi;
    pub mod constants;
    mod encoder;
    pub use encoder::{AmfEncoder, AmfSession};
}

#[cfg(target_os = "linux")]
pub mod vaapi;
