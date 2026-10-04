//! Unified input sink that dispatches events to the appropriate device handler.

use std::sync::{Arc, RwLock};

use flux_core::error::Result;
use flux_core::types::DesktopRect;

use crate::backend::InputBackend;
use crate::events::InputEvent;
use crate::gamepad::GamepadSink;
use crate::keyboard::{KeyboardEvent, KeyboardSink};
use crate::keymap::scancode_to_evdev;
use crate::mouse::{MouseEvent, MouseSink};

/// Windows `WHEEL_DELTA`: the wire `Scroll` unit for one wheel notch.
const WHEEL_DELTA: f64 = 120.0;

/// Unified input sink that handles all input device types.
pub struct InputSink {
    keyboard: KeyboardSink,
    mouse: MouseSink,
    gamepad: GamepadSink,
    backend: RwLock<Option<Arc<dyn InputBackend>>>,
}

impl InputSink {
    /// Create a new input sink for the captured output rectangle.
    pub fn new(target_rect: DesktopRect) -> Result<Self> {
        Ok(Self {
            keyboard: KeyboardSink::new()?,
            mouse: MouseSink::new(target_rect)?,
            gamepad: GamepadSink::new()?,
            backend: RwLock::new(None),
        })
    }

    /// Update the output receiving absolute input after a display/topology change.
    pub fn set_target_rect(&self, target_rect: DesktopRect) -> Result<()> {
        self.mouse.set_target_rect(target_rect)
    }

    /// Route keyboard and mouse events through `backend` instead of the
    /// platform sinks. `None` restores the default behavior.
    pub fn set_backend(&self, backend: Option<Arc<dyn InputBackend>>) {
        *self.backend.write().unwrap_or_else(|e| e.into_inner()) = backend;
    }

