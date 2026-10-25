pub mod capability;
pub mod config;
pub mod cursor;
pub mod error;
pub mod frame;
pub mod platform;
#[cfg(feature = "registration")]
pub mod registration;
pub mod types;

pub use cursor::{CursorBitmap, CursorMetadata};
pub use error::{FluxError, Result};
pub use types::*;
