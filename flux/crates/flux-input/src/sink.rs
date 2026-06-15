//! Unified input sink that dispatches events to the appropriate device handler.

use flux_core::error::Result;

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
    /// Create a new input sink for the given screen resolution.
    pub fn new(screen_width: u32, screen_height: u32) -> Result<Self> {
        Ok(Self {
            keyboard: KeyboardSink::new()?,
            mouse: MouseSink::new(screen_width, screen_height)?,
            gamepad: GamepadSink::new()?,
        })
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
