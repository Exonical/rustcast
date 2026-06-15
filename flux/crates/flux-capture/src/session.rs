//! Linux/Wayland capture session interfaces.
//!
//! These traits decompose the Wayland capture path into two cooperating
//! halves so that the async D-Bus negotiation and the synchronous, real-time
//! frame delivery can live on different threads:
//!
//! * [`PortalSession`] — runs on the async (Tokio) side. It negotiates an
//!   `xdg-desktop-portal` `ScreenCast` (+ optional `RemoteDesktop`) session,
//!   handles user consent, and yields a PipeWire fd + the granted streams.
//! * [`PipewireFrameSource`] — runs on the dedicated capture thread. It
//!   connects to the negotiated PipeWire node and turns push-based PipeWire
//!   buffers into [`CapturedFrame`]s.
//!
//! The public [`ScreenCapture`](crate::traits::ScreenCapture) /
//! [`CaptureSession`](crate::traits::CaptureSession) traits are implemented on
//! top of these, bridged by [`crate::bridge`].

use std::os::fd::RawFd;
use std::time::Duration;

use async_trait::async_trait;
use flux_core::error::{FluxError, Result};
use flux_core::frame::CapturedFrame;
use flux_core::types::{PixelFormat, Resolution};

use crate::traits::CaptureSession;

/// The kind of source the user may pick in the portal dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Monitor,
    Window,
    Virtual,
}

/// How the cursor should be delivered relative to captured frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    /// Cursor is not captured.
    Hidden,
    /// Cursor is composited into the frame pixels.
    Embedded,
    /// Cursor is delivered as out-of-band metadata (position/shape).
    Metadata,
}

/// Options passed to [`PortalSession::negotiate`].
#[derive(Debug, Clone)]
pub struct PortalOptions {
    /// Source types to offer the user.
    pub source_kinds: Vec<SourceKind>,
    /// Allow selecting multiple sources (multi-monitor).
    pub multiple: bool,
    pub cursor_mode: CursorMode,
    /// Also negotiate `RemoteDesktop` (input) in the same session/consent.
    pub with_remote_desktop: bool,
    /// A previously persisted restore token, to re-grant without a prompt.
    pub restore_token: Option<String>,
}

impl Default for PortalOptions {
    fn default() -> Self {
        Self {
            source_kinds: vec![SourceKind::Monitor],
            multiple: false,
            cursor_mode: CursorMode::Embedded,
            with_remote_desktop: true,
            restore_token: None,
        }
    }
}

/// A single stream the user granted (one PipeWire node per source).
#[derive(Debug, Clone)]
pub struct PortalStream {
    /// PipeWire node id to connect to.
    pub node_id: u32,
    /// Source position within the virtual desktop (x, y), if reported.
    pub position: Option<(i32, i32)>,
    /// Source size (width, height), if reported.
    pub size: Option<(u32, u32)>,
    pub kind: SourceKind,
}

/// The result of a successful portal negotiation.
#[derive(Debug, Clone)]
pub struct PortalGrant {
    /// The granted streams (at least one on success).
    pub streams: Vec<PortalStream>,
    /// File descriptor for the authorized PipeWire connection.
    pub pipewire_fd: RawFd,
    /// Restore token to persist for prompt-less reconnection, if offered.
    pub restore_token: Option<String>,
    /// Whether `RemoteDesktop` (input) was granted in this session.
    pub remote_desktop: bool,
}

/// Choose which granted [`PortalStream`] to capture.
///
/// `requested` is a PipeWire `node_id` — the stable per-stream identifier the
/// portal reports (and the value [`crate::traits::DisplayInfo::id`] carries for
/// this backend). When `Some`, the matching stream is returned, or an error if
/// the compositor didn't grant it. When `None`, the stream positioned at the
/// virtual-desktop origin `(0, 0)` is preferred (the conventional primary
/// monitor), falling back to the first granted stream.
pub fn select_stream(streams: &[PortalStream], requested: Option<u32>) -> Result<&PortalStream> {
    if let Some(node_id) = requested {
        return streams
            .iter()
            .find(|s| s.node_id == node_id)
            .ok_or_else(|| {
                FluxError::Capture(format!(
                    "requested display (PipeWire node {node_id}) was not granted by the portal"
                ))
            });
    }
    streams
        .iter()
        .find(|s| s.position == Some((0, 0)))
        .or_else(|| streams.first())
        .ok_or_else(|| FluxError::Capture("portal granted no streams".into()))
}

/// Manages a Wayland portal session (async side).
#[async_trait]
pub trait PortalSession: Send {
    /// Negotiate a capture (and optionally input) session, prompting the user
    /// for consent. Returns the granted streams and PipeWire fd.
    async fn negotiate(&mut self, opts: PortalOptions) -> Result<PortalGrant>;

    /// Streams granted by the most recent successful [`Self::negotiate`].
    fn streams(&self) -> &[PortalStream];

