//! Wire format shared by the TCP (`frame_server`) and QUIC (`quic_frames`)
//! frame servers.
//!
//! Host→relay messages: [1-byte type][8-byte BE capture-timestamp µs][4-byte BE length][payload].
//!   0x01 H.264 access unit (`MSG_VIDEO`)
//!   0x02 cursor JSON metadata (`MSG_CURSOR`)
//!   0x03 resolution transition status JSON (`MSG_RESOLUTION_STATUS`)
//!   0x04 Opus audio packet ([seq u32 BE][opus payload]) (`MSG_AUDIO`)
//!   0x05 clipboard JSON (`MSG_CLIPBOARD`)
//!
//! Relay→host commands:
//!   0x01 IDR request
//!   0x02 input event ([4-byte BE len][JSON input event])
//!   0x03 target bitrate ([4-byte BE kbps])
//!   0x04 viewer count ([4-byte BE count])
//!   0x05 quality/FPS ([quality level][FPS cap], each 0 means automatic)
//!   0x06 resolution ([2-byte BE width][2-byte BE height], refresh fixed at 60)
//!   0x07 clipboard ([4-byte BE len][JSON {"mime","data"}])

use std::net::SocketAddr;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::clipboard::{ClipboardCtx, parse_clipboard_set};
use crate::display_platform::PrivacyConnection;
use crate::resolution::{ModeRequest, validate_mode_request};

pub(crate) const FRAME_HEADER_LEN: usize = 13;

pub(crate) const MSG_VIDEO: u8 = 0x01;
pub(crate) const MSG_CURSOR: u8 = 0x02;
pub(crate) const MSG_RESOLUTION_STATUS: u8 = 0x03;
pub(crate) const MSG_AUDIO: u8 = 0x04;
pub(crate) const MSG_CLIPBOARD: u8 = 0x05;

const CMD_IDR: u8 = 0x01;
const CMD_INPUT: u8 = 0x02;
const CMD_BITRATE: u8 = 0x03;
const CMD_VIEWERS: u8 = 0x04;
const CMD_QUALITY: u8 = 0x05;
const CMD_RESOLUTION: u8 = 0x06;
const CMD_CLIPBOARD: u8 = 0x07;

const MAX_INPUT_BYTES: usize = 1024 * 1024;

/// Builds the 13-byte header of a host→relay message: type, timestamp, payload length.
pub(crate) fn frame_header(message_type: u8, timestamp: u64, payload_len: usize) -> [u8; FRAME_HEADER_LEN] {
    let mut header = [0u8; FRAME_HEADER_LEN];
    header[0] = message_type;
    header[1..9].copy_from_slice(&timestamp.to_be_bytes());
    header[9..13].copy_from_slice(&(payload_len as u32).to_be_bytes());
    header
}

/// Where decoded relay commands are delivered.
#[derive(Clone)]
pub(crate) struct CommandChannels {
    pub(crate) idr_tx: std::sync::mpsc::Sender<()>,
    pub(crate) bitrate_tx: std::sync::mpsc::Sender<u32>,
    pub(crate) quality_tx: std::sync::mpsc::Sender<(u8, u8)>,
    pub(crate) resolution_tx: std::sync::mpsc::Sender<ModeRequest>,
    pub(crate) input_tx: std::sync::mpsc::Sender<(Instant, flux_input::InputEvent)>,
    pub(crate) clipboard: ClipboardCtx,
}

/// Reads relay commands until EOF, a read error, or an oversized payload.
pub(crate) async fn read_commands<R>(
    mut reader: R,
    peer: SocketAddr,
    commands: CommandChannels,
    privacy_connection: Option<PrivacyConnection>,
) where
    R: AsyncRead + Unpin,
{
    while read_command(&mut reader, peer, &commands, privacy_connection.as_ref())
        .await
        .is_some()
    {}
}

