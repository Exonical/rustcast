//! Clipboard wire format and the wiring between frame servers and capture.

use std::sync::Arc;
use std::time::Instant;

/// Clipboard wire payload shared by frame-server message `0x05`
/// (host→client) and control command `0x07` (client→host):
/// `{"mime":"text/plain;charset=utf-8","data":"<utf-8 text>"}`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct ClipboardMessage {
    pub mime: String,
    pub data: String,
}

/// Text MIME types accepted in a client→host `0x07` command — the set
/// negotiated with Mutter's `EnableClipboard`.
pub const CLIPBOARD_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

fn clipboard_mime_ok(mime: &str) -> bool {
    CLIPBOARD_MIMES.contains(&mime)
}

/// Parse a `0x07` control payload; `Err` carries a short reason for logging.
pub(crate) fn parse_clipboard_set(payload: &[u8], max_bytes: usize) -> std::result::Result<ClipboardMessage, &'static str> {
    if payload.len() > max_bytes.saturating_add(4096) {
        return Err("clipboard command exceeds size cap");
    }
    let msg: ClipboardMessage = serde_json::from_slice(payload).map_err(|_| "invalid clipboard JSON")?;
    if !clipboard_mime_ok(&msg.mime) {
        return Err("unsupported clipboard MIME type");
    }
    if msg.data.len() > max_bytes {
        return Err("clipboard payload exceeds size cap");
    }
    Ok(msg)
}

/// Clipboard wiring shared by the TCP/QUIC frame servers and the capture
/// thread: client→host sets go through `sink`, host→client events are
/// broadcast on `events` as serialized `ClipboardMessage` JSON.
#[derive(Clone)]
pub(crate) struct ClipboardCtx {
    pub(crate) sink: flux_capture::traits::ClipboardSink,
    pub(crate) events: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    pub(crate) epoch: Instant,
    pub(crate) enabled: bool,
    pub(crate) max_bytes: usize,
}

/// Route clipboard sync through the capture session's backend, when it has
/// one, and spawn a forwarder turning backend events into `0x05` frames.
pub(crate) fn attach_clipboard(ctx: &ClipboardCtx, session: &dyn flux_capture::traits::CaptureSession) {
    #[cfg(not(target_os = "windows"))]
    {
        if !ctx.enabled {
            ctx.sink.set_backend(None);
            return;
        }
        let backend = session.clipboard_backend();
        ctx.sink.set_backend(backend.clone());
        if let Some(backend) = backend {
            let mut events = backend.events();
            let tx = ctx.events.clone();
            let epoch = ctx.epoch;
            match std::thread::Builder::new()
                .name("flux-clipboard".into())
                .spawn(move || {
                    while let Ok(event) = events.blocking_recv() {
                        let payload = match serde_json::to_vec(&ClipboardMessage {
                            mime: event.mime,
                            data: event.data,
                        }) {
                            Ok(p) => p,
                            Err(e) => {
                                tracing::warn!("clipboard event serialize failed: {e}");
                                continue;
                            }
                        };
                        let ts = epoch.elapsed().as_micros() as u64;
                        tracing::info!("clipboard 0x05 broadcast {} bytes", payload.len());
                        let _ = tx.send(Arc::new((ts, payload)));
                    }
                }) {
                Ok(_) => tracing::info!("Clipboard sync attached to capture session"),
                Err(e) => tracing::warn!("failed to spawn clipboard forwarder: {e}"),
            }
        }
    }
    #[cfg(target_os = "windows")]
    let _ = (ctx, session);
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_message_json_round_trips() {
        let msg = ClipboardMessage {
            mime: "text/plain;charset=utf-8".into(),
            data: "héllo".into(),
        };
        let json = serde_json::to_vec(&msg).unwrap();
        let parsed: ClipboardMessage = serde_json::from_slice(&json).unwrap();
        assert_eq!(parsed, msg);
    }

    #[test]
    fn parse_clipboard_set_enforces_mime_and_size() {
        let ok = parse_clipboard_set(
            br#"{"mime":"text/plain;charset=utf-8","data":"clip"}"#,
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(ok.data, "clip");

        assert_eq!(
            parse_clipboard_set(br#"{"mime":"image/png","data":"x"}"#, 1024 * 1024).unwrap_err(),
            "unsupported clipboard MIME type"
        );
        assert!(parse_clipboard_set(b"not json", 1024 * 1024).is_err());

        let big = format!(
            r#"{{"mime":"text/plain;charset=utf-8","data":"{}"}}"#,
            "x".repeat(2 * 1024 * 1024)
        );
        assert_eq!(
            parse_clipboard_set(big.as_bytes(), 1024 * 1024).unwrap_err(),
            "clipboard command exceeds size cap"
        );
    }
}
