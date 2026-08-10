//! Unified input sink that dispatches events to the appropriate device handler.

use flux_core::error::Result;
use flux_core::types::DesktopRect;

use crate::events::InputEvent;
use crate::gamepad::GamepadSink;
use crate::keyboard::KeyboardSink;
use crate::mouse::MouseSink;

/// Unified input sink that handles all input device types.
pub struct InputSink {
    keyboard: KeyboardSink,
    mouse: MouseSink,
    gamepad: GamepadSink,
}

impl InputSink {
    /// Create a new input sink for the captured output rectangle.
    pub fn new(target_rect: DesktopRect) -> Result<Self> {
        Ok(Self {
            keyboard: KeyboardSink::new()?,
            mouse: MouseSink::new(target_rect)?,
            gamepad: GamepadSink::new()?,
        })
    }

    /// Update the output receiving absolute input after a display/topology change.
    pub fn set_target_rect(&self, target_rect: DesktopRect) -> Result<()> {
        self.mouse.set_target_rect(target_rect)
    }

    /// Dispatch an input event to the correct device handler.
    pub fn handle_event(&self, event: &InputEvent) -> Result<()> {
        match event {
            InputEvent::Keyboard(e) => self.keyboard.inject(e),
            InputEvent::Mouse(e) => self.mouse.inject(e),
            InputEvent::Gamepad(e) => self.gamepad.inject(e),
        }
    }

    /// Process a batch of input events.
    pub fn handle_events(&self, events: &[InputEvent]) -> Result<()> {
        for event in events {
            self.handle_event(event)?;
        }
        Ok(())
    }
}
