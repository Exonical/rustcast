//! Mouse input events and virtual mouse injection.

use serde::{Deserialize, Serialize};
use std::sync::RwLock;

use flux_core::types::DesktopRect;

/// A mouse event from the remote client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MouseEvent {
    /// Relative mouse movement (delta).
    Move {
        dx: i32,
        dy: i32,
    },

    /// Absolute mouse position (normalized 0.0–1.0).
    MoveAbsolute {
        x: f32,
        y: f32,
    },

    /// A mouse button was pressed.
    ButtonDown {
        button: MouseButton,
    },

    /// A mouse button was released.
    ButtonUp {
        button: MouseButton,
    },

    /// Mouse wheel scrolled.
    Scroll {
        /// Horizontal scroll delta.
        dx: i32,
        /// Vertical scroll delta.
        dy: i32,
    },
}

/// Mouse button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// Injects mouse events into the host OS.
pub struct MouseSink {
    /// Capture target output rectangle in virtual-desktop coordinates.
    target_rect: RwLock<DesktopRect>,
}

impl MouseSink {
    pub fn new(target_rect: DesktopRect) -> flux_core::Result<Self> {
        tracing::debug!(
            "Initializing mouse input sink at ({}, {}) {}x{}",
            target_rect.left,
            target_rect.top,
            target_rect.width,
            target_rect.height,
        );
        Ok(Self {
            target_rect: RwLock::new(target_rect),
        })
    }

    /// Update the output receiving absolute input after a display/topology change.
    pub fn set_target_rect(&self, target_rect: DesktopRect) -> flux_core::Result<()> {
        *self
            .target_rect
            .write()
            .map_err(|_| flux_core::FluxError::Input("mouse target lock poisoned".into()))? =
            target_rect;
        Ok(())
    }

    /// Inject a mouse event into the host OS.
    pub fn inject(&self, event: &MouseEvent) -> flux_core::Result<()> {
        #[cfg(target_os = "windows")]
        return self.inject_windows(event);

        #[cfg(not(target_os = "windows"))]
        {
            tracing::trace!("Input injection not implemented for this OS: {:?}", event);
            Ok(())
        }
    }

    #[cfg(target_os = "windows")]
    fn inject_windows(&self, event: &MouseEvent) -> flux_core::Result<()> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_LEFTDOWN,
            MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE,
            MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
            MOUSEEVENTF_XUP, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
            SM_YVIRTUALSCREEN,
        };

        // Standard Win32 XBUTTON values
        const XBUTTON1: u32 = 0x0001;
        const XBUTTON2: u32 = 0x0002;

        let target_rect = *self
            .target_rect
            .read()
            .map_err(|_| flux_core::FluxError::Input("mouse target lock poisoned".into()))?;
        let virtual_rect = DesktopRect {
            left: unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) },
            top: unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) },
            width: unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as u32 },
            height: unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as u32 },
        };

        let (dw_flags, dx, dy, mouse_data) = match event {
            MouseEvent::Move { dx, dy } => (MOUSEEVENTF_MOVE, *dx, *dy, 0),
            MouseEvent::MoveAbsolute { x, y } => {
                let (absolute_x, absolute_y) =
                    normalized_to_virtual_absolute(*x as f64, *y as f64, target_rect, virtual_rect);
                (
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    absolute_x,
                    absolute_y,
                    0,
                )
            }
            MouseEvent::ButtonDown { button } => match button {
                MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, 0, 0, 0),
                MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, 0, 0, 0),
                MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, 0, 0, 0),
                MouseButton::Back => (MOUSEEVENTF_XDOWN, 0, 0, XBUTTON1),
                MouseButton::Forward => (MOUSEEVENTF_XDOWN, 0, 0, XBUTTON2),
            },
            MouseEvent::ButtonUp { button } => match button {
                MouseButton::Left => (MOUSEEVENTF_LEFTUP, 0, 0, 0),
                MouseButton::Right => (MOUSEEVENTF_RIGHTUP, 0, 0, 0),
                MouseButton::Middle => (MOUSEEVENTF_MIDDLEUP, 0, 0, 0),
                MouseButton::Back => (MOUSEEVENTF_XUP, 0, 0, XBUTTON1),
                MouseButton::Forward => (MOUSEEVENTF_XUP, 0, 0, XBUTTON2),
            },
            MouseEvent::Scroll { dx, dy } => {
                if *dx != 0 {
                    let input = INPUT {
                        r#type: INPUT_MOUSE,
                        Anonymous: INPUT_0 {
                            mi: MOUSEINPUT {
                                dx: 0,
                                dy: 0,
                                mouseData: *dx as u32,
                                dwFlags: MOUSEEVENTF_HWHEEL,
                                time: 0,
                                dwExtraInfo: 0,
                            },
                        },
                    };
                    unsafe {
                        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
                    }
                }
                if *dy == 0 {
                    return Ok(());
                }
                (MOUSEEVENTF_WHEEL, 0, 0, *dy as u32)
            }
        };

        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data,
                    dwFlags: dw_flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };

        unsafe {
            SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        }

        Ok(())
    }
}

