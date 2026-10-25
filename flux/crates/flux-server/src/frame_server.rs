//! TCP frame server for the relay.

use std::sync::Arc;

use crate::PrivacyController;
use crate::clipboard::{ClipboardCtx, parse_clipboard_set};
use crate::resolution::{ModeRequest, ResolutionStatusMessage, validate_mode_request};

/// TCP frame server: accepts connections and streams timestamped H.264 NALUs.
/// Protocol: [1-byte type][8-byte BE capture-timestamp µs][4-byte BE length][payload].
/// Type 0x01 is an H.264 access unit; type 0x02 is cursor JSON metadata.
/// Control commands: 0x01 IDR, 0x02 input, 0x03 bitrate, 0x04 viewer count,
/// and 0x05 quality/FPS ([quality level, FPS cap], each 0 means automatic),
/// and 0x06 resolution ([2-byte BE width, 2-byte BE height], refresh fixed at 60),
/// and 0x07 clipboard ([4-byte BE len][JSON {"mime","data"}]).
/// Outbound message type 0x05 is a clipboard payload with the same JSON shape.
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    h264_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    audio_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    clipboard: ClipboardCtx,
    cursor_rx: tokio::sync::watch::Receiver<Arc<(u64, flux_core::cursor::CursorMetadata)>>,
    idr_tx: std::sync::mpsc::Sender<()>,
    bitrate_tx: std::sync::mpsc::Sender<u32>,
    quality_tx: std::sync::mpsc::Sender<(u8, u8)>,
    resolution_tx: std::sync::mpsc::Sender<ModeRequest>,
    resolution_status_tx: tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
    input_tx: std::sync::mpsc::Sender<(std::time::Instant, flux_input::InputEvent)>,
    privacy_controller: Option<PrivacyController>,
) {
    #[cfg(not(target_os = "windows"))]
    let _ = &privacy_controller;
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("Frame server accept error: {}", e);
                continue;
            }
        };
        tracing::info!("Frame client connected: {}", addr);

        // Low-latency socket setup: disable Nagle so each frame is sent
        // immediately, and mark packets DSCP AF41 (interactive video) so
        // QoS-aware networks prioritize them when the relay runs on
        // another machine. Both are best-effort.
        if let Err(e) = stream.set_nodelay(true) {
            tracing::debug!("set_nodelay failed: {}", e);
        }
        let sock = socket2::SockRef::from(&stream);
        if let Err(e) = sock.set_tos_v4(34 << 2) {
            tracing::debug!("DSCP not set (normal on Windows): {}", e);
        }
        let mut rx = h264_tx.subscribe();
        let mut audio_rx = audio_tx.subscribe();
        let mut clipboard_rx = clipboard.events.subscribe();
        let clipboard = clipboard.clone();
        let mut cursor_rx = cursor_rx.clone();
        let idr_tx = idr_tx.clone();
        let bitrate_tx = bitrate_tx.clone();
        let quality_tx = quality_tx.clone();
        let resolution_tx = resolution_tx.clone();
        let mut resolution_status_rx = resolution_status_tx.subscribe();
        let input_tx = input_tx.clone();
        #[cfg(target_os = "windows")]
        let privacy_connection = privacy_controller
            .as_ref()
            .map(|privacy| privacy.connection());
        #[cfg(not(target_os = "windows"))]
        let privacy_connection: Option<()> = None;
        #[cfg(not(target_os = "windows"))]
        let _ = &privacy_connection;

        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut reader, mut writer) = stream.into_split();
            let mut frames_sent: u64 = 0;

            // Spawn reader task to handle upstream commands
            let mut reader_handle = tokio::spawn(async move {
                #[cfg(target_os = "windows")]
                let privacy_connection = privacy_connection;
                loop {
                    // Read command byte
                    let mut cmd = [0u8; 1];
                    if reader.read_exact(&mut cmd).await.is_err() {
                        break;
                    }

                    match cmd[0] {
                        0x01 => {
                            // IDR Request
                            tracing::info!("Client {} requested IDR frame", addr);
                            let _ = idr_tx.send(());
                        }
                        0x02 => {
                            // Input Event
                            // Read 4-byte length (Big Endian)
                            let mut len_buf = [0u8; 4];
                            if reader.read_exact(&mut len_buf).await.is_err() {
                                break;
                            }
                            let len = u32::from_be_bytes(len_buf) as usize;

                            // Limit max input packet size (e.g. 1MB) to prevent OOM
                            if len > 1024 * 1024 {
                                tracing::warn!("Input event too large: {} bytes", len);
                                break;
                            }

                            let mut payload = vec![0u8; len];
                            if reader.read_exact(&mut payload).await.is_err() {
                                break;
                            }

                            // Deserialize and dispatch
                            match serde_json::from_slice::<flux_input::InputEvent>(&payload) {
                                Ok(event) => {
                                    // Log non-movement events at INFO, movements at TRACE
                                    match &event {
                                        flux_input::InputEvent::Mouse(flux_input::mouse::MouseEvent::Move { .. }) |
                                        flux_input::InputEvent::Mouse(flux_input::mouse::MouseEvent::MoveAbsolute { .. }) => {
                                            tracing::trace!("Input: Move");
                                        }
                                        _ => {
                                            tracing::info!("Input: {:?}", event);
                                        }
                                    }
                                    let _ = input_tx.send((std::time::Instant::now(), event));
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to deserialize input event: {}", e);
                                }
                            }
                        }
                        0x03 => {
                            // Bitrate update (congestion feedback from relay)
                            let mut kbps_buf = [0u8; 4];
                            if reader.read_exact(&mut kbps_buf).await.is_err() {
                                break;
                            }
                            let kbps = u32::from_be_bytes(kbps_buf);
                            tracing::info!("Client {} requested bitrate {} kbps", addr, kbps);
                            let _ = bitrate_tx.send(kbps);
                        }
                        0x04 => {
                            let mut count_buf = [0u8; 4];
                            if reader.read_exact(&mut count_buf).await.is_err() {
                                break;
                            }
                            let count = u32::from_be_bytes(count_buf);
                            crate::metrics::viewers(count);
                            tracing::info!("Client {} reported {} viewer(s)", addr, count);
                            #[cfg(target_os = "windows")]
                            if let Some(privacy) = privacy_connection.as_ref() {
                                if let Err(error) = privacy.update(count) {
                                    tracing::error!(
                                        "Privacy mode viewer-count update failed: {error}"
                                    );
                                }
                            }
                        }
                        0x05 => {
                            let mut level = [0u8; 2];
                            if reader.read_exact(&mut level).await.is_err() {
                                break;
                            }
                            let _ = quality_tx.send((level[0], level[1]));
                        }
                        0x07 => {
                            // Clipboard set: [len u32][JSON {"mime","data"}]
                            let mut len_buf = [0u8; 4];
                            if reader.read_exact(&mut len_buf).await.is_err() {
                                break;
                            }
                            let len = u32::from_be_bytes(len_buf) as usize;
                            if len > clipboard.max_bytes.saturating_add(4096) {
                                tracing::warn!("Clipboard command too large: {} bytes", len);
                                break;
                            }
                            let mut payload = vec![0u8; len];
                            if reader.read_exact(&mut payload).await.is_err() {
                                break;
                            }
                            if clipboard.enabled {
                                match parse_clipboard_set(&payload, clipboard.max_bytes) {
                                    Ok(msg) => {
                                        if let Err(e) = clipboard.sink.set_text(msg.data) {
                                            tracing::warn!("Clipboard set failed: {}", e);
                                        }
                                    }
                                    Err(reason) => {
                                        tracing::warn!("Rejected client clipboard: {}", reason);
                                    }
                                }
                            }
                        }
                        0x06 => {
                            let mut dimensions = [0u8; 4];
                            if reader.read_exact(&mut dimensions).await.is_err() {
                                break;
                            }
                            let request = ModeRequest {
                                width: u16::from_be_bytes([dimensions[0], dimensions[1]]) as u32,
                                height: u16::from_be_bytes([dimensions[2], dimensions[3]]) as u32,
                            };
                            if validate_mode_request(request).is_err() {
                                tracing::warn!(
                                    "Ignoring invalid resolution request: {}x{}",
                                    request.width,
                                    request.height
                                );
                            } else {
                                let _ = resolution_tx.send(request);
                            }
                        }
                        _ => {
                            tracing::warn!("Unknown command byte: 0x{:02x}", cmd[0]);
                            // We could break or ignore. If we ignore, we might lose sync if protocol expects strict format.
                            // For now, let's assume valid stream or disconnect.
                        }
                    }
                }
            });

            let initial_cursor = cursor_rx.borrow_and_update().clone();
            let initial_payload = match serde_json::to_vec(&initial_cursor.1) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!("Failed to serialize initial cursor: {}", error);
                    Vec::new()
                }
            };
            if !initial_payload.is_empty()
                && write_frame_message(&mut writer, 0x02, initial_cursor.0, &initial_payload)
                    .await
                    .is_err()
            {
                reader_handle.abort();
                return;
            }

            loop {
                let (message_type, msg) = tokio::select! {
                    result = &mut reader_handle => {
                        let _ = result;
                        break;
                    }
                    result = cursor_rx.changed() => {
                        if result.is_err() {
                            break;
                        }
                        let cursor = cursor_rx.borrow_and_update().clone();
                        let Ok(payload) = serde_json::to_vec(&cursor.1) else {
                            continue;
                        };
                        if write_frame_message(&mut writer, 0x02, cursor.0, &payload)
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    result = rx.recv() => match result {
                        Ok(d) => (0x01, d),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Frame client {} lagged by {} frames", addr, n);
                            continue;
                        }
                        Err(_) => break,
                    },
                    result = audio_rx.recv() => match result {
                        Ok(d) => (0x04, d),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                    result = clipboard_rx.recv() => match result {
                        Ok(d) => (0x05, d),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                    result = resolution_status_rx.recv() => match result {
                        Ok(status) => (0x03, status),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                };
                if write_frame_message(&mut writer, message_type, msg.0, &msg.1)
                    .await
                    .is_err()
                {
                    break;
                }
                frames_sent += 1;
            }

            reader_handle.abort();
            tracing::info!("Frame client disconnected: {} ({} frames sent)", addr, frames_sent);
        });
    }
}

