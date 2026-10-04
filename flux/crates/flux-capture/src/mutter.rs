//! Direct Mutter ScreenCast + RemoteDesktop session (no portal, no consent
//! dialog) — the same private D-Bus API `gnome-remote-desktop` uses.
//!
//! A single dedicated thread owns a current-thread Tokio runtime and the zbus
//! session connection. [`MutterSession::start`] performs the handshake
//! (RemoteDesktop `CreateSession` → ScreenCast `CreateSession` linked by
//! `remote-desktop-session-id` → `RecordMonitor` → `Start`) and reports the
//! PipeWire node id; afterwards the thread forwards input commands received
//! over a channel so the hot path never blocks on D-Bus.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use flux_core::error::{FluxError, Result};
use flux_core::types::{DesktopRect, Resolution};
use flux_input::{InputBackend, mouse::MouseButton};
use futures::StreamExt;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::{Connection, Proxy};

use crate::session::CursorMode;

const RD_NAME: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const RD_SESSION_IFACE: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const SC_NAME: &str = "org.gnome.Mutter.ScreenCast";
const SC_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SC_SESSION_IFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM_IFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";
const DC_NAME: &str = "org.gnome.Mutter.DisplayConfig";
const DC_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

const STREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Mutter `cursor-mode` option value: 0 hidden, 1 embedded, 2 metadata.
pub fn cursor_mode_value(mode: CursorMode) -> u32 {
    match mode {
        CursorMode::Hidden => 0,
        CursorMode::Embedded => 1,
        CursorMode::Metadata => 2,
    }
}

/// What the ScreenCast session records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutterSource {
    /// A virtual monitor Rustcast owns (`RecordVirtual`); its mode follows the
    /// size negotiated on the PipeWire stream.
    Virtual,
    /// The primary physical/existing monitor (`RecordMonitor`).
    PrimaryMonitor,
}

impl MutterSource {
    /// `FLUX_MUTTER_SOURCE=monitor` selects the primary monitor; anything else
    /// (including unset) the virtual monitor.
    pub fn from_env() -> Self {
        Self::from_setting(std::env::var("FLUX_MUTTER_SOURCE").ok().as_deref())
    }

    fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("monitor") => Self::PrimaryMonitor,
            _ => Self::Virtual,
        }
    }
}

/// Properties passed to `RecordVirtual` / `RecordMonitor`.
fn record_options(source: MutterSource, cursor_mode: CursorMode) -> HashMap<&'static str, Value<'static>> {
    let mut options: HashMap<&'static str, Value<'static>> = HashMap::new();
    options.insert("cursor-mode", Value::from(cursor_mode_value(cursor_mode)));
    if source == MutterSource::Virtual {
        options.insert("is-platform", Value::from(true));
    }
    options
}

/// Map normalized `0..=1` coordinates onto stream pixels.
pub fn normalized_to_pixels(x: f64, y: f64, width: u32, height: u32) -> (f64, f64) {
    let clamp = |v: f64| if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.0 };
    (
        clamp(x) * width.saturating_sub(1) as f64,
        clamp(y) * height.saturating_sub(1) as f64,
    )
}

/// Convert a fractional notch delta into whole discrete steps, never rounding
/// a non-zero scroll down to nothing.
pub fn axis_steps(notches: f64) -> i32 {
    if !notches.is_finite() || notches == 0.0 {
        return 0;
    }
    let rounded = notches.round() as i32;
    if rounded == 0 {
        notches.signum() as i32
    } else {
        rounded
    }
}

/// Whether Mutter's RemoteDesktop service is reachable on the session bus.
pub fn is_available() -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return false;
    };
    rt.block_on(async {
        let Ok(conn) = Connection::session().await else {
            return false;
        };
        let Ok(dbus) = zbus::fdo::DBusProxy::new(&conn).await else {
            return false;
        };
        let Ok(name) = zbus::names::BusName::try_from(RD_NAME) else {
            return false;
        };
        dbus.name_has_owner(name).await.unwrap_or(false)
    })
}

/// Geometry of a monitor as reported by `DisplayConfig.GetCurrentState`.
#[derive(Debug, Clone, PartialEq)]
pub struct MonitorGeometry {
    pub connector: String,
    pub rect: DesktopRect,
    pub resolution: Resolution,
}

type Mode = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type MonitorId = (String, String, String, String);
type Monitor = (MonitorId, Vec<Mode>, HashMap<String, OwnedValue>);
type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<MonitorId>, HashMap<String, OwnedValue>);
type DisplayState = (u32, Vec<Monitor>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>);

