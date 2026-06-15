//! Input event types transmitted from client to server.

use serde::{Deserialize, Serialize};

use crate::gamepad::GamepadEvent;
use crate::keyboard::KeyboardEvent;
use crate::mouse::MouseEvent;

/// A single input event from the remote client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InputEvent {
    Keyboard(KeyboardEvent),
    Mouse(MouseEvent),
    Gamepad(GamepadEvent),
}

impl InputEvent {
    /// Returns the event category for QoS prioritization.
    pub fn priority(&self) -> EventPriority {
        match self {
            // Mouse movement is high-frequency, low-latency critical.
            Self::Mouse(MouseEvent::Move { .. }) => EventPriority::High,
            // Key presses/releases need reliable delivery.
            Self::Keyboard(_) => EventPriority::Normal,
            // Mouse clicks need reliable delivery.
            Self::Mouse(_) => EventPriority::Normal,
            // Gamepad is high-frequency but tolerates some loss.
            Self::Gamepad(_) => EventPriority::Normal,
        }
    }
}

/// Priority level for input event delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPriority {
    High,
    Normal,
    Low,
}
