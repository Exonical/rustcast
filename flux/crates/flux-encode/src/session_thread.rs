//! Dedicated-thread bridge for encoders whose state is `!Send`.
//!
//! The VA-API (cros-codecs) and FFmpeg backends hold `Rc`s / raw libav
//! pointers, so their encoder state cannot cross threads. [`ThreadedSession`]
//! owns one thread per session that builds the state, then serves commands
//! sent by the [`EncodeSession`] handle over a channel. Backends only supply
//! the state constructor and a [`ThreadEncoder`] impl.
//!
//! Runtime bitrate changes are fire-and-forget: [`EncodeSession::set_bitrate`]
//! returns once the command is queued (it fails only if the thread is gone),
//! and a failure reported by [`ThreadEncoder::set_bitrate`] is logged as a
//! warning on the encode thread rather than surfaced to the caller.

use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};

use flux_core::error::{FluxError, Result};
use flux_core::frame::{CapturedFrame, EncodedPacket};

use crate::traits::EncodeSession;

/// Encoder state driven by a [`ThreadedSession`]; lives on the encode thread.
pub(crate) trait ThreadEncoder {
    fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>>;
    fn flush(&mut self) -> Result<Vec<EncodedPacket>>;
    /// Force the next encoded frame to be an IDR.
    fn request_idr(&mut self);
    /// Apply a new target bitrate. Errors are logged, not returned to the caller.
    fn set_bitrate(&mut self, kbps: u32) -> Result<()>;
}

type Reply = Sender<Result<Vec<EncodedPacket>>>;

/// A command sent from [`ThreadedSession`] to the encode thread.
enum Cmd {
    Encode { frame: Arc<CapturedFrame>, reply: Reply },
    RequestIdr,
    SetBitrate(u32),
    Flush { reply: Reply },
    Stop,
}

/// Handle to an encode session running on its own thread.
pub(crate) struct ThreadedSession {
    tx: Sender<Cmd>,
    handle: Option<JoinHandle<()>>,
}

impl ThreadedSession {
    /// Spawn the encode thread `thread_name`, build the state there with `init`,
    /// and wait for it so init failures are reported synchronously. `backend`
    /// labels log messages emitted on the thread.
    pub(crate) fn spawn<S, F>(backend: &'static str, thread_name: &str, init: F) -> Result<Self>
    where
        S: ThreadEncoder,
        F: FnOnce() -> Result<S> + Send + 'static,
    {
        let (tx, rx) = unbounded::<Cmd>();
        let (ready_tx, ready_rx) = bounded::<Result<()>>(1);

        let handle = thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || run_encode_thread(backend, init, rx, ready_tx))
            .map_err(|e| FluxError::EncoderInit(format!("failed to spawn encode thread: {e}")))?;

        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                handle: Some(handle),
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(FluxError::EncoderInit(
                    "encode thread exited before signalling readiness".into(),
                ))
            }
        }
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| FluxError::EncoderInit("encode thread is gone".into()))
    }

    fn request(&self, make: impl FnOnce(Reply) -> Cmd) -> Result<Vec<EncodedPacket>> {
        let (reply_tx, reply_rx) = bounded::<Result<Vec<EncodedPacket>>>(1);
        self.send(make(reply_tx))?;
        reply_rx
            .recv()
            .map_err(|_| FluxError::EncoderInit("encode thread dropped the reply".into()))?
    }
}

impl EncodeSession for ThreadedSession {
    fn encode(&mut self, frame: Arc<CapturedFrame>) -> Result<Vec<EncodedPacket>> {
        self.request(|reply| Cmd::Encode { frame, reply })
    }

    fn request_idr(&mut self) {
        let _ = self.tx.send(Cmd::RequestIdr);
    }

    fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
        self.request(|reply| Cmd::Flush { reply })
    }

    fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        self.send(Cmd::SetBitrate(bitrate_kbps))
    }
}

