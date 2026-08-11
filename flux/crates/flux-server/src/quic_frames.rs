//! QUIC frame server: streams timestamped H.264 access units to the web relay
//! over QUIC (UDP + TLS 1.3) instead of TCP, avoiding head-of-line blocking on
//! lossy links. Each frame is sent on its own unidirectional stream, so a lost
//! packet only delays that frame, never the ones behind it. Commands from the
//! relay (IDR requests, input events) arrive on a client-opened bidirectional
//! stream using the same byte protocol as the TCP frame server.
//!
//! Stream formats:
//!   frame (server→client uni): [1-byte type][8-byte BE capture-ts µs][4-byte BE length][payload]
//!     type 0x01 = H.264 access unit, type 0x02 = cursor JSON metadata,
//!     type 0x03 = resolution transition status JSON
//!   control (client→server bi): [0x01] IDR request | [0x02][4-byte BE len][JSON input event]
//!                                | [0x03][4-byte BE target bitrate kbps]
//!                                | [0x04][4-byte BE viewer count]
//!                                | [0x05][quality level][FPS cap] (each 0 means automatic)
//!                                | [0x06][2-byte BE width][2-byte BE height]

use std::sync::Arc;
use std::time::{Duration, Instant};

use flux_crypto::CertificateManager;

#[cfg(target_os = "windows")]
use crate::ccd_display::{PrivacyConnection, PrivacyController};
#[cfg(not(target_os = "windows"))]
type PrivacyController = ();
#[cfg(not(target_os = "windows"))]
type PrivacyConnection = ();

pub const ALPN: &[u8] = b"flux-frames";

const FRAME_RESET_BACKLOG_THRESHOLD: usize = 3;
const FRAME_RESET_IDR_COOLDOWN: Duration = Duration::from_millis(500);

const SEND_TIMING_INTERVAL: Duration = Duration::from_secs(5);

/// How long frames take to hand to QUIC, accumulated over a reporting window.
/// This is the sender's share of a stall: a frame whose send takes far longer
/// than a frame interval is either being flow-controlled by the transport or is
/// simply too large for the link, and keyframes are tracked apart because they
/// are the frames big enough for that to happen to.
#[derive(Debug, Default)]
struct SendTimings {
    frames: u64,
    lagged: u64,
    sum: Duration,
    max: Duration,
    keyframes: u64,
    keyframe_max: Duration,
    keyframe_bytes_max: usize,
    window_started: Option<Instant>,
}

impl SendTimings {
    fn observe(&mut self, elapsed: Duration, bytes: usize, is_idr: bool) {
        self.frames += 1;
        self.sum += elapsed;
        self.max = self.max.max(elapsed);
        if is_idr {
            self.keyframes += 1;
            self.keyframe_max = self.keyframe_max.max(elapsed);
            self.keyframe_bytes_max = self.keyframe_bytes_max.max(bytes);
        }
    }

    /// Records frames the transport never saw because this task fell behind the
    /// encoder's broadcast channel.
    fn record_lag(&mut self, frames: u64) {
        self.lagged += frames;
    }

    /// Returns the window's summary once the reporting interval has elapsed,
    /// resetting the accumulator.
    fn take_due(&mut self, now: Instant) -> Option<String> {
        let started = *self.window_started.get_or_insert(now);
        if now.duration_since(started) < SEND_TIMING_INTERVAL || self.frames == 0 {
            return None;
        }
        let frames = self.frames as f64;
        let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
        let mut summary = format!(
            "frames={} lagged={} | send avg={:.1}ms max={:.1}ms",
            self.frames,
            self.lagged,
            ms(self.sum) / frames,
            ms(self.max),
        );
        if self.keyframes > 0 {
            summary += &format!(
                " | keyframes n={} max send={:.1}ms max={} bytes",
                self.keyframes,
                ms(self.keyframe_max),
                self.keyframe_bytes_max,
            );
        }
        *self = Self { window_started: Some(now), ..Self::default() };
        Some(summary)
    }
}

// A single newer frame is not enough reason to reset a stream, but discarded
// intermediate frames still warrant a rate-limited IDR request.
#[derive(Debug, Default)]
struct FrameResetPolicy {
    last_recovery: Option<Instant>,
}

impl FrameResetPolicy {
    fn should_reset(&mut self, backlog: usize, is_idr: bool, now: Instant) -> bool {
        if is_idr || backlog < FRAME_RESET_BACKLOG_THRESHOLD {
            return false;
        }
        if self
            .last_recovery
            .is_some_and(|last| now.duration_since(last) < FRAME_RESET_IDR_COOLDOWN)
        {
            return false;
        }
        self.last_recovery = Some(now);
        true
    }

