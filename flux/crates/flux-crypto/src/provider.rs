use rustls::crypto::{CryptoProvider, aws_lc_rs::kx_group};

pub fn crypto_provider() -> CryptoProvider {
    let mut provider = rustls::crypto::aws_lc_rs::default_provider();
    provider.kx_groups = vec![
        kx_group::MLKEM1024,
        kx_group::SECP256R1MLKEM768,
        kx_group::X25519MLKEM768,
        kx_group::SECP384R1,
        kx_group::SECP256R1,
        kx_group::X25519,
    ];
    provider
}

pub fn install_crypto_provider() {
    let _ = crypto_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::crypto_provider;
    use rustls::{
        ClientConfig, ClientConnection, NamedGroup, RootCertStore, ServerConfig, ServerConnection,
        crypto::{CryptoProvider, aws_lc_rs::kx_group},
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer, ServerName},
    };
    use std::{io::Cursor, sync::Arc};

    fn negotiate(client_provider: CryptoProvider) -> NamedGroup {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_der = cert.cert.der().clone();
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));

        let server_config = ServerConfig::builder_with_provider(Arc::new(crypto_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], private_key)
            .unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert_der).unwrap();
        let client_config = ClientConfig::builder_with_provider(Arc::new(client_provider))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();

        let mut client =
            ClientConnection::new(Arc::new(client_config), ServerName::try_from("localhost").unwrap()).unwrap();
        let mut server = ServerConnection::new(Arc::new(server_config)).unwrap();

        for _ in 0..8 {
            let mut client_records = Vec::new();
            client.write_tls(&mut client_records).unwrap();
            if !client_records.is_empty() {
                server.read_tls(&mut Cursor::new(client_records)).unwrap();
                server.process_new_packets().unwrap();
            }

            let mut server_records = Vec::new();
            server.write_tls(&mut server_records).unwrap();
            if !server_records.is_empty() {
                client.read_tls(&mut Cursor::new(server_records)).unwrap();
                client.process_new_packets().unwrap();
            }

            if !client.is_handshaking() && !server.is_handshaking() {
                break;
            }
        }

        assert!(!client.is_handshaking());
        assert!(!server.is_handshaking());
        client.negotiated_key_exchange_group().unwrap().name()
    }

    #[test]
    fn prefers_mlkem1024_and_accepts_x25519_only_clients() {
        let provider = crypto_provider();
        assert_eq!(
            provider.kx_groups.iter().map(|group| group.name()).collect::<Vec<_>>(),
            vec![
                NamedGroup::MLKEM1024,
                NamedGroup::secp256r1MLKEM768,
                NamedGroup::X25519MLKEM768,
                NamedGroup::secp384r1,
                NamedGroup::secp256r1,
                NamedGroup::X25519,
            ]
        );
        assert_eq!(negotiate(provider), NamedGroup::MLKEM1024);

        let mut x25519_only = crypto_provider();
        x25519_only.kx_groups = vec![kx_group::X25519];
        assert_eq!(negotiate(x25519_only), NamedGroup::X25519);
    }
}
