pub mod backend;
pub mod events;
pub mod gamepad;
pub mod keyboard;
pub mod keymap;
pub mod mouse;
#[cfg(all(target_os = "linux", feature = "input-portal"))]
pub mod portal_input;
pub mod sink;

pub use backend::{select_input_backend, InputBackend, NoopInputBackend};
pub use events::InputEvent;
pub use keymap::scancode_to_evdev;
pub use sink::InputSink;
