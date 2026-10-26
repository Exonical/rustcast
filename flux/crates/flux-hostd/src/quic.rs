//! QUIC frame endpoint for flux-hostd.
//!
//! The relay speaks the same wire shape flux-server's QUIC listener uses:
//! the client opens one bidirectional stream carrying command bytes, and the
//! server emits each frame message on its own unidirectional stream. We
//! bridge every connection to an in-memory duplex pipe so the whole
//! login/proxy path is identical to the TCP case.

use crate::{handle_connection, Shared};
use flux_crypto::CertificateManager;
use serde::Serialize;
use std::error::Error;
use std::io::{self, BufReader};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const ALPN: &[u8] = b"flux-frames";
const LINK_HELLO_MAX_SIZE: usize = 4 * 1024;
type LinkError = Box<dyn Error + Send + Sync>;

#[derive(Serialize)]
struct LinkHello<'a> {
    machine_id: &'a str,
    token: &'a str,
}

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
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(flux_crypto::crypto_provider()))
        .with_safe_default_protocol_versions()?
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

pub async fn dial_out(addr: String, machine_id: String, token: String, shared: Arc<Shared>) {
    let server_name = match relay_server_name(&addr) {
        Ok(name) => name,
        Err(error) => {
            tracing::error!("invalid FLUX_RELAY_QUIC_ADDR {addr:?}: {error}");
            return;
        }
    };
    let client_config = match make_client_config() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!("cannot configure relay QUIC trust roots: {error}");
            return;
        }
    };

    let mut endpoint: Option<(bool, quinn::Endpoint)> = None;
    let mut failure_backoff = Duration::from_secs(1);
    loop {
        let resolved = match tokio::net::lookup_host(&addr).await {
            Ok(addresses) => addresses.collect::<Vec<_>>(),
            Err(error) => {
                tracing::warn!("resolve relay {addr}: {error}; retrying in {failure_backoff:?}");
                tokio::time::sleep(failure_backoff).await;
                failure_backoff = (failure_backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        let Some(remote) = resolved.first().copied() else {
            tracing::warn!("relay {addr} resolved to no addresses; retrying in {failure_backoff:?}");
            tokio::time::sleep(failure_backoff).await;
            failure_backoff = (failure_backoff * 2).min(Duration::from_secs(30));
            continue;
        };
        let ipv6 = remote.is_ipv6();
        if endpoint.as_ref().is_none_or(|(family, _)| *family != ipv6) {
            let bind = if ipv6 {
                SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
            } else {
                SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
            };
            match make_client_endpoint(bind, client_config.clone()) {
                Ok(client) => endpoint = Some((ipv6, client)),
                Err(error) => {
                    tracing::warn!("create relay QUIC endpoint: {error}; retrying in {failure_backoff:?}");
                    tokio::time::sleep(failure_backoff).await;
                    failure_backoff = (failure_backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            }
        }
        let client = &endpoint.as_ref().expect("endpoint initialized").1;
        match connect_link(client, remote, &server_name, &machine_id, &token).await {
            Ok(conn) => {
                tracing::info!("host link parked at relay {addr} for machine {machine_id}");
                if let Err(error) = bridge(conn, shared.clone()).await {
                    tracing::info!("host link to relay {addr} ended: {error}");
                }
                failure_backoff = Duration::from_secs(1);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(error) => {
                tracing::warn!("host link to relay {addr} failed: {error}; retrying in {failure_backoff:?}");
                tokio::time::sleep(failure_backoff).await;
                failure_backoff = (failure_backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn relay_server_name(addr: &str) -> Result<String, LinkError> {
    if let Ok(socket) = addr.parse::<SocketAddr>() {
        return Ok(socket.ip().to_string());
    }
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "expected host:port"))?;
    let host = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    if host.is_empty() || port.parse::<u16>().is_err() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "expected host:port").into());
    }
    Ok(host.to_owned())
}

fn make_client_config() -> Result<quinn::ClientConfig, LinkError> {
    let mut roots = rustls::RootCertStore::empty();
    if let Some(path) = std::env::var("FLUX_RELAY_CA_FILE")
        .ok()
        .filter(|value| !value.is_empty())
    {
        let mut reader = BufReader::new(std::fs::File::open(path)?);
        let certs = rustls_pemfile::certs(&mut reader).collect::<Result<Vec<_>, _>>()?;
        if certs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "CA file contains no certificates").into());
        }
        for cert in certs {
            roots.add(cert)?;
        }
    } else {
        let result = rustls_native_certs::load_native_certs();
        if !result.errors.is_empty() {
            tracing::warn!("errors loading system CA certificates: {:?}", result.errors);
        }
        if result.certs.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "system CA store is empty").into());
        }
        for cert in result.certs {
            roots.add(cert)?;
        }
    }
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(flux_crypto::crypto_provider()))
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(15).try_into()?));
    transport.keep_alive_interval(Some(Duration::from_secs(3)));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

