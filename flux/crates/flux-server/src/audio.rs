//! Desktop audio capture and Opus encoding.

use std::sync::Arc;
use std::time::{Duration, Instant};

/// Desktop audio pipeline: PipeWire capture → framer → Opus → broadcast.
/// Payload sent to the frame server is [seq u32 BE][Opus bytes] with the
/// capture timestamp in microseconds relative to `stream_epoch`. Never fatal:
/// any failure just leaves the server running without audio.
pub(crate) fn audio_loop(
    audio_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    stream_epoch: Instant,
    config: flux_core::config::AudioConfig,
) {
    use flux_audio::traits::AudioEncoder;

    let mut capture = match flux_audio::create_audio_capture() {
        Ok(capture) => capture,
        Err(e) => {
            tracing::warn!("Audio capture unavailable: {e}");
            return;
        }
    };
    if let Err(e) = capture.start(None) {
        tracing::warn!("Audio capture failed to start: {e}");
        return;
    }
    let mut encoder = match flux_audio::create_audio_encoder(flux_audio::encoder::OpusEncoderConfig {
        sample_rate: config.sample_rate,
        channels: config.channels,
        bitrate_bps: config.bitrate_kbps.saturating_mul(1000),
        application: flux_audio::encoder::OpusApplication::Audio,
        ..Default::default()
    }) {
        Ok(encoder) => encoder,
        Err(e) => {
            tracing::warn!("Opus encoder unavailable: {e}");
            let _ = capture.stop();
            return;
        }
    };
    tracing::info!(
        "Audio encoder ready: Opus {}Hz {}ch {}kbps 20ms",
        config.sample_rate,
        config.channels,
        config.bitrate_kbps
    );
    let mut framer = flux_audio::AudioFramer::new(encoder.samples_per_frame(), config.channels, config.sample_rate);
    let mut last_warn = Instant::now() - Duration::from_secs(60);
    loop {
        let samples = match capture.next_samples() {
            Ok(samples) => samples,
            Err(e) => {
                tracing::warn!("Audio capture ended: {e}");
                break;
            }
        };
        for chunk in framer.push(samples) {
            // Keep draining capture even with no relay clients.
            if audio_tx.receiver_count() == 0 {
                continue;
            }
            match encoder.encode(&chunk) {
                Ok(packet) => {
                    let mut payload = Vec::with_capacity(4 + packet.data.len());
                    payload.extend_from_slice(&(packet.sequence as u32).to_be_bytes());
                    payload.extend_from_slice(&packet.data);
                    let ts = chunk.timestamp.saturating_duration_since(stream_epoch).as_micros() as u64;
                    let _ = audio_tx.send(Arc::new((ts, payload)));
                }
                Err(e) => {
                    if last_warn.elapsed() >= Duration::from_secs(5) {
                        tracing::warn!("Audio encode error: {e}");
                        last_warn = Instant::now();
                    }
                }
            }
        }
    }
}