    fn should_request_idr(&mut self, now: Instant) -> bool {
        if self
            .last_recovery
            .is_some_and(|last| now.duration_since(last) < FRAME_RESET_IDR_COOLDOWN)
        {
            return false;
        }
        self.last_recovery = Some(now);
        true
    }
}

fn is_idr_frame(data: &[u8]) -> bool {
    let mut index = 0;
    while index + 3 < data.len() {
        let start_code_len = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        if index + start_code_len < data.len() && data[index + start_code_len] & 0x1f == 5 {
            return true;
        }
        index += start_code_len;
    }
    false
}

pub fn make_endpoint(
    bind_addr: std::net::SocketAddr,
    cert_manager: &CertificateManager,
) -> Result<quinn::Endpoint, Box<dyn std::error::Error>> {
    let cert_chain = cert_manager.rustls_cert_chain()?;
    let key = cert_manager.rustls_private_key()?;

    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let mut server_config =
        quinn::ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(tls)?));
    let transport = Arc::get_mut(&mut server_config.transport).unwrap();
    transport.max_idle_timeout(Some(Duration::from_secs(15).try_into()?));
    transport.keep_alive_interval(Some(Duration::from_secs(3)));

    let endpoint = quinn::Endpoint::server(server_config, bind_addr)?;
    Ok(endpoint)
}

pub async fn serve(
    endpoint: quinn::Endpoint,
    h264_tx: tokio::sync::broadcast::Sender<Arc<(u64, Vec<u8>)>>,
    cursor_rx: tokio::sync::watch::Receiver<Arc<(u64, flux_core::cursor::CursorMetadata)>>,
    idr_tx: std::sync::mpsc::Sender<()>,
    bitrate_tx: std::sync::mpsc::Sender<u32>,
    quality_tx: std::sync::mpsc::Sender<(u8, u8)>,
    resolution_tx: std::sync::mpsc::Sender<crate::ModeRequest>,
    resolution_status_tx: tokio::sync::broadcast::Sender<crate::ResolutionStatusMessage>,
    input_tx: std::sync::mpsc::Sender<(std::time::Instant, flux_input::InputEvent)>,
    privacy_controller: Option<PrivacyController>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let connection = match incoming.await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("QUIC handshake failed: {}", e);
                continue;
            }
        };
        tracing::info!("QUIC frame client connected: {}", connection.remote_address());

        let rx = h264_tx.subscribe();
        let cursor_rx = cursor_rx.clone();
        tokio::spawn(handle_connection(
            connection,
            rx,
            cursor_rx,
            idr_tx.clone(),
            bitrate_tx.clone(),
            quality_tx.clone(),
            resolution_tx.clone(),
            resolution_status_tx.subscribe(),
            input_tx.clone(),
            privacy_controller.clone(),
        ));
    }
}

