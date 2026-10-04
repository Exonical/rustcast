//! Session negotiation state machine.
//!
//! Implements the handshake flow:
//!   Client → Hello → Server
//!   Server → Welcome → Client
//!   (optional) Server → PinRequired → Client → PinSubmit → Server → PairResult
//!   Client → SessionRequest → Server
//!   Server → SessionAccepted / SessionRejected → Client
//!
//! **Unverified scaffolding — not wired into any live path.** Only the Hello
//! version check is implemented. PIN verification (`PinAuthenticator`),
//! paired-certificate lookup, codec intersection, capability/resource
//! validation and session key generation (`flux-crypto`) are missing, so the
//! security-relevant steps fail closed: [`SessionNegotiation::process`]
//! returns [`FluxError::Negotiation`] for `PinSubmit` and `SessionRequest`
//! instead of reporting a successful pairing or minting a session.

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

    fn handle_pin_submit(&mut self, _pin_msg: &PinSubmitMessage) -> Result<Vec<MessagePayload>> {
        self.fail_not_implemented("PIN pairing verification")
    }

    fn handle_session_request(&mut self, _req: &SessionRequestMessage) -> Result<Vec<MessagePayload>> {
        self.fail_not_implemented("session acceptance (capability validation and session key generation)")
    }

    fn fail_not_implemented(&mut self, step: &str) -> Result<Vec<MessagePayload>> {
        let reason = format!("{step} is not implemented");
        self.state = NegotiationState::Failed(reason.clone());
        Err(FluxError::Negotiation(reason))
    }

    /// Whether negotiation has reached the Established state.
    pub fn is_established(&self) -> bool {
        matches!(self.state, NegotiationState::Established)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flux_core::types::{AudioCodec, ChromaSampling, DynamicRange, VideoCodec};

    fn hello(protocol_version: u32) -> MessagePayload {
        MessagePayload::Hello(HelloMessage {
            protocol_version,
            client_name: "test-client".into(),
            supported_codecs: vec![VideoCodec::H264],
            max_resolution: Resolution::new(1920, 1080),
            max_fps: 60,
            supports_hdr: false,
        })
    }

    #[test]
    fn compatible_hello_gets_welcome() {
        let mut negotiation = SessionNegotiation::new();
        let replies = negotiation.process(&hello(version::PROTOCOL_VERSION)).unwrap();
        assert!(matches!(replies.as_slice(), [MessagePayload::Welcome(_)]));
        assert!(!negotiation.is_established());
    }

    #[test]
    fn incompatible_hello_is_rejected() {
        let mut negotiation = SessionNegotiation::new();
        let replies = negotiation.process(&hello(0)).unwrap();
        assert!(matches!(replies.as_slice(), [MessagePayload::SessionRejected(_)]));
    }

    #[test]
    fn session_request_fails_closed_instead_of_minting_a_session() {
        let mut negotiation = SessionNegotiation::new();
        negotiation.process(&hello(version::PROTOCOL_VERSION)).unwrap();
        let request = MessagePayload::SessionRequest(SessionRequestMessage {
            video_codec: VideoCodec::H264,
            resolution: Resolution::new(1920, 1080),
            fps: 60,
            video_bitrate_kbps: 20_000,
            dynamic_range: DynamicRange::Sdr,
            chroma_sampling: ChromaSampling::Yuv420,
            audio_codec: AudioCodec::Opus,
            audio_bitrate_kbps: 128,
            enable_input: true,
        });
        let err = negotiation.process(&request).unwrap_err();
        assert!(matches!(err, FluxError::Negotiation(_)), "{err}");
        assert!(!negotiation.is_established());
        assert!(negotiation.process(&request).is_err());
    }

    #[test]
    fn pin_submit_fails_closed_instead_of_reporting_paired() {
        let mut negotiation = SessionNegotiation {
            state: NegotiationState::AwaitingPin,
        };
        let pin = MessagePayload::PinSubmit(PinSubmitMessage {
            pin: "0000".into(),
            client_name: "test-client".into(),
        });
        let err = negotiation.process(&pin).unwrap_err();
        assert!(matches!(err, FluxError::Negotiation(_)), "{err}");
        assert!(!negotiation.is_established());
    }
}
