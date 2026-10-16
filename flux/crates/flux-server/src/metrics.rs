use std::sync::LazyLock;

use prometheus::{
    Encoder, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts, Registry, TextEncoder,
};

static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

fn register<T: prometheus::core::Collector + Clone + 'static>(collector: T) -> T {
    REGISTRY
        .register(Box::new(collector.clone()))
        .expect("host metric registration");
    collector
}

fn counter(name: &str, help: &str) -> IntCounter {
    register(IntCounter::new(name, help).expect("host counter"))
}

fn counter_vec(name: &str, help: &str, labels: &[&str]) -> IntCounterVec {
    register(IntCounterVec::new(Opts::new(name, help), labels).expect("host counter vector"))
}

fn histogram(name: &str, help: &str) -> Histogram {
    register(
        Histogram::with_opts(
            HistogramOpts::new(name, help).buckets(prometheus::exponential_buckets(0.0005, 2.0, 12).expect("buckets")),
        )
        .expect("host histogram"),
    )
}

fn histogram_vec(name: &str, help: &str, labels: &[&str]) -> HistogramVec {
    register(
        HistogramVec::new(
            HistogramOpts::new(name, help).buckets(prometheus::exponential_buckets(0.0005, 2.0, 12).expect("buckets")),
            labels,
        )
        .expect("host histogram vector"),
    )
}

static CAPTURE_FRAMES: LazyLock<IntCounter> =
    LazyLock::new(|| counter("flux_host_capture_frames_total", "Captured frames."));
static ENCODED_FRAMES: LazyLock<IntCounterVec> =
    LazyLock::new(|| counter_vec("flux_host_encoded_frames_total", "Encoded video frames.", &["type"]));
static ENCODED_BYTES: LazyLock<IntCounter> =
    LazyLock::new(|| counter("flux_host_encoded_bytes_total", "Encoded video bytes."));
static IDR_REQUESTS: LazyLock<IntCounter> =
    LazyLock::new(|| counter("flux_host_idr_requests_total", "Encoder IDR requests."));
static CAPTURE_RESTARTS: LazyLock<IntCounter> =
    LazyLock::new(|| counter("flux_host_capture_restarts_total", "Capture session restarts."));
static FRAME_RESETS: LazyLock<IntCounter> =
    LazyLock::new(|| counter("flux_host_frame_resets_total", "Reset frame streams."));
static BROADCAST_LAGGED: LazyLock<IntCounter> = LazyLock::new(|| {
    counter(
        "flux_host_broadcast_lagged_frames_total",
        "Frames skipped by lagged broadcast receivers.",
    )
});
static CAPTURE_SECONDS: LazyLock<Histogram> =
    LazyLock::new(|| histogram("flux_host_capture_seconds", "Capture stage duration."));
static ENCODE_SECONDS: LazyLock<Histogram> =
    LazyLock::new(|| histogram("flux_host_encode_seconds", "Encode stage duration."));
static FRAME_SEND_SECONDS: LazyLock<HistogramVec> =
    LazyLock::new(|| histogram_vec("flux_host_frame_send_seconds", "Frame send duration.", &["type"]));
static ENCODER_TARGET: LazyLock<prometheus::Gauge> = LazyLock::new(|| {
    register(
        prometheus::Gauge::with_opts(Opts::new(
            "flux_host_encoder_target_kbps",
            "Current encoder bitrate target.",
        ))
        .expect("host gauge"),
    )
});
static VIEWERS: LazyLock<prometheus::Gauge> = LazyLock::new(|| {
    register(
        prometheus::Gauge::with_opts(Opts::new(
            "flux_host_viewers",
            "Current viewer count reported by the relay.",
        ))
        .expect("host gauge"),
    )
});
static HOST_INFO: LazyLock<GaugeVec> = LazyLock::new(|| {
    register(
        GaugeVec::new(
            Opts::new("flux_host_info", "Host version, encoder, GPU, and operating system."),
            &["version", "encoder_backend", "gpu_vendor", "os"],
        )
        .expect("host info"),
    )
});

pub fn init() {
    let _ = (
        &*CAPTURE_FRAMES,
        &*ENCODED_FRAMES,
        &*ENCODED_BYTES,
        &*IDR_REQUESTS,
        &*CAPTURE_RESTARTS,
        &*FRAME_RESETS,
        &*BROADCAST_LAGGED,
        &*CAPTURE_SECONDS,
        &*ENCODE_SECONDS,
        &*FRAME_SEND_SECONDS,
        &*ENCODER_TARGET,
        &*VIEWERS,
        &*HOST_INFO,
    );
}

pub fn encode() -> Result<String, prometheus::Error> {
    let encoder = TextEncoder::new();
    let mut buffer = Vec::new();
    encoder.encode(&REGISTRY.gather(), &mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer).into_owned())
}

pub fn capture_frame() {
    CAPTURE_FRAMES.inc();
}

pub fn encoded_frame(is_keyframe: bool, bytes: usize) {
    ENCODED_FRAMES
        .with_label_values(&[if is_keyframe { "key" } else { "delta" }])
        .inc();
    ENCODED_BYTES.inc_by(bytes as u64);
}

pub fn idr_request() {
    IDR_REQUESTS.inc();
}

pub fn capture_restart() {
    CAPTURE_RESTARTS.inc();
}

pub fn frame_reset() {
    FRAME_RESETS.inc();
}

pub fn broadcast_lagged(frames: u64) {
    BROADCAST_LAGGED.inc_by(frames);
}

pub fn capture_seconds(duration: f64) {
    CAPTURE_SECONDS.observe(duration);
}

pub fn encode_seconds(duration: f64) {
    ENCODE_SECONDS.observe(duration);
}

pub fn frame_send_seconds(is_keyframe: bool, duration: f64) {
    FRAME_SEND_SECONDS
        .with_label_values(&[if is_keyframe { "key" } else { "delta" }])
        .observe(duration);
}

pub fn encoder_target(kbps: u32) {
    ENCODER_TARGET.set(f64::from(kbps));
}

pub fn viewers(count: u32) {
    VIEWERS.set(f64::from(count));
}

pub fn set_host_info(encoder_backend: &str, gpu_vendor: &str, os: &str) {
    HOST_INFO.reset();
    HOST_INFO
        .with_label_values(&[env!("CARGO_PKG_VERSION"), encoder_backend, gpu_vendor, os])
        .set(1.0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_encoding_exposes_host_metric_names() {
        init();
        capture_frame();
        encoded_frame(true, 123);
        let body = encode().expect("text metrics");
        for name in [
            "flux_host_capture_frames_total",
            "flux_host_encoded_frames_total",
            "flux_host_encoded_bytes_total",
        ] {
            assert!(body.contains(name), "missing {name}");
        }
    }
}