/// Reads and dispatches one command; `None` means stop reading.
async fn read_command<R>(
    reader: &mut R,
    peer: SocketAddr,
    commands: &CommandChannels,
    privacy_connection: Option<&PrivacyConnection>,
) -> Option<()>
where
    R: AsyncRead + Unpin,
{
    let [cmd] = read_array(reader).await?;
    match cmd {
        CMD_IDR => {
            tracing::info!("Client {} requested IDR frame", peer);
            let _ = commands.idr_tx.send(());
        }
        CMD_INPUT => {
            let len = read_len(reader).await?;
            if len > MAX_INPUT_BYTES {
                tracing::warn!("Input event too large: {} bytes", len);
                return None;
            }
            let payload = read_payload(reader, len).await?;
            match serde_json::from_slice::<flux_input::InputEvent>(&payload) {
                Ok(event) => {
                    log_input(&event);
                    let _ = commands.input_tx.send((Instant::now(), event));
                }
                Err(e) => tracing::warn!("Failed to deserialize input event: {}", e),
            }
        }
        CMD_BITRATE => {
            let kbps = u32::from_be_bytes(read_array(reader).await?);
            tracing::info!("Client {} requested bitrate {} kbps", peer, kbps);
            let _ = commands.bitrate_tx.send(kbps);
        }
        CMD_VIEWERS => {
            let count = u32::from_be_bytes(read_array(reader).await?);
            crate::metrics::viewers(count);
            tracing::info!("Client {} reported {} viewer(s)", peer, count);
            if let Some(privacy) = privacy_connection
                && let Err(error) = privacy.update(count)
            {
                tracing::error!("Privacy mode viewer-count update failed: {error}");
            }
        }
        CMD_QUALITY => {
            let [level, fps] = read_array(reader).await?;
            let _ = commands.quality_tx.send((level, fps));
        }
        CMD_RESOLUTION => {
            let [w0, w1, h0, h1] = read_array(reader).await?;
            let request = ModeRequest {
                width: u16::from_be_bytes([w0, w1]) as u32,
                height: u16::from_be_bytes([h0, h1]) as u32,
            };
            if validate_mode_request(request).is_err() {
                tracing::warn!(
                    "Ignoring invalid resolution request: {}x{}",
                    request.width,
                    request.height
                );
            } else {
                let _ = commands.resolution_tx.send(request);
            }
        }
        CMD_CLIPBOARD => {
            let clipboard = &commands.clipboard;
            let len = read_len(reader).await?;
            if len > clipboard.max_bytes.saturating_add(4096) {
                tracing::warn!("Clipboard command too large: {} bytes", len);
                return None;
            }
            let payload = read_payload(reader, len).await?;
            if clipboard.enabled {
                match parse_clipboard_set(&payload, clipboard.max_bytes) {
                    Ok(msg) => {
                        if let Err(e) = clipboard.sink.set_text(msg.data) {
                            tracing::warn!("Clipboard set failed: {}", e);
                        }
                    }
                    Err(reason) => tracing::warn!("Rejected client clipboard: {}", reason),
                }
            }
        }
        other => tracing::warn!("Unknown command byte from {}: 0x{:02x}", peer, other),
    }
    Some(())
}

/// Logs non-movement input at INFO and movement at TRACE.
fn log_input(event: &flux_input::InputEvent) {
    use flux_input::InputEvent::Mouse;
    use flux_input::mouse::MouseEvent::{Move, MoveAbsolute};
    match event {
        Mouse(Move { .. }) | Mouse(MoveAbsolute { .. }) => tracing::trace!("Input: Move"),
        _ => tracing::info!("Input: {:?}", event),
    }
}

async fn read_array<const N: usize, R>(reader: &mut R) -> Option<[u8; N]>
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; N];
    reader.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

async fn read_len<R>(reader: &mut R) -> Option<usize>
where
    R: AsyncRead + Unpin,
{
    Some(u32::from_be_bytes(read_array(reader).await?) as usize)
}

