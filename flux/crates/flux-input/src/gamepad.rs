//! Gamepad / controller input events and virtual gamepad injection.

use serde::{Deserialize, Serialize};

/// A gamepad event from the remote client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GamepadEvent {
    /// A digital button was pressed or released.
    Button {
        gamepad_id: u8,
        button: GamepadButton,
        pressed: bool,
    },

    /// An analog axis value changed.
    Axis {
        gamepad_id: u8,
        axis: GamepadAxis,
        /// Normalized value: -1.0 to 1.0 for sticks, 0.0 to 1.0 for triggers.
        value: f32,
    },

    /// A gamepad was connected.
    Connected { gamepad_id: u8 },

    /// A gamepad was disconnected.
    Disconnected { gamepad_id: u8 },
}

/// Standard gamepad buttons (Xbox-style layout).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GamepadButton {
    A,
    B,
    X,
    Y,
    LeftBumper,
    RightBumper,
    Back,
    Start,
    Guide,
    LeftStick,
    RightStick,
    DPadUp,
    DPadDown,
    DPadLeft,
    DPadRight,
}

/// Standard gamepad axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GamepadAxis {
    LeftStickX,
    LeftStickY,
    RightStickX,
    RightStickY,
    LeftTrigger,
    RightTrigger,
}

/// Injects gamepad events into the host OS as a virtual controller.
pub struct GamepadSink {
    // TODO:
    //   Windows: ViGEmBus client (virtual Xbox 360 / DS4 controller)
    //   Linux:   uinput virtual gamepad device
    _private: (),
}

impl GamepadSink {
    pub fn new() -> flux_core::Result<Self> {
        tracing::debug!("Initializing gamepad input sink");

        // TODO:
        //   Windows:
        //     1. Load ViGEmClient.dll
        //     2. vigem_connect(client)
        //     3. vigem_target_x360_alloc() → create virtual Xbox 360 pad
        //     4. vigem_target_add(client, target)
        //
        //   Linux:
        //     1. Open /dev/uinput
        //     2. ioctl UI_SET_EVBIT (EV_KEY, EV_ABS)
        //     3. ioctl UI_SET_KEYBIT for all gamepad buttons (BTN_A, BTN_B, etc.)
        //     4. ioctl UI_SET_ABSBIT for all axes (ABS_X, ABS_Y, etc.)
        //     5. Write uinput_user_dev with absmin/absmax
        //     6. ioctl UI_DEV_CREATE

        Ok(Self { _private: () })
    }

    /// Inject a gamepad event.
    pub fn inject(&self, event: &GamepadEvent) -> flux_core::Result<()> {
        match event {
            GamepadEvent::Button { gamepad_id, button, pressed } => {
                tracing::trace!(
                    "Gamepad {} button {:?} {}",
                    gamepad_id,
                    button,
                    if *pressed { "down" } else { "up" }
                );
                // TODO:
                //   Windows: vigem_target_x360_update with XUSB_REPORT
                //   Linux:   write EV_KEY event
            }
            GamepadEvent::Axis { gamepad_id, axis, value } => {
                tracing::trace!("Gamepad {} axis {:?} = {:.3}", gamepad_id, axis, value);
                // TODO:
                //   Windows: vigem_target_x360_update with axis values
                //   Linux:   write EV_ABS event
            }
            GamepadEvent::Connected { gamepad_id } => {
                tracing::info!("Virtual gamepad {} connected", gamepad_id);
            }
            GamepadEvent::Disconnected { gamepad_id } => {
                tracing::info!("Virtual gamepad {} disconnected", gamepad_id);
            }
        }
        Ok(())
    }
}