    /// Dispatch an input event to the correct device handler.
    pub fn handle_event(&self, event: &InputEvent) -> Result<()> {
        let backend = self.backend.read().unwrap_or_else(|e| e.into_inner()).clone();
        match (event, backend) {
            (InputEvent::Keyboard(e), Some(backend)) => dispatch_keyboard(backend.as_ref(), e),
            (InputEvent::Mouse(e), Some(backend)) => dispatch_mouse(backend.as_ref(), e),
            (InputEvent::Keyboard(e), None) => self.keyboard.inject(e),
            (InputEvent::Mouse(e), None) => self.mouse.inject(e),
            (InputEvent::Gamepad(e), _) => self.gamepad.inject(e),
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

fn dispatch_keyboard(backend: &dyn InputBackend, event: &KeyboardEvent) -> Result<()> {
    let (scan_code, down) = match event {
        KeyboardEvent::KeyDown { scan_code, .. } => (*scan_code, true),
        KeyboardEvent::KeyUp { scan_code, .. } => (*scan_code, false),
    };
    match scancode_to_evdev(scan_code) {
        Some(evdev) => backend.key(evdev, down),
        None => {
            tracing::trace!("dropping key with unmapped scancode {scan_code:#x}");
            Ok(())
        }
    }
}

fn dispatch_mouse(backend: &dyn InputBackend, event: &MouseEvent) -> Result<()> {
    match event {
        MouseEvent::Move { dx, dy } => backend.pointer_motion(*dx as f64, *dy as f64),
        MouseEvent::MoveAbsolute { x, y } => backend.pointer_absolute(*x as f64, *y as f64),
        MouseEvent::ButtonDown { button } => backend.pointer_button(*button, true),
        MouseEvent::ButtonUp { button } => backend.pointer_button(*button, false),
        // Wire scroll is in WHEEL_DELTA units with positive = up/right; the
        // backend contract is notches with positive = down/right.
        MouseEvent::Scroll { dx, dy } => backend.pointer_axis(*dx as f64 / WHEEL_DELTA, -(*dy as f64) / WHEEL_DELTA),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mouse::MouseButton;
    use std::sync::Mutex;

    #[derive(Debug, PartialEq)]
    enum Call {
        Motion(f64, f64),
        Absolute(f64, f64),
        Button(MouseButton, bool),
        Axis(f64, f64),
        Key(u32, bool),
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Call>>);

    impl Recorder {
        fn take(&self) -> Vec<Call> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    impl InputBackend for Recorder {
        fn name(&self) -> &'static str {
            "recorder"
        }
        fn supports_absolute(&self) -> bool {
            true
        }
        fn pointer_motion(&self, dx: f64, dy: f64) -> Result<()> {
            self.0.lock().unwrap().push(Call::Motion(dx, dy));
            Ok(())
        }
        fn pointer_absolute(&self, x: f64, y: f64) -> Result<()> {
            self.0.lock().unwrap().push(Call::Absolute(x, y));
            Ok(())
        }
        fn pointer_button(&self, button: MouseButton, down: bool) -> Result<()> {
            self.0.lock().unwrap().push(Call::Button(button, down));
            Ok(())
        }
        fn pointer_axis(&self, dx: f64, dy: f64) -> Result<()> {
            self.0.lock().unwrap().push(Call::Axis(dx, dy));
            Ok(())
        }
        fn key(&self, evdev_code: u32, down: bool) -> Result<()> {
            self.0.lock().unwrap().push(Call::Key(evdev_code, down));
            Ok(())
        }
    }

    fn sink_with_recorder() -> (InputSink, Arc<Recorder>) {
        let sink = InputSink::new(DesktopRect {
            left: 0,
            top: 0,
            width: 1920,
            height: 1080,
        })
        .unwrap();
        let recorder = Arc::new(Recorder::default());
        sink.set_backend(Some(recorder.clone()));
        (sink, recorder)
    }

    fn key_down(scan_code: u16) -> InputEvent {
        InputEvent::Keyboard(KeyboardEvent::KeyDown {
            scan_code,
            key_code: None,
            modifiers: 0,
        })
    }

    fn key_up(scan_code: u16) -> InputEvent {
        InputEvent::Keyboard(KeyboardEvent::KeyUp {
            scan_code,
            key_code: None,
            modifiers: 0,
        })
    }

    #[test]
    fn keys_translate_scancodes_to_evdev() {
        let (sink, rec) = sink_with_recorder();
        sink.handle_event(&key_down(0x1E)).unwrap(); // A
        sink.handle_event(&key_up(0x1E)).unwrap();
        sink.handle_event(&key_down(0xE04B)).unwrap(); // extended: Left arrow
        assert_eq!(
            rec.take(),
            vec![Call::Key(30, true), Call::Key(30, false), Call::Key(105, true)]
        );
    }

    #[test]
    fn unmapped_scancodes_are_dropped() {
        let (sink, rec) = sink_with_recorder();
        sink.handle_event(&key_down(0x7FFF)).unwrap();
        assert!(rec.take().is_empty());
    }

    #[test]
    fn mouse_motion_and_buttons_route_to_backend() {
        let (sink, rec) = sink_with_recorder();
        sink.handle_event(&InputEvent::Mouse(MouseEvent::Move { dx: 3, dy: -4 })).unwrap();
        sink.handle_event(&InputEvent::Mouse(MouseEvent::MoveAbsolute { x: 0.25, y: 0.5 }))
            .unwrap();
        sink.handle_event(&InputEvent::Mouse(MouseEvent::ButtonDown {
            button: MouseButton::Right,
        }))
        .unwrap();
        sink.handle_event(&InputEvent::Mouse(MouseEvent::ButtonUp {
            button: MouseButton::Right,
        }))
        .unwrap();
        assert_eq!(
            rec.take(),
            vec![
                Call::Motion(3.0, -4.0),
                Call::Absolute(0.25, 0.5),
                Call::Button(MouseButton::Right, true),
                Call::Button(MouseButton::Right, false),
            ]
        );
    }

    #[test]
    fn scroll_converts_wheel_delta_to_notches_with_wayland_sign() {
        let (sink, rec) = sink_with_recorder();
        // Wheel up one notch (+120) must become a negative (upward) axis value.
        sink.handle_event(&InputEvent::Mouse(MouseEvent::Scroll { dx: 0, dy: 120 }))
            .unwrap();
        sink.handle_event(&InputEvent::Mouse(MouseEvent::Scroll { dx: 240, dy: -120 }))
            .unwrap();
        assert_eq!(rec.take(), vec![Call::Axis(0.0, -1.0), Call::Axis(2.0, 1.0)]);
    }

    #[test]
    fn clearing_the_backend_stops_routing() {
        let (sink, rec) = sink_with_recorder();
        sink.set_backend(None);
        sink.handle_event(&key_down(0x1E)).unwrap();
        assert!(rec.take().is_empty());
    }
}
