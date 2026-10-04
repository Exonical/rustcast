//! Delivery checks shared by the platform keyboard and mouse sinks.

use flux_core::{FluxError, Result};

/// Turn `SendInput`'s inserted-event count into a result. Windows returns
/// fewer than `submitted` when it blocks injection (UIPI, secure desktop).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub(crate) fn check_inserted(
    device: &str,
    inserted: u32,
    submitted: usize,
    cause: impl FnOnce() -> String,
) -> Result<()> {
    if inserted as usize == submitted {
        return Ok(());
    }
    Err(FluxError::Input(format!(
        "SendInput inserted {inserted} of {submitted} {device} event(s): {}",
        cause()
    )))
}

/// Inject `inputs` via `SendInput`, failing if the OS dropped any of them.
#[cfg(target_os = "windows")]
pub(crate) fn send_input(device: &str, inputs: &[windows::Win32::UI::Input::KeyboardAndMouse::INPUT]) -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{INPUT, SendInput};

    let inserted = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    check_inserted(device, inserted, inputs.len(), || {
        windows::core::Error::from_thread().to_string()
    })
}

/// Error for a platform sink with no injection implementation on this OS.
#[cfg(not(target_os = "windows"))]
pub(crate) fn not_implemented(device: &str) -> FluxError {
    FluxError::UnsupportedPlatform(format!(
        "{device} injection is not implemented on {}; route input through a capture-session InputBackend",
        std::env::consts::OS
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_insert_count_is_ok() {
        check_inserted("keyboard", 1, 1, || unreachable!()).unwrap();
        check_inserted("mouse", 0, 0, || unreachable!()).unwrap();
    }

    #[test]
    fn short_insert_count_is_an_input_error() {
        let err = check_inserted("mouse", 0, 1, || "Access is denied.".into()).unwrap_err();
        match err {
            FluxError::Input(msg) => {
                assert!(msg.contains("0 of 1 mouse"), "{msg}");
                assert!(msg.contains("Access is denied."), "{msg}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn not_implemented_is_unsupported_platform() {
        assert!(matches!(
            not_implemented("keyboard"),
            FluxError::UnsupportedPlatform(msg) if msg.contains("keyboard")
        ));
    }
}
