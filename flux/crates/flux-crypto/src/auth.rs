//! PIN-based client authentication.
//!
//! Implements a pairing flow where new clients must enter a PIN displayed
//! on the host to establish trust. Once paired, clients are remembered via
//! their certificate fingerprint.

use std::collections::HashMap;

use flux_core::error::{FluxError, Result};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Manages PIN-based pairing and client trust.
pub struct PinAuthenticator {
    /// Currently active PIN (regenerated for each pairing attempt).
    active_pin: Option<String>,

    /// Paired clients keyed by certificate SHA-256 fingerprint.
    paired_clients: HashMap<String, PairedClient>,
}

/// A client that has successfully paired with this host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedClient {
    /// Human-readable client name.
    pub name: String,

    /// SHA-256 fingerprint of the client's TLS certificate.
    pub cert_fingerprint: String,

    /// When this client was first paired (UTC, RFC 3339).
    pub paired_at: String,
}

impl PinAuthenticator {
    pub fn new() -> Self {
        Self {
            active_pin: None,
            paired_clients: HashMap::new(),
        }
    }

    /// Load previously paired clients from a JSON file.
    pub fn load_paired_clients(&mut self, path: &std::path::Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let data = std::fs::read_to_string(path)?;
        self.paired_clients = serde_json::from_str(&data)
            .map_err(|e| FluxError::Config(format!("failed to parse paired clients: {e}")))?;
        tracing::info!("Loaded {} paired clients", self.paired_clients.len());
        Ok(())
    }

    /// Save paired clients to a JSON file.
    pub fn save_paired_clients(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(&self.paired_clients)
            .map_err(|e| FluxError::Config(format!("failed to serialize paired clients: {e}")))?;
        std::fs::write(path, data)?;
        Ok(())
    }

    /// Generate a new 4-digit PIN for pairing.
    pub fn generate_pin(&mut self) -> &str {
        let rng = SystemRandom::new();
        let mut bytes = [0u8; 2];
        rng.fill(&mut bytes).expect("RNG failure");
        let pin_num = u16::from_be_bytes(bytes) % 10000;
        let pin = format!("{:04}", pin_num);

        tracing::info!("Generated pairing PIN: {}", pin);
        self.active_pin = Some(pin);
        self.active_pin.as_ref().unwrap()
    }

    /// Verify a PIN submitted by a client. Returns `true` if it matches.
    pub fn verify_pin(&mut self, submitted_pin: &str) -> bool {
        match &self.active_pin {
            Some(pin) if pin == submitted_pin => {
                tracing::info!("PIN verification successful");
                self.active_pin = None; // Invalidate after use
                true
            }
            Some(_) => {
                tracing::warn!("PIN verification failed: incorrect PIN");
                false
            }
            None => {
                tracing::warn!("PIN verification failed: no active PIN");
                false
            }
        }
    }

    /// Register a successfully paired client.
    pub fn add_paired_client(&mut self, name: String, cert_fingerprint: String) {
        tracing::info!("Pairing client '{}' (fingerprint: {})", name, cert_fingerprint);
        self.paired_clients.insert(
            cert_fingerprint.clone(),
            PairedClient {
                name,
                cert_fingerprint,
                paired_at: utc_now_rfc3339(),
            },
        );
    }

    /// Check if a client certificate fingerprint is already paired.
    pub fn is_paired(&self, cert_fingerprint: &str) -> bool {
        self.paired_clients.contains_key(cert_fingerprint)
    }

    /// Remove a paired client.
    pub fn remove_paired_client(&mut self, cert_fingerprint: &str) -> bool {
        self.paired_clients.remove(cert_fingerprint).is_some()
    }

    /// List all paired clients.
    pub fn paired_clients(&self) -> impl Iterator<Item = &PairedClient> {
        self.paired_clients.values()
    }
}

/// Compute the SHA-256 fingerprint of a DER-encoded certificate.
pub fn cert_fingerprint(cert_der: &[u8]) -> String {
    use ring::digest;
    let hash = digest::digest(&digest::SHA256, cert_der);
    hash.as_ref()
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(":")
}

fn utc_now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC OffsetDateTime is always RFC 3339 formattable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_generation_and_verification() {
        let mut auth = PinAuthenticator::new();
        let pin = auth.generate_pin().to_string();
        assert_eq!(pin.len(), 4);
        assert!(auth.verify_pin(&pin));
        // PIN is consumed after successful verification
        assert!(!auth.verify_pin(&pin));
    }

    #[test]
    fn wrong_pin_rejected() {
        let mut auth = PinAuthenticator::new();
        let _pin = auth.generate_pin().to_string();
        assert!(!auth.verify_pin("9999"));
    }

    #[test]
    fn paired_client_management() {
        let mut auth = PinAuthenticator::new();
        let fp = "AA:BB:CC:DD".to_string();

        assert!(!auth.is_paired(&fp));
        auth.add_paired_client("Test Client".into(), fp.clone());
        assert!(auth.is_paired(&fp));
        assert!(auth.remove_paired_client(&fp));
        assert!(!auth.is_paired(&fp));
    }

    #[test]
    fn paired_at_is_current_utc_rfc3339() {
        let mut auth = PinAuthenticator::new();
        let fp = "AA:BB:CC:DD".to_string();

        let before = OffsetDateTime::now_utc();
        auth.add_paired_client("Test Client".into(), fp.clone());
        let after = OffsetDateTime::now_utc();

        let client = auth.paired_clients().find(|c| c.cert_fingerprint == fp).unwrap();
        let paired_at = OffsetDateTime::parse(&client.paired_at, &Rfc3339).unwrap();
        assert!(client.paired_at.ends_with('Z'));
        assert!(before <= paired_at && paired_at <= after);
    }
}
