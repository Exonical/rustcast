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

/// Read one client command. Returns `None` on EOF / malformed stream.
/// Pre-login, unknown command bytes kill the connection (cannot resync).
pub async fn read_client_command(
    r: &mut (impl AsyncReadExt + Unpin),
) -> std::io::Result<Option<ClientCommand>> {
    let mut ty = [0u8; 1];
    match r.read_exact(&mut ty).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    // Consume a length-prefixed payload, capped at `cap`.
    async fn read_len_payload(
        r: &mut (impl AsyncReadExt + Unpin),
        cap: usize,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let mut len_buf = [0u8; 4];
        r.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        if len > cap {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("command payload too large: {len}"),
            ));
        }
        let mut payload = vec![0u8; len];
        r.read_exact(&mut payload).await?;
        Ok(Some(payload))
    }
    match ty[0] {
        CMD_LOGIN_RESPONSE => match read_len_payload(r, MAX_LOGIN_PAYLOAD).await? {
            Some(payload) => match serde_json::from_slice::<LoginResponse>(&payload) {
                Ok(resp) => Ok(Some(ClientCommand::Login(resp.response))),
                Err(_) => Ok(None),
            },
            None => Ok(None),
        },
        // Fixed-size commands: IDR(0x01)=bare, bitrate/viewer/resolution
        // (0x03/0x04/0x06)=4 bytes, quality(0x05)=2 bytes. Length-prefixed:
        // input(0x02), clipboard(0x07). Drop them all pre-login.
        0x01 => Ok(Some(ClientCommand::Dropped(0x01))),
        0x03 | 0x04 | 0x06 => {
            let mut buf = [0u8; 4];
            r.read_exact(&mut buf).await?;
            Ok(Some(ClientCommand::Dropped(ty[0])))
        }
        0x05 => {
            let mut buf = [0u8; 2];
            r.read_exact(&mut buf).await?;
            Ok(Some(ClientCommand::Dropped(0x05)))
        }
        0x02 | 0x07 => {
            read_len_payload(r, 64 * 1024 * 1024).await?;
            Ok(Some(ClientCommand::Dropped(ty[0])))
        }
        other => {
            tracing::warn!("login: unknown command byte 0x{other:02x}; closing");
            Ok(None)
        }
    }
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
    let mut ty = [0u8; 1];
    match r.read_exact(&mut ty).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut bytes = ty.to_vec();
    match ty[0] {
        0x01 => {}
        0x03 | 0x04 | 0x06 => {
            let mut b = [0u8; 4];
            r.read_exact(&mut b).await?;
            bytes.extend_from_slice(&b);
        }
        0x05 => {
            let mut b = [0u8; 2];
            r.read_exact(&mut b).await?;
            bytes.extend_from_slice(&b);
        }
        0x02 | 0x07 | 0x08 | CMD_LOCK => {
            let mut len_buf = [0u8; 4];
            r.read_exact(&mut len_buf).await?;
            let len = u32::from_be_bytes(len_buf) as usize;
            if len > 64 * 1024 * 1024 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "payload too large",
                ));
            }
            bytes.extend_from_slice(&len_buf);
            let mut payload = vec![0u8; len];
            r.read_exact(&mut payload).await?;
            bytes.extend_from_slice(&payload);
        }
        other => {
            tracing::warn!("proxy: unknown command byte 0x{other:02x}; closing");
            return Ok(None);
        }
    }
    Ok(Some(RawCommand { ty: ty[0], bytes }))
}