async fn read_payload<R>(reader: &mut R, len: usize) -> Option<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, channel};

    struct Received {
        idr: Receiver<()>,
        bitrate: Receiver<u32>,
        quality: Receiver<(u8, u8)>,
        resolution: Receiver<ModeRequest>,
        input: Receiver<(Instant, flux_input::InputEvent)>,
    }

    fn channels() -> (CommandChannels, Received) {
        let (idr_tx, idr) = channel();
        let (bitrate_tx, bitrate) = channel();
        let (quality_tx, quality) = channel();
        let (resolution_tx, resolution) = channel();
        let (input_tx, input) = channel();
        let clipboard = ClipboardCtx {
            sink: flux_capture::traits::ClipboardSink::default(),
            events: tokio::sync::broadcast::channel::<Arc<(u64, Vec<u8>)>>(4).0,
            epoch: Instant::now(),
            enabled: true,
            max_bytes: 16,
        };
        let commands = CommandChannels {
            idr_tx,
            bitrate_tx,
            quality_tx,
            resolution_tx,
            input_tx,
            clipboard,
        };
        (
            commands,
            Received {
                idr,
                bitrate,
                quality,
                resolution,
                input,
            },
        )
    }

    async fn run(wire: &[u8]) -> Received {
        let (commands, received) = channels();
        read_commands(wire, "127.0.0.1:1".parse().unwrap(), commands, None).await;
        received
    }

    fn length_prefixed(cmd: u8, payload: &[u8]) -> Vec<u8> {
        let mut wire = vec![cmd];
        wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        wire.extend_from_slice(payload);
        wire
    }

    #[test]
    fn frame_header_layout() {
        assert_eq!(
            frame_header(0x04, 0x0102_0304_0506_0708, 0x0A0B_0C0D),
            [0x04, 1, 2, 3, 4, 5, 6, 7, 8, 0x0A, 0x0B, 0x0C, 0x0D]
        );
    }

    #[test]
    fn message_type_bytes() {
        assert_eq!(
            [MSG_VIDEO, MSG_CURSOR, MSG_RESOLUTION_STATUS, MSG_AUDIO, MSG_CLIPBOARD],
            [0x01, 0x02, 0x03, 0x04, 0x05]
        );
    }

    #[tokio::test]
    async fn dispatches_each_command() {
        let mut wire = vec![
            CMD_IDR,
            CMD_BITRATE,
            0,
            0,
            0x13,
            0x88,
            CMD_VIEWERS,
            0,
            0,
            0,
            2,
            CMD_QUALITY,
            3,
            30,
        ];
        wire.extend_from_slice(&[CMD_RESOLUTION, 0x07, 0x80, 0x04, 0x38]);
        wire.extend(length_prefixed(CMD_INPUT, br#"{"Mouse":{"Move":{"dx":1,"dy":2}}}"#));
        wire.extend(length_prefixed(CMD_CLIPBOARD, br#"{"mime":"text/plain","data":"hi"}"#));
        let received = run(&wire).await;

        assert!(received.idr.try_recv().is_ok());
        assert_eq!(received.bitrate.try_recv().unwrap(), 5000);
        assert_eq!(received.quality.try_recv().unwrap(), (3, 30));
        let mode = received.resolution.try_recv().unwrap();
        assert_eq!((mode.width, mode.height), (1920, 1080));
        assert!(matches!(
            received.input.try_recv().unwrap().1,
            flux_input::InputEvent::Mouse(flux_input::mouse::MouseEvent::Move { dx: 1, dy: 2 })
        ));
    }

    #[tokio::test]
    async fn invalid_resolution_is_dropped_and_reading_continues() {
        let received = run(&[CMD_RESOLUTION, 0, 100, 0, 100, CMD_IDR]).await;
        assert!(received.resolution.try_recv().is_err());
        assert!(received.idr.try_recv().is_ok());
    }

    #[tokio::test]
    async fn malformed_input_json_is_skipped() {
        let mut wire = length_prefixed(CMD_INPUT, b"not json");
        wire.push(CMD_IDR);
        let received = run(&wire).await;
        assert!(received.input.try_recv().is_err());
        assert!(received.idr.try_recv().is_ok());
    }

    #[tokio::test]
    async fn unknown_command_is_skipped_and_reading_continues() {
        let received = run(&[0x7F, CMD_IDR]).await;
        assert!(received.idr.try_recv().is_ok());
    }

    #[tokio::test]
    async fn oversized_payloads_stop_reading() {
        let mut wire = vec![CMD_INPUT];
        wire.extend_from_slice(&((MAX_INPUT_BYTES + 1) as u32).to_be_bytes());
        wire.push(CMD_IDR);
        assert!(run(&wire).await.idr.try_recv().is_err());

        let mut wire = length_prefixed(CMD_CLIPBOARD, &[b' '; 16 + 4097]);
        wire.push(CMD_IDR);
        assert!(run(&wire).await.idr.try_recv().is_err());
    }

    #[tokio::test]
    async fn truncated_command_stops_reading() {
        let received = run(&[CMD_BITRATE, 0, 0]).await;
        assert!(received.bitrate.try_recv().is_err());
    }
}