    /// Close the portal session and release its resources.
    async fn close(&mut self) -> Result<()>;
}

/// Preferred frame formats to request from PipeWire, in priority order.
#[derive(Debug, Clone)]
pub struct FormatPrefs {
    /// Acceptable pixel formats, most-preferred first.
    pub formats: Vec<PixelFormat>,
    /// Prefer DMA-BUF (zero-copy) over shared-memory buffers.
    pub prefer_dmabuf: bool,
    /// Target resolution hint (the compositor may override).
    pub resolution: Resolution,
    /// Target framerate hint.
    pub framerate: u32,
}

impl Default for FormatPrefs {
    fn default() -> Self {
        Self {
            formats: vec![PixelFormat::Bgra8, PixelFormat::Nv12],
            prefer_dmabuf: true,
            resolution: Resolution::new(1920, 1080),
            framerate: 60,
        }
    }
}

/// Whether the negotiated PipeWire buffers are GPU (DMA-BUF) or CPU (SHM).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferKind {
    DmaBuf,
    SharedMemory,
}

/// The format PipeWire actually fixated for the stream.
#[derive(Debug, Clone)]
pub struct NegotiatedFormat {
    pub buffer_kind: BufferKind,
    pub format: PixelFormat,
    pub resolution: Resolution,
    /// DRM format modifier (DMA-BUF only).
    pub modifier: Option<u64>,
}

/// Receives frames from a PipeWire stream (synchronous, capture-thread side).
pub trait PipewireFrameSource: Send {
    /// Connect to a PipeWire node using a portal-provided fd, negotiating the
    /// given format preferences.
    fn connect(&mut self, pipewire_fd: RawFd, node_id: u32, prefs: FormatPrefs) -> Result<()>;

    /// Block up to `timeout` for the next frame. `Ok(None)` means no frame
    /// arrived before the timeout (not an error).
    fn recv_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>>;

    /// The format fixated by the stream, once known.
    fn negotiated_format(&self) -> Option<NegotiatedFormat>;

    /// Disconnect and stop the underlying PipeWire loop.
    fn disconnect(&mut self) -> Result<()>;
}

/// Adapts any [`PipewireFrameSource`] into the pull-based
/// [`CaptureSession`] consumed by the streaming pipeline.
///
/// `next_frame` polls the source in `poll_timeout` slices until a frame
/// arrives or the source reports an error; `try_next_frame` polls once with a
/// zero timeout.
pub struct FrameSourceSession<S: PipewireFrameSource> {
    source: S,
    poll_timeout: Duration,
    running: bool,
}

impl<S: PipewireFrameSource> FrameSourceSession<S> {
    pub fn new(source: S, poll_timeout: Duration) -> Self {
        Self {
            source,
            poll_timeout,
            running: true,
        }
    }

    /// The format the underlying source negotiated, if known.
    pub fn negotiated_format(&self) -> Option<NegotiatedFormat> {
        self.source.negotiated_format()
    }
}

impl<S: PipewireFrameSource> CaptureSession for FrameSourceSession<S> {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        if !self.running {
            return Err(FluxError::Capture("session stopped".into()));
        }
        loop {
            if let Some(frame) = self.source.recv_frame(self.poll_timeout)? {
                return Ok(frame);
            }
        }
    }

    fn try_next_frame(&mut self) -> Result<Option<CapturedFrame>> {
        if !self.running {
            return Ok(None);
        }
        self.source.recv_frame(Duration::ZERO)
    }

    fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.source.disconnect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(node_id: u32, position: Option<(i32, i32)>) -> PortalStream {
        PortalStream {
            node_id,
            position,
            size: Some((1920, 1080)),
            kind: SourceKind::Monitor,
        }
    }

    #[test]
    fn select_by_node_id_matches() {
        let streams = vec![stream(10, Some((0, 0))), stream(20, Some((1920, 0)))];
        let s = select_stream(&streams, Some(20)).unwrap();
        assert_eq!(s.node_id, 20);
    }

    #[test]
    fn select_by_unknown_node_id_errors() {
        let streams = vec![stream(10, None)];
        assert!(select_stream(&streams, Some(99)).is_err());
    }

    #[test]
    fn select_none_prefers_origin_over_first() {
        // Origin stream is not first; it should still win as the "primary".
        let streams = vec![stream(20, Some((1920, 0))), stream(10, Some((0, 0)))];
        let s = select_stream(&streams, None).unwrap();
        assert_eq!(s.node_id, 10);
    }

    #[test]
    fn select_none_falls_back_to_first_without_positions() {
        let streams = vec![stream(7, None), stream(8, None)];
        let s = select_stream(&streams, None).unwrap();
        assert_eq!(s.node_id, 7);
    }

    #[test]
    fn select_empty_errors() {
        assert!(select_stream(&[], None).is_err());
        assert!(select_stream(&[], Some(1)).is_err());
    }
}
