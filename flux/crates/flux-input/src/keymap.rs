//! Scancode → Linux evdev keycode translation.
//!
//! The wire protocol carries platform-independent **PC/AT set-1** scancodes
//! (the same convention Windows `KEYEVENTF_SCANCODE` uses). Linux evdev
//! keycodes (`KEY_*` in `linux/input-event-codes.h`) for the main keyboard
//! block are, by construction, identical to the set-1 *make* codes for codes
//! `0x01..=0x58`, so those pass through directly. Extended (`0xE0`-prefixed)
//! keys are remapped explicitly.
//!
//! Injecting an evdev *keycode* (rather than a keysym) lets the compositor's
//! own XKB keymap resolve the symbol, which is what we want for correct
//! host-layout behavior.

/// Largest set-1 scancode that maps 1:1 onto a Linux evdev keycode.
const MAX_DIRECT_SCANCODE: u16 = 0x58;

/// Translate a set-1 scancode to a Linux evdev keycode.
///
/// Returns `None` for scancodes with no known mapping.
pub fn scancode_to_evdev(scancode: u16) -> Option<u32> {
    // Extended keys are sent as `0xE0` followed by the base code; callers may
    // encode that as `0xE000 | base`.
    if scancode & 0xFF00 == 0xE000 {
        return extended_to_evdev(scancode & 0x00FF);
    }
    if (0x01..=MAX_DIRECT_SCANCODE).contains(&scancode) {
        return Some(scancode as u32);
    }
    None
}

/// Map the base byte of an `0xE0`-prefixed extended scancode.
fn extended_to_evdev(base: u16) -> Option<u32> {
    // Values from linux/input-event-codes.h.
    let code = match base {
        0x1C => 96,  // KEY_KPENTER
        0x1D => 97,  // KEY_RIGHTCTRL
        0x38 => 100, // KEY_RIGHTALT
        0x47 => 102, // KEY_HOME
        0x48 => 103, // KEY_UP
        0x49 => 104, // KEY_PAGEUP
        0x4B => 105, // KEY_LEFT
        0x4D => 106, // KEY_RIGHT
        0x4F => 107, // KEY_END
        0x50 => 108, // KEY_DOWN
        0x51 => 109, // KEY_PAGEDOWN
        0x52 => 110, // KEY_INSERT
        0x53 => 111, // KEY_DELETE
        0x5B => 125, // KEY_LEFTMETA
        0x5C => 126, // KEY_RIGHTMETA
        _ => return None,
    };
    Some(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_block_passthrough() {
        assert_eq!(scancode_to_evdev(0x01), Some(1)); // KEY_ESC
        assert_eq!(scancode_to_evdev(0x1E), Some(30)); // KEY_A
        assert_eq!(scancode_to_evdev(0x1F), Some(31)); // KEY_S
        assert_eq!(scancode_to_evdev(0x1C), Some(28)); // KEY_ENTER
        assert_eq!(scancode_to_evdev(0x39), Some(57)); // KEY_SPACE
        assert_eq!(scancode_to_evdev(0x2A), Some(42)); // KEY_LEFTSHIFT
    }

    #[test]
    fn extended_keys_remapped() {
        assert_eq!(scancode_to_evdev(0xE048), Some(103)); // Up arrow
        assert_eq!(scancode_to_evdev(0xE04D), Some(106)); // Right arrow
        assert_eq!(scancode_to_evdev(0xE05B), Some(125)); // Left Meta/Super
    }

    #[test]
    fn unknown_scancodes_are_none() {
        assert_eq!(scancode_to_evdev(0x00), None);
        assert_eq!(scancode_to_evdev(0xFFFF), None);
        assert_eq!(scancode_to_evdev(0xE000), None);
    }
}