/// Writes one frame-server message (13-byte header: type, timestamp u64 BE,
/// payload length u32 BE; then the payload) using vectored writes, so with
/// Nagle disabled the header is not sent as its own TCP segment.
async fn write_frame_message<W>(writer: &mut W, message_type: u8, timestamp: u64, payload: &[u8]) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;

    let mut header = [0u8; 13];
    header[0] = message_type;
    header[1..9].copy_from_slice(&timestamp.to_be_bytes());
    header[9..13].copy_from_slice(&(payload.len() as u32).to_be_bytes());

    let total = header.len() + payload.len();
    let mut written = 0;
    while written < total {
        let n = if written < header.len() {
            let bufs = [
                std::io::IoSlice::new(&header[written..]),
                std::io::IoSlice::new(payload),
            ];
            writer.write_vectored(&bufs).await?
        } else {
            writer.write(&payload[written - header.len()..]).await?
        };
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        written += n;
    }
    Ok(())
}

#[cfg(test)]
mod frame_message_tests {
    use super::write_frame_message;
    use std::io::IoSlice;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Accepts at most `limit` bytes per write call and counts calls.
    struct ChunkedWriter {
        out: Vec<u8>,
        limit: usize,
        calls: usize,
    }

    impl ChunkedWriter {
        fn new(limit: usize) -> Self {
            Self {
                out: Vec::new(),
                limit,
                calls: 0,
            }
        }
    }

