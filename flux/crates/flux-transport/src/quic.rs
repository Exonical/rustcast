//! QUIC transport layer.
//!
//! Provides a reliable, multiplexed transport built on QUIC (via the `quinn`
//! crate). Used for:
//!   - Session signaling and control messages (reliable, ordered)
//!   - Input event delivery (reliable, ordered)
//!   - Optional video/audio tunneling through QUIC datagrams (unreliable)
//!
//! QUIC offers advantages over raw UDP + TCP:
//!   - Built-in TLS 1.3 encryption
//!   - 0-RTT connection establishment
//!   - Multiplexed streams without head-of-line blocking
//!   - Unreliable datagrams (RFC 9221) for media
//!
//! Status: placeholder. No QUIC I/O is implemented yet; every constructor and
//! I/O method returns a "not yet implemented" [`FluxError::Network`] rather
//! than reporting success. The live host stream uses `quinn` directly in
//! `flux-server/src/quic_frames.rs`.

use std::net::SocketAddr;

use flux_core::error::{FluxError, Result};

fn not_implemented(operation: &str) -> FluxError {
    FluxError::Network(format!("QUIC {operation} not yet implemented"))
}

/// Configuration for the QUIC transport.
#[derive(Debug, Clone)]
pub struct QuicConfig {
    /// Local address to bind to.
    pub bind_addr: SocketAddr,

    /// Maximum idle timeout before the connection is closed.
    pub idle_timeout_secs: u64,

    /// Enable QUIC datagrams (RFC 9221) for media transport.
    pub enable_datagrams: bool,

    /// Maximum datagram size (for media packets).
    pub max_datagram_size: usize,

    /// Keep-alive interval in seconds.
    pub keep_alive_secs: u64,
}

impl Default for QuicConfig {
    fn default() -> Self {
        Self {
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            idle_timeout_secs: 30,
            enable_datagrams: true,
            max_datagram_size: 1350,
            keep_alive_secs: 5,
        }
    }
}

/// QUIC server that accepts incoming connections from remote clients.
pub struct QuicServer {
    // TODO: quinn::Endpoint
    _config: QuicConfig,
}

impl QuicServer {
    /// Create and bind a QUIC server endpoint.
    pub async fn bind(
        config: QuicConfig,
        _cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>,
        _private_key: rustls::pki_types::PrivateKeyDer<'static>,
    ) -> Result<Self> {
        // TODO: Full quinn server setup:
        //
        //   1. Build rustls ServerConfig:
        //      let mut tls_config = rustls::ServerConfig::builder()
        //          .with_no_client_auth()
        //          .with_single_cert(cert_chain, private_key)?;
        //      tls_config.alpn_protocols = vec![b"flux/1".to_vec()];
        //
        //   2. Build quinn ServerConfig:
        //      let mut transport = quinn::TransportConfig::default();
        //      transport.max_idle_timeout(Some(Duration::from_secs(config.idle_timeout_secs).try_into()?));
        //      transport.keep_alive_interval(Some(Duration::from_secs(config.keep_alive_secs)));
        //      if config.enable_datagrams {
        //          transport.datagram_receive_buffer_size(Some(config.max_datagram_size * 64));
        //      }
        //
        //      let server_config = quinn::ServerConfig::with_crypto(Arc::new(tls_config));
        //      server_config.transport_config(Arc::new(transport));
        //
        //   3. Bind endpoint:
        //      let endpoint = quinn::Endpoint::server(server_config, config.bind_addr)?;

        Err(not_implemented(&format!("server bind on {}", config.bind_addr)))
    }

    /// Accept the next incoming QUIC connection.
    pub async fn accept(&self) -> Result<QuicConnection> {
        // TODO: endpoint.accept().await → connecting.await → connection
        Err(not_implemented("accept"))
    }
}

/// QUIC client that connects to a remote server.
pub struct QuicClient {
    _config: QuicConfig,
}

