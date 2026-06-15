//! Wire protocol message definitions.
//!
//! All control-plane messages are serialized as JSON over reliable QUIC streams.
//! Media-plane data (video/audio RTP) uses raw binary over UDP or QUIC datagrams.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use flux_core::types::{AudioCodec, ChromaSampling, DynamicRange, Resolution, VideoCodec};

/// Top-level protocol message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Unique message ID for request/response correlation.
    pub id: Uuid,

    /// Message payload.
    pub payload: MessagePayload,
}

impl Message {
    pub fn new(payload: MessagePayload) -> Self {
        Self {
            id: Uuid::new_v4(),
            payload,
        }
    }

    /// Serialize to JSON bytes.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Deserialize from JSON bytes.
    pub fn from_bytes(data: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(data)
    }
}

/// All possible message payloads exchanged between client and server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessagePayload {
    // ── Handshake ────────────────────────────────────────────────────
    /// Client → Server: Initial connection request.
    Hello(HelloMessage),

    /// Server → Client: Server capabilities and session offer.
    Welcome(WelcomeMessage),

    // ── Pairing ──────────────────────────────────────────────────────
    /// Server → Client: PIN required for pairing.
    PinRequired,

    /// Client → Server: PIN submission.
    PinSubmit(PinSubmitMessage),

    /// Server → Client: Pairing result.
    PairResult(PairResultMessage),

    // ── Session ──────────────────────────────────────────────────────
    /// Client → Server: Request to start a streaming session.
    SessionRequest(SessionRequestMessage),

    /// Server → Client: Session accepted with negotiated parameters.
    SessionAccepted(SessionAcceptedMessage),

    /// Server → Client: Session rejected.
    SessionRejected(SessionRejectedMessage),

    // ── Stream Control ───────────────────────────────────────────────
    /// Client → Server: Request an IDR / keyframe.
    RequestIdr,

    /// Client → Server: Request bitrate change.
    BitrateUpdate(BitrateUpdateMessage),

    /// Either → Either: Keepalive / ping.
    Ping { timestamp_ms: u64 },

    /// Either → Either: Pong response.
    Pong { echo_timestamp_ms: u64 },

    // ── Session Lifecycle ────────────────────────────────────────────
    /// Either → Either: Graceful session teardown.
    SessionEnd { reason: String },
}

// ── Message Payloads ─────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloMessage {
    /// Client's protocol version.
    pub protocol_version: u32,
    /// Client's human-readable name.
    pub client_name: String,
    /// Client's supported video codecs (ordered by preference).
    pub supported_codecs: Vec<VideoCodec>,
    /// Maximum resolution the client can decode.
    pub max_resolution: Resolution,
    /// Maximum framerate the client can decode.
    pub max_fps: u32,
    /// Whether the client supports HDR.
    pub supports_hdr: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WelcomeMessage {
    /// Server's protocol version.
    pub protocol_version: u32,
    /// Server's human-readable name.
    pub server_name: String,
    /// Server's available video codecs.
    pub available_codecs: Vec<VideoCodec>,
    /// Server's available audio codecs.
    pub available_audio_codecs: Vec<AudioCodec>,
    /// Maximum resolution the server can encode.
    pub max_resolution: Resolution,
    /// Whether the client is already paired.
    pub is_paired: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinSubmitMessage {
    pub pin: String,
    pub client_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairResultMessage {
    pub success: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRequestMessage {
    /// Requested video codec.
    pub video_codec: VideoCodec,
    /// Requested resolution.
    pub resolution: Resolution,
    /// Requested framerate.
    pub fps: u32,
    /// Requested video bitrate in kbps.
    pub video_bitrate_kbps: u32,
    /// Requested dynamic range.
    pub dynamic_range: DynamicRange,
    /// Requested chroma sampling.
    pub chroma_sampling: ChromaSampling,
    /// Requested audio codec.
    pub audio_codec: AudioCodec,
    /// Requested audio bitrate in kbps.
    pub audio_bitrate_kbps: u32,
    /// Enable input forwarding.
    pub enable_input: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAcceptedMessage {
    /// Assigned session ID.
    pub session_id: Uuid,
    /// Negotiated video codec (may differ from request).
    pub video_codec: VideoCodec,
    /// Negotiated resolution.
    pub resolution: Resolution,
    /// Negotiated framerate.
    pub fps: u32,
    /// Negotiated video bitrate.
    pub video_bitrate_kbps: u32,
    /// UDP port for video RTP.
    pub video_rtp_port: u16,
    /// UDP port for audio RTP.
    pub audio_rtp_port: u16,
    /// UDP port for input events.
    pub input_port: u16,
    /// AES-GCM encryption key for control messages (hex-encoded).
    pub control_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRejectedMessage {
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BitrateUpdateMessage {
    pub video_bitrate_kbps: u32,
}
