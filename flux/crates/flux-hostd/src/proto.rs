//! Login exchange framing on the frame-server wire.
//!
//! host→relay message type `0x06` ("login") uses the standard frame format
//! `[type u8][ts u64][len u32][payload]` with a JSON payload:
//! `{"state":"prompt","style":"echo_on"|"echo_off"|"info"|"error","message":"..."}`,
//! `{"state":"ok","user":"bryce","uid":2101,"gid":8001,"home":"/home/bryce","reattached":bool}`,
//! `{"state":"failed","message":"..."}` or `{"state":"busy","user":"<other>"}`.
//!
//! relay→host command `0x08` ("login response") is
//! `[0x08][len u32 BE][JSON {"response":"..."}]`. Before a session is
//! established all other command types are parsed and dropped.

use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Host→client login frame type.
pub const MSG_LOGIN: u8 = 0x06;
/// Client→host login response command type.
pub const CMD_LOGIN_RESPONSE: u8 = 0x08;
/// Client→host lock command type (empty payload).
pub const CMD_LOCK: u8 = 0x09;
/// Client→host hello/auth command type (token payload).
pub const CMD_HELLO: u8 = 0x0A;

const MAX_LOGIN_PAYLOAD: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptStyle {
    EchoOn,
    EchoOff,
    Info,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LoginFrame {
    Prompt {
        style: PromptStyle,
        message: String,
    },
    Ok {
        user: String,
        uid: u32,
        gid: u32,
        home: String,
        reattached: bool,
    },
    Failed {
        message: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        user: Option<String>,
    },
    Busy {
        user: String,
    },
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LoginResponse {
    pub response: String,
}

/// A client command read from the wire pre-login. `Login` carries the
/// response text; everything else is consumed and dropped.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientCommand {
    Login(String),
    /// A non-login command that was consumed and dropped (its type byte).
    Dropped(u8),
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// Serialize a login frame as `[0x06][ts u64][len u32][JSON]`.
pub fn encode_login_frame(frame: &LoginFrame) -> Vec<u8> {
    let payload = serde_json::to_vec(frame).unwrap_or_default();
    let mut out = Vec::with_capacity(13 + payload.len());
    out.push(MSG_LOGIN);
    out.extend_from_slice(&now_micros().to_be_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

pub async fn write_login_frame(
    w: &mut (impl AsyncWriteExt + Unpin),
    frame: &LoginFrame,
) -> std::io::Result<()> {
    w.write_all(&encode_login_frame(frame)).await
}

/// Upper bound for any length-prefixed command payload.
const MAX_COMMAND_PAYLOAD: usize = 64 * 1024 * 1024;

/// Wire shape of a relay→host command after its type byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameShape {
    /// No body.
    Bare,
    /// A fixed number of body bytes.
    Fixed(usize),
    /// `[len u32 BE][payload]`.
    LengthPrefixed,
}

/// The relay→host command table: IDR(0x01) is bare,
/// bitrate/viewer/resolution (0x03/0x04/0x06) carry 4 bytes, quality(0x05)
/// 2 bytes; input(0x02), clipboard(0x07), login response(0x08), lock(0x09)
/// and hello(0x0A) are length-prefixed. `None` for unknown types.
fn command_frame_spec(ty: u8) -> Option<FrameShape> {
    match ty {
        0x01 => Some(FrameShape::Bare),
        0x03 | 0x04 | 0x06 => Some(FrameShape::Fixed(4)),
        0x05 => Some(FrameShape::Fixed(2)),
        0x02 | 0x07 | CMD_LOGIN_RESPONSE | CMD_LOCK | CMD_HELLO => Some(FrameShape::LengthPrefixed),
        _ => None,
    }
}

/// Read a command type byte; `None` on clean EOF.
async fn read_command_type(r: &mut (impl AsyncReadExt + Unpin)) -> std::io::Result<Option<u8>> {
    let mut ty = [0u8; 1];
    match r.read_exact(&mut ty).await {
        Ok(_) => Ok(Some(ty[0])),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(e),
    }
}

/// Read the rest of a command of type `ty` and return its full wire bytes
/// (type byte included). Length-prefixed payloads above `cap` are rejected.
async fn read_command_body(
    r: &mut (impl AsyncReadExt + Unpin),
    ty: u8,
    shape: FrameShape,
    cap: usize,
) -> std::io::Result<Vec<u8>> {
    let mut bytes = vec![ty];
    match shape {
        FrameShape::Bare => {}
        FrameShape::Fixed(n) => {
            bytes.resize(1 + n, 0);
            r.read_exact(&mut bytes[1..]).await?;
        }
        FrameShape::LengthPrefixed => {
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf).await?;
            let len = u32::from_be_bytes(len_buf) as usize;
            if len > cap {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("command payload too large: {len}"),
                ));
            }
            bytes.extend_from_slice(&len_buf);
            bytes.resize(5 + len, 0);
            r.read_exact(&mut bytes[5..]).await?;
        }
    }
    Ok(bytes)
}

