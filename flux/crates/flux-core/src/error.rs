use thiserror::Error;

pub type Result<T> = std::result::Result<T, FluxError>;

#[derive(Debug, Error)]
pub enum FluxError {
    // ── Capture ──────────────────────────────────────────────────────
    #[error("screen capture failed: {0}")]
    Capture(String),

    #[error("no suitable capture backend available")]
    NoCaptureBackend,

    // ── Encoding ─────────────────────────────────────────────────────
    #[error("encoder initialization failed: {0}")]
    EncoderInit(String),

    #[error("encoding failed for frame {frame}: {reason}")]
    Encode { frame: u64, reason: String },

    #[error("no suitable encoder backend available")]
    NoEncoderBackend,

    // ── Audio ────────────────────────────────────────────────────────
    #[error("audio capture failed: {0}")]
    AudioCapture(String),

    #[error("audio encoding failed: {0}")]
    AudioEncode(String),

    // ── Transport ────────────────────────────────────────────────────
    #[error("network error: {0}")]
    Network(String),

    #[error("connection timed out after {0:?}")]
    Timeout(std::time::Duration),

    #[error("peer disconnected")]
    Disconnected,

    // ── Protocol ─────────────────────────────────────────────────────
    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("session negotiation failed: {0}")]
    Negotiation(String),

    // ── Crypto ───────────────────────────────────────────────────────
    #[error("cryptographic error: {0}")]
    Crypto(String),

    #[error("authentication failed: {0}")]
    Auth(String),

    // ── Input ────────────────────────────────────────────────────────
    #[error("input subsystem error: {0}")]
    Input(String),

    // ── Config ───────────────────────────────────────────────────────
    #[error("configuration error: {0}")]
    Config(String),

    #[error("failed to read config file: {0}")]
    ConfigRead(#[from] std::io::Error),

    // ── GPU / Platform ───────────────────────────────────────────────
    #[error("GPU error: {0}")]
    Gpu(String),

    #[error("platform not supported: {0}")]
    UnsupportedPlatform(String),

    // ── Generic ──────────────────────────────────────────────────────
    #[error("internal error: {0}")]
    Internal(String),
}