impl Drop for ThreadedSession {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run_encode_thread<S, F>(backend: &'static str, init: F, rx: Receiver<Cmd>, ready_tx: Sender<Result<()>>)
where
    S: ThreadEncoder,
    F: FnOnce() -> Result<S>,
{
    let mut state = match init() {
        Ok(state) => {
            let _ = ready_tx.send(Ok(()));
            state
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Encode { frame, reply } => {
                let result = state.encode(&frame);
                // Release the frame before replying so the caller can reclaim it.
                drop(frame);
                let _ = reply.send(result);
            }
            Cmd::Flush { reply } => {
                let _ = reply.send(state.flush());
            }
            Cmd::RequestIdr => state.request_idr(),
            Cmd::SetBitrate(kbps) => {
                if let Err(e) = state.set_bitrate(kbps) {
                    tracing::warn!("{backend} set_bitrate failed: {e}");
                }
            }
            Cmd::Stop => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use flux_core::types::{PixelFormat, Resolution};

    use super::*;

    #[derive(Default)]
    struct Log {
        bitrates: Mutex<Vec<u32>>,
        dropped: AtomicBool,
    }

    struct FakeState {
        log: Arc<Log>,
        force_idr: bool,
        fail_bitrate: bool,
    }

    impl ThreadEncoder for FakeState {
        fn encode(&mut self, frame: &CapturedFrame) -> Result<Vec<EncodedPacket>> {
            if frame.sequence == u64::MAX {
                return Err(FluxError::Encode {
                    frame: frame.sequence,
                    reason: "bad frame".into(),
                });
            }
            Ok(vec![EncodedPacket {
                data: frame.data.clone(),
                pts: frame.sequence,
                frame_index: frame.sequence,
                is_keyframe: std::mem::take(&mut self.force_idr),
            }])
        }

        fn flush(&mut self) -> Result<Vec<EncodedPacket>> {
            Ok(Vec::new())
        }

        fn request_idr(&mut self) {
            self.force_idr = true;
        }

        fn set_bitrate(&mut self, kbps: u32) -> Result<()> {
            self.log.bitrates.lock().unwrap().push(kbps);
            if self.fail_bitrate {
                return Err(FluxError::EncoderInit("tune failed".into()));
            }
            Ok(())
        }
    }

    impl Drop for FakeState {
        fn drop(&mut self) {
            self.log.dropped.store(true, Ordering::SeqCst);
        }
    }

    fn spawn_fake(log: &Arc<Log>, fail_bitrate: bool) -> ThreadedSession {
        let log = Arc::clone(log);
        ThreadedSession::spawn("fake", "flux-test-encode", move || {
            Ok(FakeState {
                log,
                force_idr: false,
                fail_bitrate,
            })
        })
        .unwrap()
    }

    fn frame(sequence: u64) -> Arc<CapturedFrame> {
        Arc::new(CapturedFrame {
            sequence,
            timestamp: Instant::now(),
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(1, 1),
            stride: 4,
            data: vec![sequence as u8; 4],
            gpu_handle: None,
        })
    }

    #[test]
    fn init_error_is_returned_from_spawn() {
        let err = ThreadedSession::spawn("fake", "flux-test-encode", || -> Result<FakeState> {
            Err(FluxError::EncoderInit("no device".into()))
        })
        .err()
        .unwrap();
        assert!(matches!(err, FluxError::EncoderInit(ref m) if m == "no device"));
    }

    #[test]
    fn init_panic_reports_missing_readiness() {
        let err = ThreadedSession::spawn("fake", "flux-test-encode", || -> Result<FakeState> { panic!("boom") })
            .err()
            .unwrap();
        assert!(matches!(err, FluxError::EncoderInit(ref m) if m.contains("before signalling readiness")));
    }

    #[test]
    fn encode_releases_frame_before_replying() {
        let log = Arc::new(Log::default());
        let mut session = spawn_fake(&log, false);
        let f = frame(7);
        let packets = session.encode(Arc::clone(&f)).unwrap();
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].pts, 7);
        assert_eq!(Arc::strong_count(&f), 1);
    }

    #[test]
    fn encode_error_is_returned_to_caller() {
        let log = Arc::new(Log::default());
        let mut session = spawn_fake(&log, false);
        assert!(matches!(session.encode(frame(u64::MAX)), Err(FluxError::Encode { .. })));
        assert!(session.encode(frame(1)).is_ok());
    }

    #[test]
    fn request_idr_applies_to_next_encode() {
        let log = Arc::new(Log::default());
        let mut session = spawn_fake(&log, false);
        assert!(!session.encode(frame(1)).unwrap()[0].is_keyframe);
        session.request_idr();
        assert!(session.encode(frame(2)).unwrap()[0].is_keyframe);
        assert!(!session.encode(frame(3)).unwrap()[0].is_keyframe);
    }

    #[test]
    fn set_bitrate_failure_is_logged_not_returned() {
        let log = Arc::new(Log::default());
        let mut session = spawn_fake(&log, true);
        assert!(session.set_bitrate(4_000).is_ok());
        assert!(session.set_bitrate(6_000).is_ok());
        // Commands are processed in order, so a round-trip proves both ran.
        assert!(session.flush().unwrap().is_empty());
        assert_eq!(*log.bitrates.lock().unwrap(), vec![4_000, 6_000]);
        assert!(session.encode(frame(1)).is_ok());
    }

    #[test]
    fn drop_stops_thread_and_drops_state() {
        let log = Arc::new(Log::default());
        let session = spawn_fake(&log, false);
        assert!(!log.dropped.load(Ordering::SeqCst));
        drop(session);
        assert!(log.dropped.load(Ordering::SeqCst));
    }
}
