//! Real Wayland input injection via `xdg-desktop-portal` `RemoteDesktop`.
//!
//! [`PortalInputBackend`] implements [`InputBackend`] by driving the
//! `org.freedesktop.portal.RemoteDesktop` D-Bus interface through [`ashpd`].
//! Construction negotiates a session (`CreateSession` → `SelectDevices` →
//! `Start`, which raises the compositor's consent dialog) and then forwards
//! pointer/keyboard events with the `NotifyPointer*` / `NotifyKeyboard*` calls.
//!
//! The ashpd proxies are async and own a `zbus` connection, so — exactly like
//! the PipeWire capture loop — the session runs on a dedicated thread with its
//! own current-thread Tokio runtime. The public backend talks to it over a
//! channel, so the hot `pointer_motion`/`key` calls never block on D-Bus: they
//! enqueue a command and return immediately while the thread awaits delivery.
//!
//! Scope (matches the roadmap): relative pointer motion, scroll, buttons and
//! keyboard via evdev keycodes. *Absolute* pointer positioning needs an
//! associated `ScreenCast` stream node (so the compositor can map normalized
//! coordinates onto an output); a `RemoteDesktop`-only session has none, so
//! [`InputBackend::supports_absolute`] reports `false` here and absolute
//! events are dropped. Sharing the capture session's stream for absolute input
//! is a later integration step.

use std::thread::{self, JoinHandle};

use ashpd::desktop::remote_desktop::{DeviceType, KeyState, RemoteDesktop};
use ashpd::desktop::{PersistMode, Session};
use ashpd::WindowIdentifier;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use flux_core::error::{FluxError, Result};

use crate::backend::InputBackend;
use crate::mouse::MouseButton;

// Linux evdev pointer button codes (`BTN_*` in `linux/input-event-codes.h`);
// the portal's `NotifyPointerButton` expects these.
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;
const BTN_SIDE: i32 = 0x113;
const BTN_EXTRA: i32 = 0x114;

/// A command forwarded from [`PortalInputBackend`] to the session thread.
enum Cmd {
    Motion { dx: f64, dy: f64 },
    Button { button: i32, down: bool },
    Axis { dx: f64, dy: f64 },
    Key { keycode: i32, down: bool },
}

/// Wayland input-injection backend backed by the RemoteDesktop portal.
pub struct PortalInputBackend {
    /// `Option` so [`Drop`] can close the channel (ending the session loop)
    /// before joining the thread.
    tx: Option<UnboundedSender<Cmd>>,
    handle: Option<JoinHandle<()>>,
    supports_absolute: bool,
}

impl PortalInputBackend {
    /// Negotiate a RemoteDesktop session and start the injection thread.
    ///
    /// Blocks until the portal handshake completes (including the user consent
    /// dialog) so negotiation failures surface synchronously.
    pub fn new() -> Result<Self> {
        let (tx, rx) = unbounded_channel::<Cmd>();
        // Surface negotiation result synchronously; carries `supports_absolute`.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<bool>>();

        let handle = thread::Builder::new()
            .name("flux-portal-input".into())
            .spawn(move || run_session_thread(rx, ready_tx))
            .map_err(|e| FluxError::Input(format!("failed to spawn input thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(supports_absolute)) => Ok(Self {
                tx: Some(tx),
                handle: Some(handle),
                supports_absolute,
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(FluxError::Input(
                    "input thread exited before signalling readiness".into(),
                ))
            }
        }
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or_else(|| FluxError::Input("portal input backend is shutting down".into()))?
            .send(cmd)
            .map_err(|_| FluxError::Input("portal input thread is gone".into()))
    }
}

impl InputBackend for PortalInputBackend {
    fn name(&self) -> &'static str {
        "portal-remotedesktop"
    }

    fn supports_absolute(&self) -> bool {
        self.supports_absolute
    }

    fn pointer_motion(&self, dx: f64, dy: f64) -> Result<()> {
        self.send(Cmd::Motion { dx, dy })
    }

    fn pointer_absolute(&self, x: f64, y: f64) -> Result<()> {
        // A RemoteDesktop-only session has no ScreenCast stream to map onto, so
        // the portal cannot place the pointer in absolute screen space. Drop
        // rather than error on the hot path; callers should consult
        // `supports_absolute()` and fall back to relative motion.
        tracing::trace!("portal backend has no stream for absolute motion; dropping ({x}, {y})");
        Ok(())
    }

    fn pointer_button(&self, button: MouseButton, down: bool) -> Result<()> {
        self.send(Cmd::Button {
            button: evdev_button(button),
            down,
        })
    }