/// Read one client command. Returns `None` on EOF / malformed stream.
/// Pre-login, unknown command bytes kill the connection (cannot resync), as
/// do lock (0x09) and hello (0x0A), which are only valid around a session.
pub async fn read_client_command(
    r: &mut (impl AsyncReadExt + Unpin),
) -> std::io::Result<Option<ClientCommand>> {
    let Some(ty) = read_command_type(r).await? else {
        return Ok(None);
    };
    let shape = match command_frame_spec(ty) {
        Some(shape) if ty != CMD_LOCK && ty != CMD_HELLO => shape,
        _ => {
            tracing::warn!("login: unknown command byte 0x{ty:02x}; closing");
            return Ok(None);
        }
    };
    if ty != CMD_LOGIN_RESPONSE {
        read_command_body(r, ty, shape, MAX_COMMAND_PAYLOAD).await?;
        return Ok(Some(ClientCommand::Dropped(ty)));
    }
    let bytes = read_command_body(r, ty, shape, MAX_LOGIN_PAYLOAD).await?;
    Ok(serde_json::from_slice::<LoginResponse>(&bytes[5..])
        .ok()
        .map(|resp| ClientCommand::Login(resp.response)))
}

/// One full client command as raw wire bytes plus its type byte. Used by the
/// proxy: everything is forwarded verbatim except intercepted commands.
#[derive(Debug)]
pub struct RawCommand {
    pub ty: u8,
    pub bytes: Vec<u8>,
}

