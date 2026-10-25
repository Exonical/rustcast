//! TCP frame server for the relay.

use std::sync::Arc;

use crate::PrivacyController;
use crate::frame_protocol::{
    CommandChannels, MSG_AUDIO, MSG_CLIPBOARD, MSG_CURSOR, MSG_RESOLUTION_STATUS, MSG_VIDEO, frame_header,
    read_commands,
};
use crate::resolution::ResolutionStatusMessage;

/// TCP frame server: accepts connections and streams timestamped H.264 NALUs.
/// Wire format (`MSG_*` messages out, commands in) is documented in `frame_protocol`.
pub(crate) async fn serve(
    listener: tokio::net::TcpListener,
    h264_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    audio_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    cursor_rx: tokio::sync::watch::Receiver<Arc<(u64, flux_core::cursor::CursorMetadata)>>,
    commands: CommandChannels,
    resolution_status_tx: tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
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
        let mut clipboard_rx = commands.clipboard.events.subscribe();
        let mut cursor_rx = cursor_rx.clone();
        let commands = commands.clone();
        let mut resolution_status_rx = resolution_status_tx.subscribe();
        #[cfg(target_os = "windows")]
        let privacy_connection = privacy_controller
            .as_ref()
            .map(|privacy| privacy.connection());
        #[cfg(not(target_os = "windows"))]
        let privacy_connection = None;

        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut frames_sent: u64 = 0;

            let mut reader_handle = tokio::spawn(read_commands(reader, addr, commands, privacy_connection));

            let initial_cursor = cursor_rx.borrow_and_update().clone();
            let initial_payload = match serde_json::to_vec(&initial_cursor.1) {
                Ok(payload) => payload,
                Err(error) => {
                    tracing::warn!("Failed to serialize initial cursor: {}", error);
                    Vec::new()
                }
            };
            if !initial_payload.is_empty()
                && write_frame_message(&mut writer, MSG_CURSOR, initial_cursor.0, &initial_payload)
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
                        if write_frame_message(&mut writer, MSG_CURSOR, cursor.0, &payload)
                            .await
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                    result = rx.recv() => match result {
                        Ok(d) => (MSG_VIDEO, d),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Frame client {} lagged by {} frames", addr, n);
                            continue;
                        }
                        Err(_) => break,
                    },
                    result = audio_rx.recv() => match result {
                        Ok(d) => (MSG_AUDIO, d),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                    result = clipboard_rx.recv() => match result {
                        Ok(d) => (MSG_CLIPBOARD, d),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    },
                    result = resolution_status_rx.recv() => match result {
                        Ok(status) => (MSG_RESOLUTION_STATUS, status),
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

    let header = frame_header(message_type, timestamp, payload.len());

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
