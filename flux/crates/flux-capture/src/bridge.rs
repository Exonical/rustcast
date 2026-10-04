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
//!
//! The bridge also keeps a small pool of CPU frame buffers so a producer that
//! copies pixels (PipeWire shared memory) can reuse allocations instead of
//! allocating a full frame each time. A buffer only enters the pool once no
//! frame references it: either the frame was overwritten before the consumer
//! took it, or the consumer handed it back via [`FrameSource::recycle`].

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use flux_core::frame::CapturedFrame;

/// Maximum number of idle buffers kept for reuse. With one frame in the slot
/// and one held by the consumer, two spares let the producer keep copying
/// without allocating in steady state.
const MAX_FREE_BUFFERS: usize = 2;

#[derive(Default)]
struct Slot {
    frame: Option<CapturedFrame>,
    closed: bool,
    /// Frames overwritten before the consumer could take them.
    dropped: u64,
    /// Total frames pushed.
    pushed: u64,
    /// Idle CPU buffers no frame references anymore.
    free: Vec<Vec<u8>>,
}

impl Slot {
    fn recycle(&mut self, buf: Vec<u8>) {
        if buf.capacity() > 0 && self.free.len() < MAX_FREE_BUFFERS {
            self.free.push(buf);
        }
    }
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
        if let Some(stale) = slot.frame.replace(frame) {
            slot.dropped += 1;
            slot.recycle(stale.data);
        }
        slot.pushed += 1;
        let dropped = slot.dropped;
        drop(slot);
        self.shared.cv.notify_one();
        dropped
    }

    /// Take an idle buffer from the pool (empty, with any previous capacity
    /// retained), or a new empty `Vec` if none is available.
    pub fn take_buffer(&self) -> Vec<u8> {
        let mut buf = self.shared.slot.lock().unwrap().free.pop().unwrap_or_default();
        buf.clear();
        buf
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

    /// Return a consumed frame's buffer to the pool so the producer can reuse
    /// its allocation. The caller gives up ownership, so no frame can still
    /// reference it.
    pub fn recycle(&self, buf: Vec<u8>) {
        self.shared.slot.lock().unwrap().recycle(buf);
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

    fn frame_with_data(seq: u64, data: Vec<u8>) -> CapturedFrame {
        CapturedFrame { data, ..frame(seq) }
    }

    #[test]
    fn take_buffer_is_empty_without_recycled_buffers() {
        let (sink, _source) = FrameBridge::new();
        let buf = sink.take_buffer();
        assert!(buf.is_empty());
        assert_eq!(buf.capacity(), 0);
    }

    #[test]
    fn recycled_buffer_is_reused_by_producer() {
        let (sink, source) = FrameBridge::new();
        sink.push(frame_with_data(1, vec![7u8; 4096]));
        let got = source.recv(Duration::from_millis(10)).unwrap();
        let ptr = got.data.as_ptr();
        source.recycle(got.data);

        let buf = sink.take_buffer();
        assert!(buf.is_empty(), "reused buffer is handed out cleared");
        assert!(buf.capacity() >= 4096);
        assert_eq!(buf.as_ptr(), ptr, "same allocation is reused");
        assert_eq!(sink.take_buffer().capacity(), 0, "pool drained");
    }

    #[test]
    fn overwritten_frame_buffer_is_recycled() {
        let (sink, source) = FrameBridge::new();
        let stale = vec![1u8; 1024];
        let stale_ptr = stale.as_ptr();
        sink.push(frame_with_data(1, stale));
        sink.push(frame_with_data(2, vec![2u8; 1024]));

        let reused = sink.take_buffer();
        assert_eq!(reused.as_ptr(), stale_ptr);
        let got = source.recv(Duration::from_millis(10)).unwrap();
        assert_eq!(got.sequence, 2);
        assert_eq!(got.data, vec![2u8; 1024], "delivered frame is untouched");
    }

    #[test]
    fn pool_is_bounded_and_skips_empty_buffers() {
        let (sink, source) = FrameBridge::new();
        source.recycle(Vec::new());
        for _ in 0..MAX_FREE_BUFFERS + 3 {
            source.recycle(vec![0u8; 16]);
        }
        let mut reused = 0;
        while sink.take_buffer().capacity() > 0 {
            reused += 1;
        }
        assert_eq!(reused, MAX_FREE_BUFFERS);
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
