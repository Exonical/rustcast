//! AES-GCM authenticated encryption for stream data.
//!
//! Used to encrypt control messages and input events in transit.
//! Video/audio RTP packets may optionally be encrypted depending on the
//! security configuration.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm, Nonce};

use flux_core::error::{FluxError, Result};

/// AES-GCM cipher for encrypting/decrypting stream data.
pub struct AesGcmCipher {
    inner: CipherInner,
    nonce_counter: u64,
}

enum CipherInner {
    Aes128(Aes128Gcm),
    Aes256(Aes256Gcm),
}

/// Key size selection.
#[derive(Debug, Clone, Copy)]
pub enum KeySize {
    Aes128,
    Aes256,
}

impl AesGcmCipher {
    /// Create a new AES-GCM cipher from a key.
    ///
    /// Key must be exactly 16 bytes (AES-128) or 32 bytes (AES-256).
    pub fn new(key: &[u8]) -> Result<Self> {
        let inner = match key.len() {
            16 => {
                let cipher = Aes128Gcm::new_from_slice(key)
                    .map_err(|e| FluxError::Crypto(format!("AES-128 key error: {e}")))?;
                CipherInner::Aes128(cipher)
            }
            32 => {
                let cipher = Aes256Gcm::new_from_slice(key)
                    .map_err(|e| FluxError::Crypto(format!("AES-256 key error: {e}")))?;
                CipherInner::Aes256(cipher)
            }
            n => {
                return Err(FluxError::Crypto(format!(
                    "invalid key length {n}: expected 16 or 32 bytes"
                )));
            }
        };

        Ok(Self {
            inner,
            nonce_counter: 0,
        })
    }

    /// Encrypt plaintext with an auto-incrementing nonce.
    ///
    /// Returns `(nonce_bytes, ciphertext)`.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
        let nonce_val = self.nonce_counter;
        self.nonce_counter += 1;

        let mut nonce_bytes = [0u8; 12];
        nonce_bytes[4..12].copy_from_slice(&nonce_val.to_be_bytes());
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = match &self.inner {
            CipherInner::Aes128(cipher) => cipher
                .encrypt(nonce, plaintext)
                .map_err(|e| FluxError::Crypto(format!("AES-128 encrypt error: {e}")))?,
            CipherInner::Aes256(cipher) => cipher
                .encrypt(nonce, plaintext)
                .map_err(|e| FluxError::Crypto(format!("AES-256 encrypt error: {e}")))?,
        };

        Ok((nonce_bytes.to_vec(), ciphertext))
    }

    /// Encrypt with an explicit nonce (12 bytes).
    pub fn encrypt_with_nonce(&self, nonce: &[u8; 12], plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::from_slice(nonce);
        match &self.inner {
            CipherInner::Aes128(cipher) => cipher
                .encrypt(nonce, plaintext)
                .map_err(|e| FluxError::Crypto(format!("encrypt error: {e}"))),
            CipherInner::Aes256(cipher) => cipher
                .encrypt(nonce, plaintext)
                .map_err(|e| FluxError::Crypto(format!("encrypt error: {e}"))),
        }
    }

    /// Decrypt ciphertext with the given nonce (12 bytes).
    pub fn decrypt(&self, nonce: &[u8; 12], ciphertext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::from_slice(nonce);
        match &self.inner {
            CipherInner::Aes128(cipher) => cipher
                .decrypt(nonce, ciphertext)
                .map_err(|e| FluxError::Crypto(format!("decrypt error: {e}"))),
            CipherInner::Aes256(cipher) => cipher
                .decrypt(nonce, ciphertext)
                .map_err(|e| FluxError::Crypto(format!("decrypt error: {e}"))),
        }
    }
}

/// Generate a cryptographically secure random key.
pub fn generate_key(size: KeySize) -> Vec<u8> {
    use ring::rand::{SecureRandom, SystemRandom};
    let rng = SystemRandom::new();
    let len = match size {
        KeySize::Aes128 => 16,
        KeySize::Aes256 => 32,
    };
    let mut key = vec![0u8; len];
    rng.fill(&mut key).expect("RNG failure");
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_round_trip_128() {
        let key = generate_key(KeySize::Aes128);
        let mut cipher = AesGcmCipher::new(&key).unwrap();
        let plaintext = b"hello flux streaming";

        let (nonce, ciphertext) = cipher.encrypt(plaintext).unwrap();
        assert_ne!(ciphertext, plaintext);

        let nonce: [u8; 12] = nonce.try_into().unwrap();
        let decrypted = cipher.decrypt(&nonce, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_decrypt_round_trip_256() {
        let key = generate_key(KeySize::Aes256);
        let mut cipher = AesGcmCipher::new(&key).unwrap();
        let plaintext = b"GPU-accelerated remote desktop";

        let (nonce, ciphertext) = cipher.encrypt(plaintext).unwrap();
        let nonce: [u8; 12] = nonce.try_into().unwrap();
        let decrypted = cipher.decrypt(&nonce, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = generate_key(KeySize::Aes128);
        let key2 = generate_key(KeySize::Aes128);
        let mut cipher1 = AesGcmCipher::new(&key1).unwrap();
        let cipher2 = AesGcmCipher::new(&key2).unwrap();

        let (nonce, ciphertext) = cipher1.encrypt(b"secret").unwrap();
        let nonce: [u8; 12] = nonce.try_into().unwrap();
        assert!(cipher2.decrypt(&nonce, &ciphertext).is_err());
    }
}