fn is_true(value: Option<&OwnedValue>) -> bool {
    matches!(value.map(|v| &**v), Some(Value::Bool(true)))
}

/// Pick the primary logical monitor's geometry out of a `GetCurrentState` reply.
fn primary_monitor(state: &DisplayState) -> Option<MonitorGeometry> {
    let (_, monitors, logical, _) = state;
    let lm = logical.iter().find(|l| l.4).or_else(|| logical.first())?;
    let connector = lm.5.first()?.0.clone();
    let monitor = monitors.iter().find(|m| m.0 .0 == connector)?;
    let mode = monitor
        .1
        .iter()
        .find(|m| is_true(m.6.get("is-current")))
        .or_else(|| monitor.1.first())?;
    let (width, height) = (mode.1.max(0) as u32, mode.2.max(0) as u32);
    Some(MonitorGeometry {
        connector,
        rect: DesktopRect {
            left: lm.0,
            top: lm.1,
            width,
            height,
        },
        resolution: Resolution::new(width, height),
    })
}

/// Query the primary monitor's real size and position from Mutter.
pub fn query_primary_monitor() -> Result<MonitorGeometry> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| FluxError::Capture(format!("failed to build runtime: {e}")))?;
    rt.block_on(async {
        let conn = Connection::session().await.map_err(cap_err)?;
        let proxy = Proxy::new(&conn, DC_NAME, DC_PATH, DC_NAME).await.map_err(cap_err)?;
        let state: DisplayState = proxy.call("GetCurrentState", &()).await.map_err(cap_err)?;
        primary_monitor(&state).ok_or_else(|| FluxError::Capture("Mutter reported no monitors".into()))
    })
}

fn cap_err(e: zbus::Error) -> FluxError {
    FluxError::Capture(format!("Mutter D-Bus: {e}"))
}

enum Cmd {
    Motion { dx: f64, dy: f64 },
    Absolute { x: f64, y: f64 },
    Button { button: i32, down: bool },
    Axis { axis: u32, steps: i32 },
    Key { keycode: u32, down: bool },
    Shutdown,
}

/// Stream size shared with the input backend, updated by the capture side from
/// the PipeWire-negotiated format.
#[derive(Debug)]
pub struct StreamSize {
    width: AtomicU32,
    height: AtomicU32,
}

impl StreamSize {
    fn new(resolution: Resolution) -> Self {
        Self {
            width: AtomicU32::new(resolution.width),
            height: AtomicU32::new(resolution.height),
        }
    }

    pub fn set(&self, resolution: Resolution) {
        self.width.store(resolution.width, Ordering::Relaxed);
        self.height.store(resolution.height, Ordering::Relaxed);
    }

    fn get(&self) -> (u32, u32) {
        (self.width.load(Ordering::Relaxed), self.height.load(Ordering::Relaxed))
    }
}

/// A live Mutter ScreenCast+RemoteDesktop session.
pub struct MutterSession {
    source: MutterSource,
    node_id: u32,
    tx: UnboundedSender<Cmd>,
    size: Arc<StreamSize>,
    closed: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MutterSession {
    /// Create and start a session recording `source`.
    ///
    /// `initial_size` seeds absolute-pointer scaling until the PipeWire stream
    /// reports its real size via [`Self::set_stream_size`].
    pub fn start(source: MutterSource, cursor_mode: CursorMode, initial_size: Resolution) -> Result<Self> {
        let (tx, rx) = unbounded_channel::<Cmd>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(u32, String)>>();
        let closed = Arc::new(AtomicBool::new(false));
        let thread_closed = Arc::clone(&closed);
        let handle = std::thread::Builder::new()
            .name("flux-mutter".into())
            .spawn(move || run_thread(source, cursor_mode, rx, ready_tx, thread_closed))
            .map_err(|e| FluxError::Capture(format!("failed to spawn Mutter thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok((node_id, stream_path))) => {
                tracing::info!(
                    "Mutter session created ({}): PipeWire node {node_id}, stream {stream_path}",
                    match source {
                        MutterSource::Virtual => "RecordVirtual",
                        MutterSource::PrimaryMonitor => "RecordMonitor",
                    }
                );
                Ok(Self {
                    source,
                    node_id,
                    tx,
                    size: Arc::new(StreamSize::new(initial_size)),
                    closed,
                    handle: Some(handle),
                })
            }
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(FluxError::Capture("Mutter thread exited before signalling readiness".into()))
            }
        }
    }

    pub fn source(&self) -> MutterSource {
        self.source
    }

    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    pub fn set_stream_size(&self, resolution: Resolution) {
        self.size.set(resolution);
    }

    /// Whether Mutter closed the session (e.g. the shell's stop-sharing
    /// indicator) or it otherwise disappeared from the bus.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Input backend that injects through this session's RemoteDesktop half.
    pub fn input_backend(&self) -> Arc<dyn InputBackend> {
        Arc::new(MutterInput {
            tx: self.tx.clone(),
            size: Arc::clone(&self.size),
            closed: Arc::clone(&self.closed),
        })
    }
}

impl Drop for MutterSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct MutterInput {
    tx: UnboundedSender<Cmd>,
    size: Arc<StreamSize>,
    closed: Arc<AtomicBool>,
}

impl MutterInput {
    fn send(&self, cmd: Cmd) -> Result<()> {
        if self.closed.load(Ordering::Acquire) {
            tracing::trace!("dropping input: Mutter session closed");
            return Ok(());
        }
        self.tx
            .send(cmd)
            .map_err(|_| FluxError::Input("Mutter session is gone".into()))
    }
}

impl InputBackend for MutterInput {
    fn name(&self) -> &'static str {
        "mutter-remotedesktop"
    }

