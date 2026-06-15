//! Mock portal / PipeWire implementations for tests and offline development.
//!
//! These let the capture pipeline be exercised on machines with no Wayland
//! session, no PipeWire daemon, and no GPU — which is exactly what CI needs.
//! Enabled in this crate's own tests, and for downstream crates via the
//! `mock` feature.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use flux_core::error::{FluxError, Result};
use flux_core::frame::CapturedFrame;
use flux_core::types::{PixelFormat, Resolution};

use crate::session::{
    BufferKind, FormatPrefs, NegotiatedFormat, PipewireFrameSource, PortalGrant, PortalOptions, PortalSession,
    PortalStream, SourceKind,
};

/// A scriptable mock [`PortalSession`].
///
/// By default it grants a single monitor stream. Set `deny` to simulate a
/// user rejecting the consent prompt.
pub struct MockPortalSession {
    pub grant: PortalGrant,
    pub deny: bool,
    streams: Vec<PortalStream>,
}

impl MockPortalSession {
    pub fn single_monitor() -> Self {
        let streams = vec![PortalStream {
            node_id: 42,
            position: Some((0, 0)),
            size: Some((1920, 1080)),
            kind: SourceKind::Monitor,
        }];
        Self {
            grant: PortalGrant {
                streams: streams.clone(),
                pipewire_fd: -1,
                restore_token: Some("mock-restore-token".into()),
                remote_desktop: true,
            },
            deny: false,
            streams,
        }
    }

    pub fn denying() -> Self {
        let mut s = Self::single_monitor();
        s.deny = true;
        s
    }
}

#[async_trait]
impl PortalSession for MockPortalSession {
    async fn negotiate(&mut self, _opts: PortalOptions) -> Result<PortalGrant> {
        if self.deny {
            return Err(FluxError::Capture("user denied portal consent".into()));
        }
        Ok(self.grant.clone())
    }

    fn streams(&self) -> &[PortalStream] {
        &self.streams
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}

/// A [`PipewireFrameSource`] that replays a queue of pre-canned frames.
pub struct MockFrameSource {
    queue: Arc<Mutex<VecDeque<CapturedFrame>>>,
    format: NegotiatedFormat,
    connected: bool,
}

impl MockFrameSource {
    /// Build a mock source that will emit `count` SHM frames of the given size.
    pub fn with_frames(count: u64, resolution: Resolution) -> Self {
        let mut q = VecDeque::new();
        for seq in 1..=count {
            q.push_back(CapturedFrame {
                sequence: seq,
                timestamp: std::time::Instant::now(),
                format: PixelFormat::Bgra8,
                resolution,
                stride: resolution.width * 4,
                data: vec![0u8; (resolution.width * resolution.height * 4) as usize],
                gpu_handle: None,
            });
        }
        Self {
            queue: Arc::new(Mutex::new(q)),
            format: NegotiatedFormat {
                buffer_kind: BufferKind::SharedMemory,
                format: PixelFormat::Bgra8,
                resolution,
                modifier: None,
            },
            connected: false,
        }
    }

    /// Number of frames still queued.
    pub fn remaining(&self) -> usize {
        self.queue.lock().unwrap().len()
    }
}

impl PipewireFrameSource for MockFrameSource {
    fn connect(&mut self, _fd: std::os::fd::RawFd, _node_id: u32, _prefs: FormatPrefs) -> Result<()> {
        self.connected = true;
        Ok(())
    }

    fn recv_frame(&mut self, _timeout: Duration) -> Result<Option<CapturedFrame>> {
        if !self.connected {
            return Err(FluxError::Capture("not connected".into()));
        }
        Ok(self.queue.lock().unwrap().pop_front())
    }

    fn negotiated_format(&self) -> Option<NegotiatedFormat> {
        if self.connected {
            Some(self.format.clone())
        } else {
            None
        }
    }

    fn disconnect(&mut self) -> Result<()> {
        self.connected = false;
        self.queue.lock().unwrap().clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::FrameSourceSession;
    use crate::traits::CaptureSession;

    #[tokio::test]
    async fn mock_portal_grants_single_monitor() {
        let mut portal = MockPortalSession::single_monitor();
        let grant = portal.negotiate(PortalOptions::default()).await.unwrap();
        assert_eq!(grant.streams.len(), 1);
        assert_eq!(grant.streams[0].node_id, 42);
        assert!(grant.remote_desktop);
        assert_eq!(grant.restore_token.as_deref(), Some("mock-restore-token"));
    }

    #[tokio::test]
    async fn mock_portal_denial_is_an_error() {
        let mut portal = MockPortalSession::denying();
        assert!(portal.negotiate(PortalOptions::default()).await.is_err());
    }

    #[test]
    fn frame_source_session_delivers_then_blocks() {
        let res = Resolution::new(640, 480);
        let mut source = MockFrameSource::with_frames(3, res);
        source.connect(-1, 42, FormatPrefs::default()).unwrap();
        let mut session = FrameSourceSession::new(source, Duration::from_millis(1));

        assert_eq!(session.next_frame().unwrap().sequence, 1);
        assert_eq!(session.next_frame().unwrap().sequence, 2);
        assert_eq!(
            session.negotiated_format().unwrap().buffer_kind,
            BufferKind::SharedMemory
        );
        assert_eq!(session.try_next_frame().unwrap().map(|f| f.sequence), Some(3));
        // Queue drained: try returns None rather than blocking.
        assert!(session.try_next_frame().unwrap().is_none());

        session.stop().unwrap();
        assert!(session.next_frame().is_err());
    }

    #[test]
    fn recv_before_connect_errors() {
        let mut source = MockFrameSource::with_frames(1, Resolution::new(8, 8));
        assert!(source.recv_frame(Duration::ZERO).is_err());
        assert!(source.negotiated_format().is_none());
    }

    /// End-to-end: negotiate a (mock) portal session, then push frames from a
    /// producer thread through the latest-wins [`FrameBridge`] and pull them
    /// from the consumer side — the same wiring the real portal + PipeWire
    /// stream uses, minus the OS dependencies.
    #[tokio::test]
    async fn portal_then_bridge_pipeline_delivers_frames() {
        use crate::bridge::FrameBridge;

        let mut portal = MockPortalSession::single_monitor();
        let grant = portal.negotiate(PortalOptions::default()).await.unwrap();
        let node_id = grant.streams[0].node_id;
        assert_eq!(node_id, 42);

        let res = Resolution::new(320, 240);
        let (sink, source) = FrameBridge::new();

        // Producer thread stands in for the PipeWire `process` callback.
        let producer = std::thread::spawn(move || {
            for seq in 1..=5u64 {
                sink.push(CapturedFrame {
                    sequence: seq,
                    timestamp: std::time::Instant::now(),
                    format: PixelFormat::Bgra8,
                    resolution: res,
                    stride: res.width * 4,
                    data: vec![0u8; (res.width * res.height * 4) as usize],
                    gpu_handle: None,
                });
            }
            sink.close();
        });

        // Consumer drains until the bridge is closed; latest-wins means we may
        // observe fewer than 5 frames, but the last must be frame 5.
        let mut last = 0;
        let mut count = 0;
        while let Some(frame) = source.recv(Duration::from_millis(200)) {
            assert!(frame.sequence >= last);
            last = frame.sequence;
            count += 1;
        }
        producer.join().unwrap();

        assert!(count >= 1, "expected at least one frame");
        assert_eq!(last, 5, "the most recent frame must always arrive");
    }
}
