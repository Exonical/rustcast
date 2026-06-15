//! Pluggable input-injection backends.
//!
//! On Wayland there is no single way to inject input; the right mechanism
//! depends on the compositor and the permissions granted (see the project
//! plan: portal `RemoteDesktop` → libei → uinput). [`InputBackend`] is the
//! low-level primitive interface those mechanisms implement; the higher-level
//! [`InputSink`](crate::sink::InputSink) translates wire [`InputEvent`]s into
//! these calls.

use flux_core::capability::InputBackendKind;
use flux_core::error::Result;

use crate::mouse::MouseButton;

/// A low-level input-injection backend (portal, libei, uinput, SendInput…).
pub trait InputBackend: Send + Sync {
    /// Human-readable backend name.
    fn name(&self) -> &'static str;

    /// Whether this backend can place the pointer at an absolute position.
    /// uinput, for example, cannot map to screen space across monitors.
    fn supports_absolute(&self) -> bool;

    /// Relative pointer motion, in device pixels.
    fn pointer_motion(&self, dx: f64, dy: f64) -> Result<()>;

    /// Absolute pointer motion, normalized to `0.0..=1.0` of the target
    /// stream/output.
    fn pointer_absolute(&self, x: f64, y: f64) -> Result<()>;

    /// Press or release a mouse button.
    fn pointer_button(&self, button: MouseButton, down: bool) -> Result<()>;

    /// Scroll axis motion (`dx` horizontal, `dy` vertical).
    fn pointer_axis(&self, dx: f64, dy: f64) -> Result<()>;

    /// Press or release a key by Linux evdev keycode.
    fn key(&self, evdev_code: u32, down: bool) -> Result<()>;
}

/// A backend that logs and discards events.
///
/// Used as the Linux placeholder until the portal/libei/uinput backends land,
/// so the pipeline can run end-to-end without injecting anything.
pub struct NoopInputBackend {
    kind: InputBackendKind,
}

impl NoopInputBackend {
    pub fn new(kind: InputBackendKind) -> Self {
        Self { kind }
    }
}

impl InputBackend for NoopInputBackend {
    fn name(&self) -> &'static str {
        "noop"
    }
    fn supports_absolute(&self) -> bool {
        // The portal/libei backends this stands in for do support absolute.
        !matches!(self.kind, InputBackendKind::Uinput)
    }
    fn pointer_motion(&self, dx: f64, dy: f64) -> Result<()> {
        tracing::trace!("noop pointer_motion dx={dx} dy={dy}");
        Ok(())
    }
    fn pointer_absolute(&self, x: f64, y: f64) -> Result<()> {
        tracing::trace!("noop pointer_absolute x={x} y={y}");
        Ok(())
    }
    fn pointer_button(&self, button: MouseButton, down: bool) -> Result<()> {
        tracing::trace!("noop pointer_button {button:?} down={down}");
        Ok(())
    }
    fn pointer_axis(&self, dx: f64, dy: f64) -> Result<()> {
        tracing::trace!("noop pointer_axis dx={dx} dy={dy}");
        Ok(())
    }
    fn key(&self, evdev_code: u32, down: bool) -> Result<()> {
        tracing::trace!("noop key code={evdev_code} down={down}");
        Ok(())
    }
}

/// Select an input backend for the given kind.
///
/// With the `input-portal` feature on Linux, [`InputBackendKind::Portal`]
/// resolves to the real `xdg-desktop-portal` RemoteDesktop backend; if its
/// negotiation fails we fall back to [`NoopInputBackend`] so the pipeline still
/// runs. Other kinds (libei, uinput) remain placeholders for now.
pub fn select_input_backend(kind: InputBackendKind) -> Box<dyn InputBackend> {
    #[cfg(all(target_os = "linux", feature = "input-portal"))]
    if matches!(kind, InputBackendKind::Portal) {
        match crate::portal_input::PortalInputBackend::new() {
            Ok(backend) => return Box::new(backend),
            Err(e) => {
                tracing::warn!("portal input backend unavailable ({e}); falling back to noop");
            }
        }
    }

    Box::new(NoopInputBackend::new(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_backend_accepts_all_events() {
        let be = select_input_backend(InputBackendKind::Portal);
        assert_eq!(be.name(), "noop");
        assert!(be.supports_absolute());
        be.pointer_motion(1.0, -2.0).unwrap();
        be.pointer_absolute(0.5, 0.5).unwrap();
        be.pointer_button(MouseButton::Left, true).unwrap();
        be.pointer_axis(0.0, 1.0).unwrap();
        be.key(30, true).unwrap();
    }

    #[test]
    fn uinput_kind_reports_no_absolute() {
        let be = select_input_backend(InputBackendKind::Uinput);
        assert!(!be.supports_absolute());
    }
}