async fn handle_connection(
    connection: quinn::Connection,
    mut rx: tokio::sync::broadcast::Receiver<Arc<(u64, Vec<u8>)>>,
    mut cursor_rx: tokio::sync::watch::Receiver<Arc<(u64, flux_core::cursor::CursorMetadata)>>,
    idr_tx: std::sync::mpsc::Sender<()>,
    bitrate_tx: std::sync::mpsc::Sender<u32>,
    quality_tx: std::sync::mpsc::Sender<(u8, u8)>,
    resolution_tx: std::sync::mpsc::Sender<crate::ModeRequest>,
    mut resolution_status_rx: tokio::sync::broadcast::Receiver<crate::ResolutionStatusMessage>,
    input_tx: std::sync::mpsc::Sender<(std::time::Instant, flux_input::InputEvent)>,
    privacy_controller: Option<PrivacyController>,
) {
    #[cfg(not(target_os = "windows"))]
    let _ = &privacy_controller;
    #[cfg(target_os = "windows")]
    let privacy_connection = privacy_controller
        .as_ref()
        .map(|privacy| privacy.connection());
    #[cfg(not(target_os = "windows"))]
    let privacy_connection = None;
    let frame_idr_tx = idr_tx.clone();
    let control_conn = connection.clone();
    let control = tokio::spawn(async move {
        loop {
            let (_, recv) = match control_conn.accept_bi().await {
                Ok(s) => s,
                Err(_) => return,
            };
            let idr_tx = idr_tx.clone();
            let bitrate_tx = bitrate_tx.clone();
            let quality_tx = quality_tx.clone();
            let resolution_tx = resolution_tx.clone();
            let input_tx = input_tx.clone();
            let privacy_connection = privacy_connection.clone();
            tokio::spawn(read_commands(
                recv,
                idr_tx,
                bitrate_tx,
                quality_tx,
                resolution_tx,
                input_tx,
                privacy_connection,
            ));
        }
    });

    let initial = cursor_rx.borrow_and_update().clone();
    if let Ok(payload) = serde_json::to_vec(&initial.1) {
        if !send_message(&connection, 0x02, initial.0, payload).await {
            control.abort();
            return;
        }
    }

    let mut reset_policy = FrameResetPolicy::default();
    let mut send_timings = SendTimings::default();
    let mut pending_frame: Option<Arc<(u64, Vec<u8>)>> = None;
    loop {
        if let Some(summary) = send_timings.take_due(Instant::now()) {
            tracing::info!("QUIC send: {}", summary);
        }
        if let Some(frame) = pending_frame.take() {
            let bytes = frame.1.len();
            let is_idr = is_idr_frame(&frame.1);
            let started = Instant::now();
            let result = send_frame_message(
                &connection,
                frame.0,
                frame.1.clone(),
                &mut rx,
                &mut reset_policy,
                &frame_idr_tx,
            )
            .await;
            send_timings.observe(started.elapsed(), bytes, is_idr);
            match result {
                FrameSendResult::Complete => {}
                FrameSendResult::Superseded {
                    frame,
                    needs_recovery,
                    recovery_requested,
                } => {
                    if needs_recovery
                        && !recovery_requested
                        && reset_policy.should_request_idr(Instant::now())
                    {
                        let _ = frame_idr_tx.send(());
                    }
                    pending_frame = Some(frame);
                }
                FrameSendResult::Disconnected => break,
            }
            continue;
        }
        tokio::select! {
            result = rx.recv() => {
                let frame = match result {
                    Ok(f) => f,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("QUIC frame sender lagged by {} frames", n);
                        send_timings.record_lag(n);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                };
                let bytes = frame.1.len();
                let is_idr = is_idr_frame(&frame.1);
                let started = Instant::now();
                let result = send_frame_message(
                    &connection,
                    frame.0,
                    frame.1.clone(),
                    &mut rx,
                    &mut reset_policy,
                    &frame_idr_tx,
                ).await;
                send_timings.observe(started.elapsed(), bytes, is_idr);
                match result {
                    FrameSendResult::Complete => {}
                    FrameSendResult::Superseded {
                        frame,
                        needs_recovery,
                        recovery_requested,
                    } => {
                        if needs_recovery
                            && !recovery_requested
                            && reset_policy.should_request_idr(Instant::now())
                        {
                            let _ = frame_idr_tx.send(());
                        }
                        pending_frame = Some(frame);
                    }
                    FrameSendResult::Disconnected => break,
                }
            }
            result = cursor_rx.changed() => {
                if result.is_err() {
                    break;
                }
                let cursor = cursor_rx.borrow_and_update().clone();
                let Ok(payload) = serde_json::to_vec(&cursor.1) else {
                    continue;
                };
                if !send_message(&connection, 0x02, cursor.0, payload).await {
                    break;
                }
            }
            result = resolution_status_rx.recv() => {
                let status = match result {
                    Ok(status) => status,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                if !send_message(&connection, 0x03, status.0, status.1.clone()).await {
                    break;
                }
            }
        }
    }

    control.abort();
    connection.close(0u32.into(), b"done");
}

async fn send_message(
    connection: &quinn::Connection,
    message_type: u8,
    ts: u64,
    payload: Vec<u8>,
) -> bool {
    let mut stream = match connection.open_uni().await {
        Ok(s) => s,
        Err(error) => {
            tracing::info!("QUIC frame client disconnected: {}", error);
            return false;
        }
    };
    let mut header = [0u8; 13];
    header[0] = message_type;
    header[1..9].copy_from_slice(&ts.to_be_bytes());
    header[9..13].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    if stream.write_all(&header).await.is_err() || stream.write_all(&payload).await.is_err() {
        tracing::info!("QUIC frame write failed; client disconnected");
        return false;
    }
    let _ = stream.finish();
    true
}

enum FrameSendResult {
    Complete,
    Superseded {
        frame: Arc<(u64, Vec<u8>)>,
        needs_recovery: bool,
        recovery_requested: bool,
    },
    Disconnected,
}

async fn send_frame_message(
    connection: &quinn::Connection,
    ts: u64,
    payload: Vec<u8>,
    rx: &mut tokio::sync::broadcast::Receiver<Arc<(u64, Vec<u8>)>>,
    reset_policy: &mut FrameResetPolicy,
    idr_tx: &std::sync::mpsc::Sender<()>,
) -> FrameSendResult {
    let mut stream = match connection.open_uni().await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::info!("QUIC frame client disconnected: {}", error);
            return FrameSendResult::Disconnected;
        }
    };
    let mut header = [0u8; 13];
    header[0] = 0x01;
    header[1..9].copy_from_slice(&ts.to_be_bytes());
    header[9..13].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    if stream.write_all(&header).await.is_err() {
        tracing::info!("QUIC frame write failed; client disconnected");
        return FrameSendResult::Disconnected;
    }

    let is_idr = is_idr_frame(&payload);
    let mut pending = None;
    let mut backlog = 0;
    let mut dropped_frames = false;
    let mut offset = 0;
    while offset < payload.len() {
        if is_idr {
            match stream.write(&payload[offset..]).await {
                Ok(0) => return FrameSendResult::Disconnected,
                Ok(written) => {
                    offset += written;
                    continue;
                }
                Err(_) => {
                    tracing::info!("QUIC frame write failed; client disconnected");
                    return FrameSendResult::Disconnected;
                }
            }
        }

        let mut reset = false;
        let mut write_completed = false;
        {
            let write = stream.write(&payload[offset..]);
            tokio::pin!(write);
            tokio::select! {
                result = &mut write => {
                    match result {
                        Ok(0) => return FrameSendResult::Disconnected,
                        Ok(written) => {
                            offset += written;
                            write_completed = true;
                        }
                        Err(_) => {
                            tracing::info!("QUIC frame write failed; client disconnected");
                            return FrameSendResult::Disconnected;
                        }
                    }
                }
                result = rx.recv() => {
                    match result {
                        Ok(frame) => {
                            pending = Some(frame);
                            backlog += 1;
                            loop {
                                match rx.try_recv() {
                                    Ok(frame) => {
                                        pending = Some(frame);
                                        backlog += 1;
                                        dropped_frames = true;
                                    }
                                    Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => {
                                        backlog += n as usize;
                                        dropped_frames = true;
                                    }
                                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                                        return FrameSendResult::Disconnected;
                                    }
                                }
                            }
                            if reset_policy.should_reset(backlog, false, Instant::now()) {
                                reset = true;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            backlog += n as usize;
                            dropped_frames = true;
                            if reset_policy.should_request_idr(Instant::now()) {
                                let _ = idr_tx.send(());
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            return FrameSendResult::Disconnected;
                        }
                    }
                }
            }
        }
        if reset {
            let _ = stream.reset(quinn::VarInt::from_u32(0));
            let _ = idr_tx.send(());
            tracing::debug!(
                "resetting stale QUIC P-frame after {} newer frames queued",
                backlog
            );
            return FrameSendResult::Superseded {
                frame: pending.expect("pending frame"),
                needs_recovery: true,
                recovery_requested: true,
            };
        }
        if write_completed {
            continue;
        }
    }

    let _ = stream.finish();
    pending.map_or(FrameSendResult::Complete, |frame| FrameSendResult::Superseded {
        frame,
        needs_recovery: dropped_frames,
        recovery_requested: false,
    })
}

