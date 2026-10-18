//! Accumulates variable-size capture buffers into exact Opus-frame-sized
//! chunks, carrying the first sample's timestamp forward so each chunk keeps a
//! capture-time Instant.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::traits::AudioSamples;

/// Splits incoming [`AudioSamples`] into fixed-size frames (samples per
/// channel) for the Opus encoder.
pub struct AudioFramer {
    /// Samples per channel per output frame (e.g. 960 for 20 ms @ 48 kHz).
    frame_samples: usize,
    channels: u16,
    sample_rate: u32,
    /// Buffered interleaved samples not yet emitted.
    pending: VecDeque<f32>,
    /// Timestamp of the first pending sample.
    pending_start: Option<Instant>,
    sequence: u64,
}

impl AudioFramer {
    pub fn new(frame_samples: usize, channels: u16, sample_rate: u32) -> Self {
        Self {
            frame_samples,
            channels,
            sample_rate,
            pending: VecDeque::new(),
            pending_start: None,
            sequence: 0,
        }
    }

    /// Feed one capture buffer; returns any complete frames now available.
    pub fn push(&mut self, samples: AudioSamples) -> Vec<AudioSamples> {
        let frame_len = self.frame_samples * self.channels as usize;
        let mut offset = 0usize;
        let mut out = Vec::new();
        while self.pending.len() + (samples.data.len() - offset) >= frame_len {
            // Timestamp the chunk by the position of its first sample.
            let start = match self.pending_start {
                Some(t) => t,
                None => samples.timestamp + duration_of_samples(offset as u64 / self.channels as u64, self.sample_rate),
            };
            let take = frame_len - self.pending.len();
            let mut data: Vec<f32> = self.pending.drain(..).collect();
            data.extend_from_slice(&samples.data[offset..offset + take]);
            offset += take;
            self.pending_start = None;
            self.sequence += 1;
            out.push(AudioSamples {
                data,
                sample_rate: self.sample_rate,
                channels: self.channels,
                sequence: self.sequence,
                timestamp: start,
            });
        }
        if offset < samples.data.len() {
            if self.pending.is_empty() {
                self.pending_start = Some(
                    samples.timestamp + duration_of_samples(offset as u64 / self.channels as u64, self.sample_rate),
                );
            }
            self.pending.extend(&samples.data[offset..]);
        }
        out
    }
}

fn duration_of_samples(frames: u64, sample_rate: u32) -> Duration {
    Duration::from_secs_f64(frames as f64 / sample_rate.max(1) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(data: Vec<f32>, ts: Instant) -> AudioSamples {
        AudioSamples {
            data,
            sample_rate: 48000,
            channels: 2,
            sequence: 0,
            timestamp: ts,
        }
    }

    #[test]
    fn emits_exact_frames() {
        // 960 stereo frames = 1920 interleaved f32.
        let mut framer = AudioFramer::new(960, 2, 48000);
        let now = Instant::now();
        let out = framer.push(samples(vec![1.0; 1920], now));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].data.len(), 1920);
        assert_eq!(out[0].timestamp, now);
    }

    #[test]
    fn splits_and_carries_over() {
        let mut framer = AudioFramer::new(960, 2, 48000);
        let now = Instant::now();
        // 1.5 frames in one push, then another half.
        assert!(framer.push(samples(vec![0.0; 2880], now)).len() == 1);
        let out = framer.push(samples(vec![0.0; 960], Instant::now()));
        assert_eq!(out.len(), 1);
        // Second chunk's timestamp = first chunk's timestamp + 20 ms.
        assert_eq!(out[0].timestamp, now + Duration::from_millis(20));
    }

    #[test]
    fn undersized_buffers_accumulate() {
        let mut framer = AudioFramer::new(960, 2, 48000);
        let now = Instant::now();
        assert!(framer.push(samples(vec![0.0; 1000], now)).is_empty());
        let out = framer.push(samples(vec![0.0; 920], now + Duration::from_millis(10)));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].timestamp, now);
    }
}