    fn pointer_axis(&self, dx: f64, dy: f64) -> Result<()> {
        self.send(Cmd::Axis { dx, dy })
    }

    fn key(&self, evdev_code: u32, down: bool) -> Result<()> {
        self.send(Cmd::Key {
            keycode: evdev_code as i32,
            down,
        })
    }
}

impl Drop for PortalInputBackend {
    fn drop(&mut self) {
        // Dropping the sender closes the channel, which ends the session loop
        // and lets the thread close the portal session and exit.
        self.tx.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Map a [`MouseButton`] to its Linux evdev button code.
fn evdev_button(button: MouseButton) -> i32 {
    match button {
        MouseButton::Left => BTN_LEFT,
        MouseButton::Right => BTN_RIGHT,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => BTN_SIDE,
        MouseButton::Forward => BTN_EXTRA,
    }
}

/// Map a press/release flag to the portal's [`KeyState`].
fn key_state(down: bool) -> KeyState {
    if down {
        KeyState::Pressed
    } else {
        KeyState::Released
    }
}

/// Body of the dedicated session thread: build a runtime, negotiate the portal
/// session, then forward commands until the channel closes.
fn run_session_thread(rx: UnboundedReceiver<Cmd>, ready_tx: std::sync::mpsc::Sender<Result<bool>>) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(FluxError::Input(format!("failed to build input runtime: {e}"))));
            return;
        }
    };

    runtime.block_on(async move {
        let (remote_desktop, session) = match negotiate().await {
            Ok(pair) => pair,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };

        // Relative-only: no associated ScreenCast stream for absolute mapping.
        let _ = ready_tx.send(Ok(false));

        forward_commands(&remote_desktop, &session, rx).await;
        let _ = session.close().await;
    });
}

/// Run `CreateSession` → `SelectDevices` → `Start` for keyboard + pointer.
async fn negotiate() -> Result<(RemoteDesktop<'static>, Session<'static, RemoteDesktop<'static>>)> {
    let remote_desktop = RemoteDesktop::new().await.map_err(input_err)?;
    let session = remote_desktop.create_session().await.map_err(input_err)?;

    remote_desktop
        .select_devices(
            &session,
            DeviceType::Keyboard | DeviceType::Pointer,
            None,
            PersistMode::DoNot,
        )
        .await
        .map_err(input_err)?;

    remote_desktop
        .start(&session, &WindowIdentifier::default())
        .await
        .map_err(input_err)?
        .response()
        .map_err(input_err)?;

    Ok((remote_desktop, session))
}

/// Drain the command channel, issuing the matching portal notify call for each.
async fn forward_commands(
    remote_desktop: &RemoteDesktop<'_>,
    session: &Session<'_, RemoteDesktop<'_>>,
    mut rx: UnboundedReceiver<Cmd>,
) {
    while let Some(cmd) = rx.recv().await {
        let result = match cmd {
            Cmd::Motion { dx, dy } => remote_desktop.notify_pointer_motion(session, dx, dy).await,
            Cmd::Button { button, down } => {
                remote_desktop
                    .notify_pointer_button(session, button, key_state(down))
                    .await
            }
            // The portal spec expects the deltas to be 0 when `finish` is set,
            // so deliver the motion with `finish = false` and then a terminating
            // `(0, 0, finish = true)` to close the scroll sequence.
            Cmd::Axis { dx, dy } => match remote_desktop.notify_pointer_axis(session, dx, dy, false).await {
                Ok(()) => remote_desktop.notify_pointer_axis(session, 0.0, 0.0, true).await,
                Err(e) => Err(e),
            },
            Cmd::Key { keycode, down } => {
                remote_desktop
                    .notify_keyboard_keycode(session, keycode, key_state(down))
                    .await
            }
        };
        if let Err(e) = result {
            tracing::warn!("portal input notify failed: {e}");
        }
    }
}

fn input_err(e: ashpd::Error) -> FluxError {
    FluxError::Input(format!("xdg-desktop-portal RemoteDesktop: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_buttons_map_to_evdev_codes() {
        assert_eq!(evdev_button(MouseButton::Left), 0x110);
        assert_eq!(evdev_button(MouseButton::Right), 0x111);
        assert_eq!(evdev_button(MouseButton::Middle), 0x112);
        assert_eq!(evdev_button(MouseButton::Back), 0x113);
        assert_eq!(evdev_button(MouseButton::Forward), 0x114);
    }

    #[test]
    fn key_state_maps_press_and_release() {
        assert_eq!(key_state(true), KeyState::Pressed);
        assert_eq!(key_state(false), KeyState::Released);
    }
}
