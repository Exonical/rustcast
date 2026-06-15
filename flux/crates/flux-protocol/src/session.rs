//! Session negotiation state machine.
//!
//! Implements the handshake flow:
//!   Client → Hello → Server
//!   Server → Welcome → Client
//!   (optional) Server → PinRequired → Client → PinSubmit → Server → PairResult
//!   Client → SessionRequest → Server
//!   Server → SessionAccepted / SessionRejected → Client

use flux_core::error::{FluxError, Result};
use flux_core::types::Resolution;

use crate::messages::*;
use crate::version;

/// Server-side session negotiation state machine.
pub struct SessionNegotiation {
    state: NegotiationState,
}

#[derive(Debug)]
#[allow(dead_code)]
enum NegotiationState {
    AwaitingHello,
    AwaitingPin,
    AwaitingSessionRequest { client_hello: HelloMessage },
    Established,
    Failed(String),
}

impl SessionNegotiation {
    pub fn new() -> Self {
        Self {
            state: NegotiationState::AwaitingHello,
        }
    }

    /// Process an incoming message and return the response message(s).
    pub fn process(&mut self, msg: &MessagePayload) -> Result<Vec<MessagePayload>> {
        match (&self.state, msg) {
            (NegotiationState::AwaitingHello, MessagePayload::Hello(hello)) => {
                self.handle_hello(hello)
            }
            (NegotiationState::AwaitingPin, MessagePayload::PinSubmit(pin_msg)) => {
                self.handle_pin_submit(pin_msg)
            }
            (
                NegotiationState::AwaitingSessionRequest { .. },
                MessagePayload::SessionRequest(req),
            ) => self.handle_session_request(req),
            _ => Err(FluxError::Protocol(format!(
                "unexpected message in state {:?}",
                self.state
            ))),
        }
    }

    fn handle_hello(&mut self, hello: &HelloMessage) -> Result<Vec<MessagePayload>> {
        if !version::is_compatible(hello.protocol_version) {
            self.state = NegotiationState::Failed("incompatible protocol version".into());
            return Ok(vec![MessagePayload::SessionRejected(
                SessionRejectedMessage {
                    reason: format!(
                        "protocol version {} not compatible (need >= {})",
                        hello.protocol_version,
                        version::MIN_COMPATIBLE_VERSION,
                    ),
                },
            )]);
        }

        tracing::info!(
            "Client '{}' connected (protocol v{})",
            hello.client_name,
            hello.protocol_version,
        );

        let welcome = WelcomeMessage {
            protocol_version: version::PROTOCOL_VERSION,
            server_name: "Flux Host".into(),
            available_codecs: hello.supported_codecs.clone(), // TODO: intersect with server caps
            available_audio_codecs: vec![flux_core::types::AudioCodec::Opus],
            max_resolution: Resolution::new(3840, 2160),
            is_paired: false, // TODO: check cert fingerprint
        };

        // TODO: Check if client is already paired. If not, require PIN.
        self.state = NegotiationState::AwaitingSessionRequest {
            client_hello: hello.clone(),
        };

        Ok(vec![MessagePayload::Welcome(welcome)])
    }

    fn handle_pin_submit(&mut self, pin_msg: &PinSubmitMessage) -> Result<Vec<MessagePayload>> {
        // TODO: Verify PIN via PinAuthenticator
        tracing::info!("Client submitted PIN for pairing");

        let result = PairResultMessage {
            success: true,
            message: "Paired successfully".into(),
        };

        self.state = NegotiationState::AwaitingSessionRequest {
            client_hello: HelloMessage {
                protocol_version: version::PROTOCOL_VERSION,
                client_name: pin_msg.client_name.clone(),
                supported_codecs: vec![],
                max_resolution: Resolution::new(1920, 1080),
                max_fps: 60,
                supports_hdr: false,
            },
        };

        Ok(vec![MessagePayload::PairResult(result)])
    }

    fn handle_session_request(
        &mut self,
        req: &SessionRequestMessage,
    ) -> Result<Vec<MessagePayload>> {
        tracing::info!(
            "Session requested: {} {}@{}fps {}kbps",
            req.video_codec,
            req.resolution,
            req.fps,
            req.video_bitrate_kbps,
        );

        // TODO: Validate against server capabilities and resource limits.
        // TODO: Allocate encoder, capture session, ports.
        // TODO: Generate session encryption key.

        let session_id = uuid::Uuid::new_v4();
        let accepted = SessionAcceptedMessage {
            session_id,
            video_codec: req.video_codec,
            resolution: req.resolution,
            fps: req.fps,
            video_bitrate_kbps: req.video_bitrate_kbps,
            video_rtp_port: 47998,
            audio_rtp_port: 48000,
            input_port: 47999,
            control_key: hex_encode(&flux_crypto_key_stub()),
        };

        self.state = NegotiationState::Established;

        Ok(vec![MessagePayload::SessionAccepted(accepted)])
    }

    /// Whether negotiation has reached the Established state.
    pub fn is_established(&self) -> bool {
        matches!(self.state, NegotiationState::Established)
    }
}

/// Stub: generate a 16-byte key. Real impl uses flux-crypto.
fn flux_crypto_key_stub() -> [u8; 16] {
    let mut key = [0u8; 16];
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::Instant::now().hash(&mut hasher);
    let hash = hasher.finish();
    key[0..8].copy_from_slice(&hash.to_le_bytes());
    key[8..16].copy_from_slice(&hash.to_be_bytes());
    key
}

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}