    fn supports_absolute(&self) -> bool {
        true
    }

    fn pointer_motion(&self, dx: f64, dy: f64) -> Result<()> {
        self.send(Cmd::Motion { dx, dy })
    }

    fn pointer_absolute(&self, x: f64, y: f64) -> Result<()> {
        let (w, h) = self.size.get();
        let (x, y) = normalized_to_pixels(x, y, w, h);
        self.send(Cmd::Absolute { x, y })
    }

    fn pointer_button(&self, button: MouseButton, down: bool) -> Result<()> {
        self.send(Cmd::Button {
            button: button.evdev_code() as i32,
            down,
        })
    }

    fn pointer_axis(&self, dx: f64, dy: f64) -> Result<()> {
        let (h, v) = (axis_steps(dx), axis_steps(dy));
        if v != 0 {
            self.send(Cmd::Axis { axis: 0, steps: v })?;
        }
        if h != 0 {
            self.send(Cmd::Axis { axis: 1, steps: h })?;
        }
        Ok(())
    }

    fn key(&self, evdev_code: u32, down: bool) -> Result<()> {
        self.send(Cmd::Key {
            keycode: evdev_code,
            down,
        })
    }
}

type ClosedStream = zbus::proxy::SignalStream<'static>;

struct Handshake {
    conn: Connection,
    rd_session: Proxy<'static>,
    stream_path: OwnedObjectPath,
    node_id: u32,
    rd_closed: ClosedStream,
    sc_closed: ClosedStream,
}

fn run_thread(
    source: MutterSource,
    cursor_mode: CursorMode,
    rx: UnboundedReceiver<Cmd>,
    ready_tx: std::sync::mpsc::Sender<Result<(u32, String)>>,
    closed: Arc<AtomicBool>,
) {
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            let _ = ready_tx.send(Err(FluxError::Capture(format!("failed to build Mutter runtime: {e}"))));
            return;
        }
    };
    runtime.block_on(async move {
        let Handshake {
            conn,
            rd_session,
            stream_path,
            node_id,
            mut rd_closed,
            mut sc_closed,
        } = match handshake(source, cursor_mode).await {
            Ok(v) => v,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                return;
            }
        };
        let _ = ready_tx.send(Ok((node_id, stream_path.to_string())));
        forward_commands(&rd_session, stream_path.as_str(), rx, &mut rd_closed, &mut sc_closed, &closed).await;
        if !closed.load(Ordering::Acquire)
            && let Err(e) = rd_session.call::<_, _, ()>("Stop", &()).await
        {
            tracing::debug!("Mutter RemoteDesktop Stop failed: {e}");
        }
        closed.store(true, Ordering::Release);
        drop(conn);
    });
}