fn make_client_endpoint(bind: SocketAddr, config: quinn::ClientConfig) -> Result<quinn::Endpoint, LinkError> {
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(config);
    Ok(endpoint)
}

fn encode_link_hello(machine_id: &str, token: &str) -> Result<Vec<u8>, LinkError> {
    let payload = serde_json::to_vec(&LinkHello { machine_id, token })?;
    if payload.is_empty() || payload.len() > LINK_HELLO_MAX_SIZE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "link hello exceeds 4 KiB").into());
    }
    let length = u16::try_from(payload.len())?;
    let mut framed = Vec::with_capacity(2 + payload.len());
    framed.extend_from_slice(&length.to_be_bytes());
    framed.extend_from_slice(&payload);
    Ok(framed)
}

async fn connect_link(
    endpoint: &quinn::Endpoint,
    addr: SocketAddr,
    server_name: &str,
    machine_id: &str,
    token: &str,
) -> Result<quinn::Connection, LinkError> {
    let hello = encode_link_hello(machine_id, token)?;
    let conn = endpoint.connect(addr, server_name)?.await?;
    let result = async {
        let (mut send, mut recv) = conn.open_bi().await?;
        send.write_all(&hello).await?;
        send.finish()?;
        let mut ack = [0u8; 1];
        recv.read_exact(&mut ack).await?;
        if ack[0] != 0 {
            return Err(io::Error::other(format!(
                "relay rejected host link with acknowledgement 0x{:02x}",
                ack[0]
            ))
            .into());
        }
        Ok(())
    }
    .await;
    match result {
        Ok(()) => Ok(conn),
        Err(error) => {
            conn.close(quinn::VarInt::from_u32(0x1), b"invalid host-link hello");
            Err(error)
        }
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

#[cfg(test)]
mod dial_out_tests {
    use super::*;
    use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

    fn test_endpoints() -> (quinn::Endpoint, quinn::Endpoint, SocketAddr) {
        flux_crypto::install_crypto_provider();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
        let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(flux_crypto::crypto_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], key)
            .unwrap();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).unwrap(),
        ));
        let transport = Arc::get_mut(&mut server_config.transport).unwrap();
        transport.max_idle_timeout(Some(Duration::from_secs(15).try_into().unwrap()));
        transport.keep_alive_interval(Some(Duration::from_secs(3)));
        let server = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse::<SocketAddr>().unwrap()).unwrap();
        let addr = server.local_addr().unwrap();

        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let mut client_tls = rustls::ClientConfig::builder_with_provider(Arc::new(flux_crypto::crypto_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).unwrap();
        let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(Duration::from_secs(15).try_into().unwrap()));
        transport.keep_alive_interval(Some(Duration::from_secs(3)));
        client_config.transport_config(Arc::new(transport));
        let client = make_client_endpoint("127.0.0.1:0".parse().unwrap(), client_config).unwrap();
        (server, client, addr)
    }

    async fn receive_link_hello(conn: quinn::Connection, acknowledge: bool) -> (Vec<u8>, quinn::Connection) {
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let mut length = [0u8; 2];
        recv.read_exact(&mut length).await.unwrap();
        let mut payload = vec![0; u16::from_be_bytes(length) as usize];
        recv.read_exact(&mut payload).await.unwrap();
        if acknowledge {
            send.write_all(&[0]).await.unwrap();
            send.finish().unwrap();
        } else {
            conn.close(quinn::VarInt::from_u32(0x1), b"unauthorized");
        }
        (payload, conn)
    }

    #[tokio::test]
    async fn connect_link_sends_hello_and_accepts_ack() {
        let (server, client, addr) = test_endpoints();
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            let conn = incoming.await.unwrap();
            receive_link_hello(conn, true).await
        });
        let conn = connect_link(&client, addr, "localhost", "machine-1", "secret")
            .await
            .unwrap();
        let (payload, server_conn) = server_task.await.unwrap();
        assert_eq!(payload, br#"{"machine_id":"machine-1","token":"secret"}"#);
        conn.close(quinn::VarInt::from_u32(0), b"test done");
        drop(server_conn);
    }

    #[tokio::test]
    async fn connect_link_rejects_relay_application_error() {
        let (server, client, addr) = test_endpoints();
        let server_task = tokio::spawn(async move {
            let incoming = server.accept().await.unwrap();
            let conn = incoming.await.unwrap();
            receive_link_hello(conn, false).await
        });
        let result = connect_link(&client, addr, "localhost", "machine-1", "secret").await;
        assert!(result.is_err(), "link handshake unexpectedly succeeded");
        let (payload, _server_conn) = server_task.await.unwrap();
        assert_eq!(payload, br#"{"machine_id":"machine-1","token":"secret"}"#);
    }
}
