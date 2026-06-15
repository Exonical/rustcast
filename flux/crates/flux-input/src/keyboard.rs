//! Keyboard input events and virtual key injection.

use serde::{Deserialize, Serialize};

/// A keyboard event from the remote client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum KeyboardEvent {
    /// A key was pressed.
    KeyDown {
        /// Platform-independent scan code.
        scan_code: u16,
        /// Virtual key code (platform-specific, optional).
        key_code: Option<u16>,
        /// Modifier flags active at the time of the event (raw bitmask).
        modifiers: u16,
    },

    /// A key was released.
    KeyUp {
        scan_code: u16,
        key_code: Option<u16>,
        modifiers: u16,
    },
}

impl KeyboardEvent {
    /// Get the modifier flags as a typed bitflags value.
    pub fn modifier_flags(&self) -> ModifierFlags {
        match self {
            Self::KeyDown { modifiers, .. } | Self::KeyUp { modifiers, .. } => {
                ModifierFlags::from_bits_truncate(*modifiers)
            }
        }
    }
}

bitflags::bitflags! {
    /// Active keyboard modifier flags.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    pub struct ModifierFlags: u16 {
        const SHIFT     = 0x0001;
        const CTRL      = 0x0002;
        const ALT       = 0x0004;
        const META      = 0x0008; // Win key / Super
        const CAPS_LOCK = 0x0010;
        const NUM_LOCK  = 0x0020;
    }
}

/// Injects keyboard events into the host OS.
pub struct KeyboardSink {
    _private: (),
}

impl KeyboardSink {
    pub fn new() -> flux_core::Result<Self> {
        tracing::debug!("Initializing keyboard input sink");
        // TODO:
        //   Windows: No init needed — uses SendInput() directly
        //   Linux:   Open /dev/uinput or use libei/inputtino
        Ok(Self { _private: () })
    }

    /// Inject a keyboard event into the host OS.
    pub fn inject(&self, event: &KeyboardEvent) -> flux_core::Result<()> {
        #[cfg(target_os = "windows")]
        return self.inject_windows(event);

        #[cfg(not(target_os = "windows"))]
        {
            tracing::trace!("Input injection not implemented for this OS: {:?}", event);
            Ok(())
        }
    }

    #[cfg(target_os = "windows")]
    fn inject_windows(&self, event: &KeyboardEvent) -> flux_core::Result<()> {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
            KEYEVENTF_SCANCODE, VIRTUAL_KEY,
        };

        let mut dw_flags = windows::Win32::UI::Input::KeyboardAndMouse::KEYBD_EVENT_FLAGS(0);
        let mut scan_code = 0;
        let mut vk_code = 0;

        match event {
            KeyboardEvent::KeyDown {
                scan_code: sc,
                key_code: kc,
                modifiers: _,
            } => {
                if *sc > 0 {
                    dw_flags |= KEYEVENTF_SCANCODE;
                    scan_code = *sc;
                } else if let Some(vk) = kc {
                    vk_code = *vk;
                }
            }
            KeyboardEvent::KeyUp {
                scan_code: sc,
                key_code: kc,
                modifiers: _,
            } => {
                dw_flags |= KEYEVENTF_KEYUP;
                if *sc > 0 {
                    dw_flags |= KEYEVENTF_SCANCODE;
                    scan_code = *sc;
                } else if let Some(vk) = kc {
                    vk_code = *vk;
                }
            }
        }

        if scan_code == 0 && vk_code == 0 {
            // Nothing to inject
            return Ok(());
        }

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk_code),
                    wScan: scan_code,
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