async fn handshake(source: MutterSource, cursor_mode: CursorMode) -> Result<Handshake> {
    let conn = Connection::session().await.map_err(cap_err)?;

    let rd = Proxy::new(&conn, RD_NAME, RD_PATH, RD_NAME).await.map_err(cap_err)?;
    let rd_path: OwnedObjectPath = rd.call("CreateSession", &()).await.map_err(cap_err)?;
    let rd_session = Proxy::new(&conn, RD_NAME, rd_path, RD_SESSION_IFACE)
        .await
        .map_err(cap_err)?;
    let session_id: String = rd_session.get_property("SessionId").await.map_err(cap_err)?;

    let sc = Proxy::new(&conn, SC_NAME, SC_PATH, SC_NAME).await.map_err(cap_err)?;
    let mut opts: HashMap<&str, Value<'_>> = HashMap::new();
    opts.insert("remote-desktop-session-id", Value::from(session_id));
    let sc_path: OwnedObjectPath = sc.call("CreateSession", &(opts,)).await.map_err(cap_err)?;
    let sc_session = Proxy::new(&conn, SC_NAME, sc_path, SC_SESSION_IFACE)
        .await
        .map_err(cap_err)?;

    let record_opts = record_options(source, cursor_mode);
    let stream_path: OwnedObjectPath = match source {
        MutterSource::Virtual => sc_session.call("RecordVirtual", &(record_opts,)).await,
        MutterSource::PrimaryMonitor => sc_session.call("RecordMonitor", &("", record_opts)).await,
    }
    .map_err(cap_err)?;
    let stream = Proxy::new(&conn, SC_NAME, stream_path.clone(), SC_STREAM_IFACE)
        .await
        .map_err(cap_err)?;
    let mut added = stream.receive_signal("PipeWireStreamAdded").await.map_err(cap_err)?;
    let rd_closed = rd_session.receive_signal("Closed").await.map_err(cap_err)?;
    let sc_closed = sc_session.receive_signal("Closed").await.map_err(cap_err)?;

    rd_session.call::<_, _, ()>("Start", &()).await.map_err(cap_err)?;

    let msg = tokio::time::timeout(STREAM_TIMEOUT, added.next())
        .await
        .map_err(|_| FluxError::Capture("timed out waiting for Mutter PipeWireStreamAdded".into()))?
        .ok_or_else(|| FluxError::Capture("Mutter stream signal closed".into()))?;
    let node_id: u32 = msg
        .body()
        .deserialize()
        .map_err(|e| FluxError::Capture(format!("bad PipeWireStreamAdded body: {e}")))?;

    Ok(Handshake {
        conn,
        rd_session,
        stream_path,
        node_id,
        rd_closed,
        sc_closed,
    })
}

/// Whether a D-Bus failure means the Mutter session object no longer exists.
fn is_object_gone(error: &zbus::Error) -> bool {
    matches!(
        error,
        zbus::Error::MethodError(name, _, _)
            if matches!(
                name.as_str(),
                "org.freedesktop.DBus.Error.UnknownMethod" | "org.freedesktop.DBus.Error.UnknownObject"
            )
    )
}

async fn forward_commands(
    rd: &Proxy<'_>,
    stream_path: &str,
    mut rx: UnboundedReceiver<Cmd>,
    rd_closed: &mut ClosedStream,
    sc_closed: &mut ClosedStream,
    closed: &AtomicBool,
) {
    loop {
        let cmd = tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(cmd) => cmd,
                None => break,
            },
            _ = rd_closed.next() => {
                mark_closed(closed, "RemoteDesktop session Closed signal");
                return;
            }
            _ = sc_closed.next() => {
                mark_closed(closed, "ScreenCast session Closed signal");
                return;
            }
        };
        let result = match cmd {
            Cmd::Shutdown => break,
            Cmd::Motion { dx, dy } => rd.call::<_, _, ()>("NotifyPointerMotionRelative", &(dx, dy)).await,
            Cmd::Absolute { x, y } => {
                rd.call::<_, _, ()>("NotifyPointerMotionAbsolute", &(stream_path, x, y))
                    .await
            }
            Cmd::Button { button, down } => rd.call::<_, _, ()>("NotifyPointerButton", &(button, down)).await,
            Cmd::Axis { axis, steps } => rd.call::<_, _, ()>("NotifyPointerAxisDiscrete", &(axis, steps)).await,
            Cmd::Key { keycode, down } => rd.call::<_, _, ()>("NotifyKeyboardKeycode", &(keycode, down)).await,
        };
        if let Err(e) = result {
            if is_object_gone(&e) {
                mark_closed(closed, "session object disappeared");
                return;
            }
            tracing::warn!("Mutter input notify failed: {e}");
        }
    }
}

