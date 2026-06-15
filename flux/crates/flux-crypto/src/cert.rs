//! TLS certificate generation and management.
//!
//! Generates self-signed certificates for host identity and mutual TLS.
//! Certificates are persisted to disk and reused across restarts.

use std::path::Path;

use flux_core::error::{FluxError, Result};
use rcgen::{CertificateParams, KeyPair};

/// Manages TLS certificates for the Flux host.
pub struct CertificateManager {
    cert_pem: String,
    key_pem: String,
}

impl CertificateManager {
    /// Load existing certificates from disk, or generate new ones if they don't exist.
    pub fn load_or_create(cert_path: &Path, key_path: &Path) -> Result<Self> {
        if cert_path.exists() && key_path.exists() {
            tracing::info!("Loading existing certificates from {}", cert_path.display());
            let cert_pem = std::fs::read_to_string(cert_path)?;
            let key_pem = std::fs::read_to_string(key_path)?;
            Ok(Self { cert_pem, key_pem })
        } else {
            tracing::info!("Generating new self-signed certificate");
            let manager = Self::generate()?;
            manager.save(cert_path, key_path)?;
            Ok(manager)
        }
    }

    /// Generate a new self-signed certificate.
    pub fn generate() -> Result<Self> {
        let mut params = CertificateParams::new(vec!["flux-host".into()])
            .map_err(|e| FluxError::Crypto(format!("cert params error: {e}")))?;

        params.distinguished_name.push(
            rcgen::DnType::CommonName,
            rcgen::DnValue::Utf8String("Flux Remote Host".into()),
        );
        params.distinguished_name.push(
            rcgen::DnType::OrganizationName,
            rcgen::DnValue::Utf8String("Flux".into()),
        );

        // Generate a new key pair.
        let key_pair = KeyPair::generate()
            .map_err(|e| FluxError::Crypto(format!("key generation failed: {e}")))?;

        let cert = params
            .self_signed(&key_pair)
            .map_err(|e| FluxError::Crypto(format!("self-sign failed: {e}")))?;

        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();

        tracing::info!("Generated new self-signed certificate");

        Ok(Self { cert_pem, key_pem })
    }

    /// Save certificates to disk.
    pub fn save(&self, cert_path: &Path, key_path: &Path) -> Result<()> {
        if let Some(parent) = cert_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Some(parent) = key_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        std::fs::write(cert_path, &self.cert_pem)?;
        std::fs::write(key_path, &self.key_pem)?;

        tracing::debug!("Saved certificate to {}", cert_path.display());
        tracing::debug!("Saved private key to {}", key_path.display());
        Ok(())
    }

    /// Get the certificate PEM string.
    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Get the private key PEM string.
    pub fn key_pem(&self) -> &str {
        &self.key_pem
    }

    /// Parse the certificate chain for use with rustls.
    pub fn rustls_cert_chain(&self) -> Result<Vec<rustls::pki_types::CertificateDer<'static>>> {
        let mut reader = std::io::BufReader::new(self.cert_pem.as_bytes());
        let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| FluxError::Crypto(format!("PEM parse error: {e}")))?;
        Ok(certs)
    }

    /// Parse the private key for use with rustls.
    pub fn rustls_private_key(&self) -> Result<rustls::pki_types::PrivateKeyDer<'static>> {
        let mut reader = std::io::BufReader::new(self.key_pem.as_bytes());
        let key = rustls_pemfile::private_key(&mut reader)
            .map_err(|e| FluxError::Crypto(format!("key parse error: {e}")))?
            .ok_or_else(|| FluxError::Crypto("no private key found in PEM".into()))?;
        Ok(key)
    }
}
