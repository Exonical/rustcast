//! QUIC frame server: streams timestamped H.264 access units to the web relay
//! over QUIC (UDP + TLS 1.3) instead of TCP, avoiding head-of-line blocking on
//! lossy links. Each frame is sent on its own unidirectional stream, so a lost
//! packet only delays that frame, never the ones behind it. Commands from the
//! relay (IDR requests, input events) arrive on a client-opened bidirectional
//! stream using the same byte protocol as the TCP frame server.
//!
//! Stream formats:
//!   frame (server→client uni): [8-byte BE capture-ts µs][4-byte BE length][H.264 data]
//!   control (client→server bi): [0x01] IDR request | [0x02][4-byte BE len][JSON input event]
//!                                | [0x03][4-byte BE target bitrate kbps]

use std::sync::Arc;
use std::time::Duration;

use flux_crypto::CertificateManager;

pub const ALPN: &[u8] = b"flux-frames";

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
    idr_tx: std::sync::mpsc::Sender<()>,
    bitrate_tx: std::sync::mpsc::Sender<u32>,
    input_tx: std::sync::mpsc::Sender<flux_input::InputEvent>,
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
        tokio::spawn(handle_connection(
            connection,
            rx,
            idr_tx.clone(),
            bitrate_tx.clone(),
            input_tx.clone(),
        ));
    }
}

async fn handle_connection(
    connection: quinn::Connection,
    mut rx: tokio::sync::broadcast::Receiver<Arc<(u64, Vec<u8>)>>,
    idr_tx: std::sync::mpsc::Sender<()>,
    bitrate_tx: std::sync::mpsc::Sender<u32>,
    input_tx: std::sync::mpsc::Sender<flux_input::InputEvent>,
) {
    let control_conn = connection.clone();
    let control = tokio::spawn(async move {
        loop {
            let (_, recv) = match control_conn.accept_bi().await {
                Ok(s) => s,
                Err(_) => return,
            };
            let idr_tx = idr_tx.clone();
            let bitrate_tx = bitrate_tx.clone();
            let input_tx = input_tx.clone();
            tokio::spawn(read_commands(recv, idr_tx, bitrate_tx, input_tx));
        }
    });

    loop {
        let frame = match rx.recv().await {
            Ok(f) => f,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::debug!("QUIC frame sender lagged by {} frames", n);
                continue;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        };

        let mut stream = match connection.open_uni().await {
            Ok(s) => s,
            Err(e) => {
                tracing::info!("QUIC frame client disconnected: {}", e);
                break;
            }
        };

        let (ts, data) = (&frame.0, &frame.1);
        let mut header = [0u8; 12];
        header[0..8].copy_from_slice(&ts.to_be_bytes());
        header[8..12].copy_from_slice(&(data.len() as u32).to_be_bytes());

        if stream.write_all(&header).await.is_err() || stream.write_all(data).await.is_err() {
            tracing::info!("QUIC frame write failed; client disconnected");
            break;
        }
        let _ = stream.finish();
    }

    control.abort();
    connection.close(0u32.into(), b"done");
}

async fn read_commands(
    mut recv: quinn::RecvStream,
    idr_tx: std::sync::mpsc::Sender<()>,
    bitrate_tx: std::sync::mpsc::Sender<u32>,
    input_tx: std::sync::mpsc::Sender<flux_input::InputEvent>,
) {
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
                        let _ = input_tx.send(event);
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
            other => {
                tracing::warn!("QUIC unknown command byte: 0x{:02x}", other);
                return;
            }
        }
    }
}
