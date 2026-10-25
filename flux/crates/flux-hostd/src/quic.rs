//! QUIC frame endpoint for flux-hostd.
//!
//! The relay speaks the same wire shape flux-server's QUIC listener uses:
//! the client opens one bidirectional stream carrying command bytes, and the
//! server emits each frame message on its own unidirectional stream. We
//! bridge every connection to an in-memory duplex pipe so the whole
//! login/proxy path is identical to the TCP case.

use crate::{handle_connection, Shared};
use flux_crypto::CertificateManager;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const ALPN: &[u8] = b"flux-frames";

/// Build the QUIC endpoint with a persistent self-signed cert, returning it
/// plus the cert SHA-256 (lowercase hex) for registration pinning.
pub fn make_endpoint(
    bind: std::net::SocketAddr,
) -> Result<(quinn::Endpoint, String), Box<dyn std::error::Error>> {
    let certs = CertificateManager::load_or_create(
        std::path::Path::new("/var/lib/flux/quic-cert.pem"),
        std::path::Path::new("/var/lib/flux/quic-key.pem"),
    )?;
    let chain = certs.rustls_cert_chain()?;
    // cert_fingerprint returns colon-separated uppercase hex; the relay
    // compares against plain lowercase hex of the leaf DER.
    let fingerprint = flux_crypto::auth::cert_fingerprint(chain[0].as_ref())
        .to_lowercase()
        .replace(':', "");
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, certs.rustls_private_key()?)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls)?,
    ));
    let transport = Arc::get_mut(&mut server.transport).unwrap();
    transport.max_idle_timeout(Some(Duration::from_secs(15).try_into()?));
    transport.keep_alive_interval(Some(Duration::from_secs(3)));
    Ok((quinn::Endpoint::server(server, bind)?, fingerprint))
}

/// Accept QUIC connections and bridge each to the normal connection handler.
pub async fn serve(endpoint: quinn::Endpoint, shared: Arc<Shared>) {
    while let Some(incoming) = endpoint.accept().await {
        let shared = shared.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => {
                    tracing::info!("QUIC frame client connected: {}", conn.remote_address());
                    if let Err(e) = bridge(conn, shared).await {
                        tracing::debug!("quic connection ended: {e}");
                    }
                }
                Err(e) => tracing::warn!("QUIC handshake failed: {e}"),
            }
        });
    }
}

async fn bridge(conn: quinn::Connection, shared: Arc<Shared>) -> std::io::Result<()> {
    let peer = conn.remote_address().ip();
    // The relay's command channel: one bidirectional stream.
    let (_send_bi, mut recv_bi) = conn.accept_bi().await.map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::ConnectionAborted, format!("accept_bi: {e}"))
    })?;

    let (mut d_host, mut d_bridge) = tokio::io::duplex(256 * 1024);
    let (mut b_rd, mut b_wr) = tokio::io::split(d_bridge);

    let s = shared.clone();
    let host_task = tokio::spawn(async move { handle_connection(&mut d_host, peer, s).await });

    // Commands from the bidi stream are raw bytes: copy into the pipe.
    let to_host = async move {
        let r = tokio::io::copy(&mut recv_bi, &mut b_wr).await;
        let _ = b_wr.shutdown().await;
        r
    };

    tokio::select! {
        r = to_host => { let _ = r; }
        _ = to_quic_frames(&conn, &mut b_rd) => {}
        _ = host_task => {}
    }
    Ok(())
}

/// Read [type][ts][len][payload] frames from the pipe and emit each as one
/// unidirectional QUIC stream, matching the agent's wire shape.
async fn to_quic_frames(
    conn: &quinn::Connection,
    b_rd: &mut (impl AsyncReadExt + Unpin),
) -> std::io::Result<()> {
    let mut hdr = [0u8; 13];
    loop {
        b_rd.read_exact(&mut hdr).await?;
        let len = u32::from_be_bytes(hdr[9..13].try_into().unwrap()) as usize;
        if len > 16 * 1024 * 1024 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "frame too large",
            ));
        }
        let mut payload = vec![0u8; len];
        b_rd.read_exact(&mut payload).await?;
        let mut uni = conn.open_uni().await.map_err(std::io::Error::other)?;
        uni.write_all(&hdr).await?;
        uni.write_all(&payload).await?;
        uni.finish()?;
    }
}