    impl tokio::io::AsyncWrite for ChunkedWriter {
        fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
            self.poll_write_vectored(cx, &[IoSlice::new(buf)])
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            bufs: &[IoSlice<'_>],
        ) -> Poll<std::io::Result<usize>> {
            self.calls += 1;
            let mut n = 0;
            for buf in bufs {
                let take = buf.len().min(self.limit - n);
                self.out.extend_from_slice(&buf[..take]);
                n += take;
                if n == self.limit {
                    break;
                }
            }
            Poll::Ready(Ok(n))
        }

        fn is_write_vectored(&self) -> bool {
            true
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn expected(message_type: u8, timestamp: u64, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![message_type];
        v.extend_from_slice(&timestamp.to_be_bytes());
        v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[tokio::test]
    async fn header_and_payload_go_out_in_one_write() {
        let payload: Vec<u8> = (0..4096u32).map(|i| i as u8).collect();
        let mut w = ChunkedWriter::new(usize::MAX);
        write_frame_message(&mut w, 0x01, 0x0102_0304_0506_0708, &payload)
            .await
            .unwrap();
        assert_eq!(w.out, expected(0x01, 0x0102_0304_0506_0708, &payload));
        assert_eq!(w.calls, 1);
    }

    #[tokio::test]
    async fn partial_writes_produce_identical_bytes() {
        let payload: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        for limit in [1, 5, 12, 13, 14, 100, 1012, 1013] {
            let mut w = ChunkedWriter::new(limit);
            write_frame_message(&mut w, 0x03, 42, &payload).await.unwrap();
            assert_eq!(w.out, expected(0x03, 42, &payload), "limit {limit}");
        }
    }

    #[tokio::test]
    async fn empty_payload_writes_header_only() {
        let mut w = ChunkedWriter::new(usize::MAX);
        write_frame_message(&mut w, 0x02, 7, &[]).await.unwrap();
        assert_eq!(w.out, expected(0x02, 7, &[]));
    }

    #[tokio::test]
    async fn zero_length_write_is_an_error() {
        let mut w = ChunkedWriter::new(0);
        let err = write_frame_message(&mut w, 0x01, 0, b"x").await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);
    }
}