fn mark_closed(closed: &AtomicBool, reason: &str) {
    if !closed.swap(true, Ordering::AcqRel) {
        tracing::warn!("Mutter session closed by the compositor ({reason}); capture must be restarted");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closed_session_drops_input_quietly() {
        let (tx, mut rx) = unbounded_channel::<Cmd>();
        let closed = Arc::new(AtomicBool::new(false));
        let input = MutterInput {
            tx,
            size: Arc::new(StreamSize::new(Resolution::new(100, 100))),
            closed: Arc::clone(&closed),
        };
        input.key(30, true).unwrap();
        assert!(rx.try_recv().is_ok());
        closed.store(true, Ordering::Release);
        input.key(30, true).unwrap();
        input.pointer_absolute(0.5, 0.5).unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn unknown_object_errors_mean_session_gone() {
        let gone = zbus::Error::MethodError(
            zbus::names::OwnedErrorName::try_from("org.freedesktop.DBus.Error.UnknownMethod").unwrap(),
            None,
            zbus::message::Message::method_call("/", "Ping")
                .unwrap()
                .build(&())
                .unwrap(),
        );
        assert!(is_object_gone(&gone));
        assert!(!is_object_gone(&zbus::Error::InvalidReply));
    }

    #[test]
    fn record_virtual_options_request_a_platform_monitor_with_cursor_metadata() {
        let options = record_options(MutterSource::Virtual, CursorMode::Metadata);
        assert_eq!(options.len(), 2);
        assert!(matches!(options["cursor-mode"], Value::U32(2)));
        assert!(matches!(options["is-platform"], Value::Bool(true)));
    }

    #[test]
    fn record_monitor_options_only_carry_the_cursor_mode() {
        let options = record_options(MutterSource::PrimaryMonitor, CursorMode::Metadata);
        assert_eq!(options.len(), 1);
        assert!(matches!(options["cursor-mode"], Value::U32(2)));
    }

    #[test]
    fn source_defaults_to_virtual_unless_monitor_is_requested() {
        assert_eq!(MutterSource::from_setting(None), MutterSource::Virtual);
        assert_eq!(MutterSource::from_setting(Some("virtual")), MutterSource::Virtual);
        assert_eq!(MutterSource::from_setting(Some("monitor")), MutterSource::PrimaryMonitor);
    }

    #[test]
    fn cursor_modes_map_to_mutter_values() {
        assert_eq!(cursor_mode_value(CursorMode::Hidden), 0);
        assert_eq!(cursor_mode_value(CursorMode::Embedded), 1);
        assert_eq!(cursor_mode_value(CursorMode::Metadata), 2);
    }

    #[test]
    fn normalized_maps_to_pixel_corners_and_clamps() {
        assert_eq!(normalized_to_pixels(0.0, 0.0, 1920, 1080), (0.0, 0.0));
        assert_eq!(normalized_to_pixels(1.0, 1.0, 1920, 1080), (1919.0, 1079.0));
        assert_eq!(normalized_to_pixels(-1.0, 2.0, 1920, 1080), (0.0, 1079.0));
        assert_eq!(normalized_to_pixels(f64::NAN, 0.5, 101, 101), (0.0, 50.0));
    }

    #[test]
    fn axis_steps_rounds_without_losing_small_scrolls() {
        assert_eq!(axis_steps(0.0), 0);
        assert_eq!(axis_steps(1.0), 1);
        assert_eq!(axis_steps(-2.0), -2);
        assert_eq!(axis_steps(0.1), 1);
        assert_eq!(axis_steps(-0.1), -1);
        assert_eq!(axis_steps(2.4), 2);
    }

    fn mode(w: i32, h: i32, current: bool) -> Mode {
        let mut props = HashMap::new();
        if current {
            props.insert("is-current".to_string(), OwnedValue::try_from(Value::from(true)).unwrap());
        }
        (format!("{w}x{h}@60"), w, h, 60.0, 1.0, vec![1.0], props)
    }

    #[test]
    fn primary_monitor_uses_current_mode_of_primary_logical_monitor() {
        let id = |c: &str| (c.to_string(), "v".into(), "p".into(), "s".into());
        let state: DisplayState = (
            1,
            vec![
                (id("Virtual-0"), vec![mode(800, 600, false), mode(1920, 1080, true)], HashMap::new()),
                (id("Virtual-1"), vec![mode(640, 480, true)], HashMap::new()),
            ],
            vec![
                (1920, 0, 1.0, 0, false, vec![id("Virtual-1")], HashMap::new()),
                (0, 0, 1.0, 0, true, vec![id("Virtual-0")], HashMap::new()),
            ],
            HashMap::new(),
        );
        let geo = primary_monitor(&state).unwrap();
        assert_eq!(geo.connector, "Virtual-0");
        assert_eq!(geo.resolution, Resolution::new(1920, 1080));
        assert_eq!((geo.rect.left, geo.rect.top), (0, 0));
    }
}
