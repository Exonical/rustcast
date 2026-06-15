//! Push → pull frame bridge.
//!
//! PipeWire delivers frames by invoking a callback on its own loop thread
//! (push), but the [`CaptureSession`](crate::traits::CaptureSession) interface
//! the rest of the pipeline consumes is pull-based (`next_frame`). This bridge
//! connects the two with **latest-wins** semantics: it only ever holds the
//! most recent frame, so a slow consumer never builds an unbounded backlog —
//! older frames are dropped to keep end-to-end latency low.
//!
//! Use [`FrameBridge::new`] to obtain a [`FrameSink`] (handed to the producer,
//! e.g. the PipeWire `process` callback) and a [`FrameSource`] (polled by the
//! capture session). Both halves are `Send`; the sink is also `Clone`.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use flux_core::frame::CapturedFrame;

#[derive(Default)]
struct Slot {
    frame: Option<CapturedFrame>,
    closed: bool,
    /// Frames overwritten before the consumer could take them.
    dropped: u64,
    /// Total frames pushed.
    pushed: u64,
}

struct Shared {
    slot: Mutex<Slot>,
    cv: Condvar,
}

/// Statistics about the bridge's throughput and drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BridgeStats {
    pub pushed: u64,
    pub dropped: u64,
}

/// Producer half of the bridge. Cloneable and `Send`.
#[derive(Clone)]
pub struct FrameSink {
    shared: Arc<Shared>,
}

/// Consumer half of the bridge.
pub struct FrameSource {
    shared: Arc<Shared>,
}

/// Creates a connected [`FrameSink`] / [`FrameSource`] pair.
pub struct FrameBridge;

impl FrameBridge {
    /// Create a connected sink/source pair (channel-style constructor).
    #[allow(clippy::new_ret_no_self)]
    pub fn new() -> (FrameSink, FrameSource) {
        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            cv: Condvar::new(),
        });
        (FrameSink { shared: shared.clone() }, FrameSource { shared })
    }
}

impl FrameSink {
    /// Publish a frame. If the consumer has not yet taken the previous frame,
    /// it is overwritten (and counted as dropped). Returns the number of
    /// frames dropped so far.
    pub fn push(&self, frame: CapturedFrame) -> u64 {
        let mut slot = self.shared.slot.lock().unwrap();
        if slot.frame.is_some() {
            slot.dropped += 1;
        }
        slot.frame = Some(frame);
        slot.pushed += 1;
        let dropped = slot.dropped;
        drop(slot);
        self.shared.cv.notify_one();
        dropped
    }

    /// Close the bridge, unblocking any waiting consumer. After this,
    /// [`FrameSource::recv`] drains the last frame (if any) then returns
    /// `None`.
    pub fn close(&self) {
        let mut slot = self.shared.slot.lock().unwrap();
        slot.closed = true;
        drop(slot);
        self.shared.cv.notify_all();
    }

    pub fn stats(&self) -> BridgeStats {
        let slot = self.shared.slot.lock().unwrap();
        BridgeStats {
            pushed: slot.pushed,
            dropped: slot.dropped,
        }
    }
}

impl FrameSource {
    /// Take the latest frame immediately, if one is available.
    pub fn try_recv(&self) -> Option<CapturedFrame> {
        self.shared.slot.lock().unwrap().frame.take()
    }

    /// Block up to `timeout` for the latest frame.
    ///
    /// Returns `Some(frame)` when a frame is available, or `None` if the
    /// bridge was closed and drained or the timeout elapsed with no frame.
    pub fn recv(&self, timeout: Duration) -> Option<CapturedFrame> {
        let mut slot = self.shared.slot.lock().unwrap();
        loop {
            if let Some(frame) = slot.frame.take() {
                return Some(frame);
            }
            if slot.closed {
                return None;
            }
            let (next, wait) = self.shared.cv.wait_timeout(slot, timeout).unwrap();
            slot = next;
            if wait.timed_out() {
                return slot.frame.take();
            }
        }
    }

    pub fn stats(&self) -> BridgeStats {
        let slot = self.shared.slot.lock().unwrap();
        BridgeStats {
            pushed: slot.pushed,
            dropped: slot.dropped,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::types::{PixelFormat, Resolution};
    use std::time::Instant;

    fn frame(seq: u64) -> CapturedFrame {
        CapturedFrame {
            sequence: seq,
            timestamp: Instant::now(),
            format: PixelFormat::Bgra8,
            resolution: Resolution::new(64, 64),
            stride: 64 * 4,
            data: Vec::new(),
            gpu_handle: None,
        }
    }

    #[test]
    fn latest_wins_drops_older_frames() {
        let (sink, source) = FrameBridge::new();
        sink.push(frame(1));
        sink.push(frame(2));
        sink.push(frame(3));

        let got = source.recv(Duration::from_millis(10)).unwrap();
        assert_eq!(got.sequence, 3, "consumer should see only the newest frame");

        let stats = source.stats();
        assert_eq!(stats.pushed, 3);
        assert_eq!(stats.dropped, 2, "two older frames overwritten");

        assert!(source.try_recv().is_none(), "slot emptied after recv");
    }

    #[test]
    fn recv_times_out_without_frames() {
        let (_sink, source) = FrameBridge::new();
        let start = Instant::now();
        assert!(source.recv(Duration::from_millis(20)).is_none());
        assert!(start.elapsed() >= Duration::from_millis(15));
    }

    #[test]
    fn close_unblocks_waiting_consumer() {
        let (sink, source) = FrameBridge::new();
        let handle = std::thread::spawn(move || source.recv(Duration::from_secs(5)));
        // Give the consumer a moment to block, then close.
        std::thread::sleep(Duration::from_millis(50));
        sink.close();
        assert!(handle.join().unwrap().is_none(), "closed bridge returns None");
    }

    #[test]
    fn close_drains_remaining_frame_first() {
        let (sink, source) = FrameBridge::new();
        sink.push(frame(7));
        sink.close();
        let got = source.recv(Duration::from_millis(10));
        assert_eq!(got.map(|f| f.sequence), Some(7));
        assert!(source.recv(Duration::from_millis(10)).is_none());
    }

    #[test]
    fn producer_consumer_across_threads() {
        let (sink, source) = FrameBridge::new();
        let producer = std::thread::spawn(move || {
            for i in 1..=100 {
                sink.push(frame(i));
                std::thread::sleep(Duration::from_micros(50));
            }
            sink.close();
        });

        let mut last = 0;
        let mut count = 0;
        while let Some(f) = source.recv(Duration::from_millis(100)) {
            assert!(f.sequence >= last, "frames are monotonic, never reordered");
            last = f.sequence;
            count += 1;
        }
        producer.join().unwrap();
        assert!(count >= 1);
        assert_eq!(last, 100, "the final frame is always delivered before close");
    }
}