async fn read_commands(
    mut recv: quinn::RecvStream,
    idr_tx: std::sync::mpsc::Sender<()>,
    bitrate_tx: std::sync::mpsc::Sender<u32>,
    quality_tx: std::sync::mpsc::Sender<(u8, u8)>,
    resolution_tx: std::sync::mpsc::Sender<crate::ModeRequest>,
    input_tx: std::sync::mpsc::Sender<(std::time::Instant, flux_input::InputEvent)>,
    privacy_connection: Option<PrivacyConnection>,
) {
    #[cfg(not(target_os = "windows"))]
    let _ = &privacy_connection;
    loop {
        let mut cmd = [0u8; 1];
        if recv.read_exact(&mut cmd).await.is_err() {
            return;
        }
        match cmd[0] {
            0x01 => {
                tracing::info!("QUIC client requested IDR frame");
                let _ = idr_tx.send(());
            }
            0x02 => {
                let mut len_buf = [0u8; 4];
                if recv.read_exact(&mut len_buf).await.is_err() {
                    return;
                }
                let len = u32::from_be_bytes(len_buf) as usize;
                if len > 1024 * 1024 {
                    tracing::warn!("QUIC input event too large: {} bytes", len);
                    return;
                }
                let mut payload = vec![0u8; len];
                if recv.read_exact(&mut payload).await.is_err() {
                    return;
                }
                match serde_json::from_slice::<flux_input::InputEvent>(&payload) {
                    Ok(event) => {
                        let _ = input_tx.send((std::time::Instant::now(), event));
                    }
                    Err(e) => tracing::warn!("QUIC input event parse error: {}", e),
                }
            }
            0x03 => {
                let mut kbps_buf = [0u8; 4];
                if recv.read_exact(&mut kbps_buf).await.is_err() {
                    return;
                }
                let kbps = u32::from_be_bytes(kbps_buf);
                tracing::info!("QUIC client requested bitrate {} kbps", kbps);
                let _ = bitrate_tx.send(kbps);
            }
            0x04 => {
                let mut count_buf = [0u8; 4];
                if recv.read_exact(&mut count_buf).await.is_err() {
                    return;
                }
                let count = u32::from_be_bytes(count_buf);
                tracing::info!("QUIC client reported {} viewer(s)", count);
                #[cfg(target_os = "windows")]
                if let Some(privacy) = privacy_connection.as_ref() {
                    if let Err(error) = privacy.update(count) {
                        tracing::error!("Privacy mode viewer-count update failed: {error}");
                    }
                }
            }
            0x05 => {
                let mut levels = [0u8; 2];
                if recv.read_exact(&mut levels).await.is_err() {
                    return;
                }
                let _ = quality_tx.send((levels[0], levels[1]));
            }
            0x06 => {
                let mut dimensions = [0u8; 4];
                if recv.read_exact(&mut dimensions).await.is_err() {
                    return;
                }
                let _ = resolution_tx.send(crate::ModeRequest {
                    width: u16::from_be_bytes([dimensions[0], dimensions[1]]) as u32,
                    height: u16::from_be_bytes([dimensions[2], dimensions[3]]) as u32,
                });
            }
            other => {
                tracing::warn!("QUIC unknown command byte: 0x{:02x}", other);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_requires_a_backlog() {
        let now = Instant::now();
        let mut policy = FrameResetPolicy::default();
        assert!(!policy.should_reset(1, false, now));
        assert!(!policy.should_reset(FRAME_RESET_BACKLOG_THRESHOLD - 1, false, now));
        assert!(policy.should_reset(FRAME_RESET_BACKLOG_THRESHOLD, false, now));
    }

    #[test]
    fn idr_frames_are_never_reset() {
        let now = Instant::now();
        let mut policy = FrameResetPolicy::default();
        assert!(!policy.should_reset(FRAME_RESET_BACKLOG_THRESHOLD + 2, true, now));
    }

    #[test]
    fn resets_are_rate_limited() {
        let now = Instant::now();
        let mut policy = FrameResetPolicy::default();
        assert!(policy.should_reset(FRAME_RESET_BACKLOG_THRESHOLD, false, now));
        assert!(!policy.should_reset(FRAME_RESET_BACKLOG_THRESHOLD, false, now));
        assert!(policy.should_reset(
            FRAME_RESET_BACKLOG_THRESHOLD,
            false,
            now + FRAME_RESET_IDR_COOLDOWN
        ));
    }

    #[test]
    fn recovery_requests_share_the_reset_rate_limit() {
        let now = Instant::now();
        let mut policy = FrameResetPolicy::default();
        assert!(policy.should_request_idr(now));
        assert!(!policy.should_request_idr(now));
        assert!(policy.should_request_idr(now + FRAME_RESET_IDR_COOLDOWN));
    }

    #[test]
    fn detects_annex_b_idr_nalus() {
        assert!(is_idr_frame(&[0, 0, 0, 1, 5]));
        assert!(is_idr_frame(&[0, 0, 1, 5]));
        assert!(!is_idr_frame(&[0, 0, 0, 1, 1]));
    }

    #[test]
    fn send_timings_report_once_per_interval() {
        let start = Instant::now();
        let mut timings = SendTimings::default();
        timings.observe(Duration::from_millis(2), 20_000, false);
        assert!(timings.take_due(start).is_none());

        timings.record_lag(3);
        timings.observe(Duration::from_millis(60), 700_000, true);
        let summary = timings.take_due(start + SEND_TIMING_INTERVAL).unwrap();
        assert_eq!(
            summary,
            "frames=2 lagged=3 | send avg=31.0ms max=60.0ms \
             | keyframes n=1 max send=60.0ms max=700000 bytes"
        );

        // Counts are per window, so the next interval starts from zero.
        timings.observe(Duration::from_millis(1), 10_000, false);
        assert_eq!(
            timings.take_due(start + 2 * SEND_TIMING_INTERVAL).unwrap(),
            "frames=1 lagged=0 | send avg=1.0ms max=1.0ms"
        );
    }
}