impl QuicClient {
    /// Create a QUIC client endpoint.
    pub async fn new(config: QuicConfig) -> Result<Self> {
        // TODO: quinn client endpoint setup:
        //
        //   let mut tls_config = rustls::ClientConfig::builder()
        //       .dangerous()
        //       .with_custom_certificate_verifier(Arc::new(FluxCertVerifier))
        //       .with_no_client_auth();
        //   tls_config.alpn_protocols = vec![b"flux/1".to_vec()];
        //
        //   let client_config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls_config)?));
        //   let mut endpoint = quinn::Endpoint::client(config.bind_addr)?;
        //   endpoint.set_default_client_config(client_config);

        Err(not_implemented(&format!("client endpoint on {}", config.bind_addr)))
    }

    /// Connect to a remote Flux server.
    pub async fn connect(&self, server_addr: SocketAddr, server_name: &str) -> Result<QuicConnection> {
        // TODO: endpoint.connect(server_addr, server_name)?.await
        Err(not_implemented(&format!("connect to {server_addr} ({server_name})")))
    }
}

/// An established QUIC connection with multiplexed streams.
pub struct QuicConnection {
    // TODO: quinn::Connection
    _private: (),
}

impl QuicConnection {
    /// Open a new bidirectional stream for reliable ordered data.
    pub async fn open_stream(&self) -> Result<QuicStream> {
        // TODO: connection.open_bi().await
        Err(not_implemented("open_stream"))
    }

    /// Accept an incoming bidirectional stream from the peer.
    pub async fn accept_stream(&self) -> Result<QuicStream> {
        // TODO: connection.accept_bi().await
        Err(not_implemented("accept_stream"))
    }

    /// Send an unreliable datagram (for media packets).
    pub fn send_datagram(&self, _data: &[u8]) -> Result<()> {
        // TODO: connection.send_datagram(Bytes::copy_from_slice(data))
        Err(not_implemented("send_datagram"))
    }

    /// Receive an unreliable datagram from the peer.
    pub async fn recv_datagram(&self) -> Result<Vec<u8>> {
        // TODO: connection.read_datagram().await
        Err(not_implemented("recv_datagram"))
    }

    /// Get the remote address of the peer.
    pub fn remote_addr(&self) -> SocketAddr {
        // TODO: connection.remote_address()
        "0.0.0.0:0".parse().unwrap()
    }

    /// Close the connection gracefully.
    pub fn close(&self, reason: &str) {
        tracing::info!("Closing QUIC connection: {}", reason);
        // TODO: connection.close(0u32.into(), reason.as_bytes())
    }
}

/// A bidirectional QUIC stream for reliable ordered data.
pub struct QuicStream {
    // TODO: (quinn::SendStream, quinn::RecvStream)
    _private: (),
}

impl QuicStream {
    /// Write data to the stream.
    pub async fn write(&mut self, _data: &[u8]) -> Result<()> {
        // TODO: send_stream.write_all(data).await
        Err(not_implemented("stream write"))
    }

    /// Read data from the stream.
    pub async fn read(&mut self, _buf: &mut [u8]) -> Result<usize> {
        // TODO: recv_stream.read(buf).await
        Err(not_implemented("stream read"))
    }

    /// Gracefully finish the send side.
    pub async fn finish(&mut self) -> Result<()> {
        // TODO: send_stream.finish()
        Err(not_implemented("stream finish"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_not_implemented<T>(result: Result<T>) {
        match result {
            Err(FluxError::Network(msg)) => assert!(msg.contains("not yet implemented"), "{msg}"),
            Err(other) => panic!("unexpected error: {other}"),
            Ok(_) => panic!("placeholder QUIC API reported success"),
        }
    }

    #[tokio::test]
    async fn placeholder_endpoints_fail_instead_of_succeeding() {
        let cert = rustls::pki_types::CertificateDer::from(vec![0u8]);
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(vec![0u8].into());
        assert_not_implemented(QuicServer::bind(QuicConfig::default(), vec![cert], key).await);
        assert_not_implemented(QuicClient::new(QuicConfig::default()).await);
    }

    #[tokio::test]
    async fn placeholder_io_fails_instead_of_succeeding() {
        let conn = QuicConnection { _private: () };
        assert_not_implemented(conn.send_datagram(b"frame"));
        assert_not_implemented(conn.recv_datagram().await);

        let mut stream = QuicStream { _private: () };
        assert_not_implemented(stream.write(b"data").await);
        assert_not_implemented(stream.read(&mut [0u8; 16]).await);
        assert_not_implemented(stream.finish().await);
    }
}
