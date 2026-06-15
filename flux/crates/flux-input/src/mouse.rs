//! Mouse input events and virtual mouse injection.

use serde::{Deserialize, Serialize};

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
    /// Display resolution for absolute coordinate mapping.
    _screen_width: u32,
    _screen_height: u32,
}

impl MouseSink {
    pub fn new(screen_width: u32, screen_height: u32) -> flux_core::Result<Self> {
        tracing::debug!(
            "Initializing mouse input sink ({}x{})",
            screen_width,
            screen_height
        );
        Ok(Self {
            _screen_width: screen_width,
            _screen_height: screen_height,
        })
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
            MOUSEEVENTF_XUP, MOUSEINPUT,
        };

        // Standard Win32 XBUTTON values
        const XBUTTON1: u32 = 0x0001;
        const XBUTTON2: u32 = 0x0002;

        let (dw_flags, dx, dy, mouse_data) = match event {
            MouseEvent::Move { dx, dy } => (MOUSEEVENTF_MOVE, *dx, *dy, 0),
            MouseEvent::MoveAbsolute { x, y } => (
                MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                (*x * 65535.0) as i32,
                (*y * 65535.0) as i32,
                0,
            ),
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
            MouseEvent::Scroll { dx: _, dy } => (MOUSEEVENTF_WHEEL, 0, 0, *dy as u32),
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