/// Read one client command and keep its raw bytes. Returns `None` on EOF or
/// malformed framing.
pub async fn read_raw_command(
    r: &mut (impl AsyncReadExt + Unpin),
) -> std::io::Result<Option<RawCommand>> {
    let Some(ty) = read_command_type(r).await? else {
        return Ok(None);
    };
    let Some(shape) = command_frame_spec(ty) else {
        tracing::warn!("proxy: unknown command byte 0x{ty:02x}; closing");
        return Ok(None);
    };
    let bytes = read_command_body(r, ty, shape, MAX_COMMAND_PAYLOAD).await?;
    Ok(Some(RawCommand { ty, bytes }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn login_frame_wire_shape() {
        let bytes = encode_login_frame(&LoginFrame::Prompt {
            style: PromptStyle::EchoOff,
            message: "Password".into(),
        });
        assert_eq!(bytes[0], 0x06);
        let len = u32::from_be_bytes([bytes[9], bytes[10], bytes[11], bytes[12]]) as usize;
        assert_eq!(len, bytes.len() - 13);
        let json: serde_json::Value = serde_json::from_slice(&bytes[13..]).unwrap();
        assert_eq!(json["state"], "prompt");
        assert_eq!(json["style"], "echo_off");
        assert_eq!(json["message"], "Password");
    }

    #[test]
    fn login_frame_ok_fields() {
        let bytes = encode_login_frame(&LoginFrame::Ok {
            user: "bryce".into(),
            uid: 2101,
            gid: 8001,
            home: "/home/bryce".into(),
            reattached: true,
        });
        let json: serde_json::Value = serde_json::from_slice(&bytes[13..]).unwrap();
        assert_eq!(json["state"], "ok");
        assert_eq!(json["uid"], 2101);
        assert_eq!(json["gid"], 8001);
        assert_eq!(json["reattached"], true);
    }

    #[tokio::test]
    async fn reads_login_response() {
        let payload = br#"{"response":"devuser"}"#.to_vec();
        let mut wire = vec![0x08];
        wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        wire.extend_from_slice(&payload);
        let mut cur = Cursor::new(wire);
        let cmd = read_client_command(&mut cur).await.unwrap();
        assert_eq!(cmd, Some(ClientCommand::Login("devuser".into())));
    }

    #[tokio::test]
    async fn drops_fixed_and_length_commands() {
        let mut wire = Vec::new();
        wire.push(0x01); // IDR, bare
        wire.push(0x03);
        wire.extend_from_slice(&4000u32.to_be_bytes()); // bitrate
        wire.push(0x02);
        let input = b"{}";
        wire.extend_from_slice(&(input.len() as u32).to_be_bytes());
        wire.extend_from_slice(input);
        let mut cur = Cursor::new(wire);
        assert_eq!(
            read_client_command(&mut cur).await.unwrap(),
            Some(ClientCommand::Dropped(0x01))
        );
        assert_eq!(
            read_client_command(&mut cur).await.unwrap(),
            Some(ClientCommand::Dropped(0x03))
        );
        assert_eq!(
            read_client_command(&mut cur).await.unwrap(),
            Some(ClientCommand::Dropped(0x02))
        );
    }

    #[tokio::test]
    async fn unknown_command_ends_stream() {
        let mut cur = Cursor::new(vec![0x7f]);
        assert_eq!(read_client_command(&mut cur).await.unwrap(), None);
    }

    #[tokio::test]
    async fn eof_returns_none() {
        let mut cur = Cursor::new(Vec::new());
        assert_eq!(read_client_command(&mut cur).await.unwrap(), None);
    }

    /// One well-formed command of each known type, as wire bytes.
    fn sample_command(ty: u8) -> Vec<u8> {
        let mut wire = vec![ty];
        match command_frame_spec(ty).unwrap() {
            FrameShape::Bare => {}
            FrameShape::Fixed(n) => wire.extend((0..n).map(|i| i as u8 + 1)),
            FrameShape::LengthPrefixed => {
                let payload = br#"{"response":"x"}"#;
                wire.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                wire.extend_from_slice(payload);
            }
        }
        wire
    }

    #[tokio::test]
    async fn raw_command_keeps_exact_bytes() {
        for ty in 0u8..=0xff {
            if command_frame_spec(ty).is_none() {
                continue;
            }
            let cmd = sample_command(ty);
            let mut wire = cmd.clone();
            wire.push(0x01);
            let mut cur = Cursor::new(wire);
            let raw = read_raw_command(&mut cur).await.unwrap().unwrap();
            assert_eq!(raw.ty, ty);
            assert_eq!(raw.bytes, cmd, "type 0x{ty:02x}");
            assert_eq!(read_raw_command(&mut cur).await.unwrap().unwrap().ty, 0x01);
        }
    }

    #[tokio::test]
    async fn client_and_raw_readers_consume_same_bytes() {
        for ty in [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, CMD_LOGIN_RESPONSE] {
            let mut wire = sample_command(ty);
            wire.push(0x01);
            let mut cur = Cursor::new(wire);
            let cmd = read_client_command(&mut cur).await.unwrap();
            let expected = if ty == CMD_LOGIN_RESPONSE {
                ClientCommand::Login("x".into())
            } else {
                ClientCommand::Dropped(ty)
            };
            assert_eq!(cmd, Some(expected));
            assert_eq!(
                read_client_command(&mut cur).await.unwrap(),
                Some(ClientCommand::Dropped(0x01)),
                "type 0x{ty:02x} left the stream misaligned"
            );
        }
    }

    #[tokio::test]
    async fn lock_and_hello_end_stream_before_login() {
        for ty in [CMD_LOCK, CMD_HELLO] {
            let mut cur = Cursor::new(sample_command(ty));
            assert_eq!(read_client_command(&mut cur).await.unwrap(), None);
        }
    }

    #[tokio::test]
    async fn unknown_command_ends_raw_stream() {
        let mut cur = Cursor::new(vec![0x7f]);
        assert!(read_raw_command(&mut cur).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn login_response_cap_is_tighter_than_proxy_cap() {
        let len = MAX_LOGIN_PAYLOAD + 1;
        let mut wire = vec![CMD_LOGIN_RESPONSE];
        wire.extend_from_slice(&(len as u32).to_be_bytes());
        wire.resize(5 + len, b' ');
        let err = read_client_command(&mut Cursor::new(wire.clone())).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let raw = read_raw_command(&mut Cursor::new(wire.clone())).await.unwrap().unwrap();
        assert_eq!(raw.bytes, wire);
    }

    #[tokio::test]
    async fn oversized_payload_rejected() {
        let mut wire = vec![0x02];
        wire.extend_from_slice(&((MAX_COMMAND_PAYLOAD + 1) as u32).to_be_bytes());
        let err = read_raw_command(&mut Cursor::new(wire.clone())).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        let err = read_client_command(&mut Cursor::new(wire)).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