/// Map normalized coordinates in the captured output to SendInput's virtual
/// desktop coordinate range. The target and virtual rectangles may have
/// negative origins and do not need to share the same top-left corner.
#[allow(dead_code)]
pub(crate) fn normalized_to_virtual_absolute(
    x: f64,
    y: f64,
    target: DesktopRect,
    virtual_desktop: DesktopRect,
) -> (i32, i32) {
    let x = if x.is_finite() { x.clamp(0.0, 1.0) } else { 0.0 };
    let y = if y.is_finite() { y.clamp(0.0, 1.0) } else { 0.0 };
    let target_x = target.left as f64 + x * target.width.saturating_sub(1) as f64;
    let target_y = target.top as f64 + y * target.height.saturating_sub(1) as f64;
    let virtual_width = virtual_desktop.width.saturating_sub(1).max(1) as f64;
    let virtual_height = virtual_desktop.height.saturating_sub(1).max(1) as f64;
    let absolute_x =
        ((target_x - virtual_desktop.left as f64) / virtual_width * 65535.0).round();
    let absolute_y =
        ((target_y - virtual_desktop.top as f64) / virtual_height * 65535.0).round();
    (
        absolute_x.clamp(0.0, 65535.0) as i32,
        absolute_y.clamp(0.0, 65535.0) as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> DesktopRect {
        DesktopRect {
            left: 2560,
            top: 0,
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn maps_non_zero_origin_target_corners() {
        let virtual_desktop = DesktopRect {
            left: 0,
            top: 0,
            width: 4480,
            height: 1080,
        };
        assert_eq!(
            normalized_to_virtual_absolute(0.0, 0.0, target(), virtual_desktop),
            (37457, 0)
        );
        assert_eq!(
            normalized_to_virtual_absolute(1.0, 1.0, target(), virtual_desktop),
            (65535, 65535)
        );
    }

    #[test]
    fn maps_target_with_negative_virtual_origin() {
        let virtual_desktop = DesktopRect {
            left: -1920,
            top: -200,
            width: 6400,
            height: 2160,
        };
        assert_eq!(
            normalized_to_virtual_absolute(0.0, 0.0, target(), virtual_desktop),
            (45882, 6071)
        );
        assert_eq!(
            normalized_to_virtual_absolute(1.0, 1.0, target(), virtual_desktop),
            (65535, 38823)
        );
    }

    #[test]
    fn clamps_out_of_range_input_to_target_corners() {
        let virtual_desktop = DesktopRect {
            left: 0,
            top: 0,
            width: 4480,
            height: 1080,
        };
        assert_eq!(
            normalized_to_virtual_absolute(-1.0, 2.0, target(), virtual_desktop),
            (37457, 65535)
        );
    }
}
